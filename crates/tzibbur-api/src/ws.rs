//! WebSocket protocol (`wss://api.tzibbur.me/v1/ws`, protocol version 1), as
//! specified by the official API.
//!
//! ```text
//! Idle → Connecting → Connected ⟲ BackingOff
//!                  ↘ UpdateRequired
//! ```
//!
//! The socket is the push transport: after `hello` the server pushes every
//! undelivered batch on its own, and new messages arrive as they are sent. There
//! is nothing to poll. The server keeps at most one un-acked batch in flight per
//! group, so every `messages` frame must be answered with an `ack` frame for its
//! last seq (the sync engine does this).
//!
//! * Bearer token is sent as an `Authorization` header at handshake.
//! * A 401 or 403 (`device_blocked`) at handshake, or close code 4001 (session
//!   revoked), invalidates the session: no reconnect.
//! * A 429 at handshake waits for `Retry-After` before the next attempt.
//! * Close code 4029 ("too many connections") waits at least
//!   [`TOO_MANY_CONNECTIONS_WAIT`] before trying again.
//! * Every other close or drop reconnects with jittered exponential backoff.
//! * The client sends `ping` after `hello.limits.heartbeatSeconds` of outbound
//!   silence (default [`PING_AFTER_OUTBOUND_SILENCE`]) and answers server pings.
//! * A `hello` with a newer protocol version, or an `error` frame whose code
//!   indicates a version mismatch, moves the socket to
//!   [`SyncState::UpdateRequired`], which is sticky across `stop()`/`start()`.

use crate::backoff::backoff_delay;
use crate::constants::{
    PING_AFTER_OUTBOUND_SILENCE, WS_CLOSE_SESSION_REVOKED, WS_CLOSE_TOO_MANY_CONNECTIONS,
    WS_EVENT_BUFFER, WS_PROTOCOL_VERSION,
};
use crate::error::{AppError, Result};
use crate::http::TzibburClient;
use crate::models::{GroupSettings, MemberDto, MessageDto, Permission, Role};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Minimum wait before reconnecting after close code 4029 (too many connections).
pub const TOO_MANY_CONNECTIONS_WAIT: Duration = Duration::from_secs(60);

/// Client → server frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientFrame {
    /// Liveness probe; answered with `pong`.
    Ping,
    /// Delivery acknowledgement: this device stored everything in `group_id` up to
    /// `seq`. The only thing that advances the delivery cursor.
    #[serde(rename_all = "camelCase")]
    Ack { group_id: String, seq: i64 },
    /// The user has read `group_id` up to `seq` (per user; moves the badge on their
    /// other devices). Not a delivery ack.
    #[serde(rename_all = "camelCase")]
    Read { group_id: String, seq: i64 },
}

/// Server → client frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerFrame {
    /// Capability handshake sent on connect. Live shape:
    /// `{"type":"hello","protocolVersion":1,"userId":"…","deviceId":"…",
    ///   "limits":{"heartbeatSeconds":30,"maxConnectionsPerDevice":3,"maxFrameBytes":16384}}`
    #[serde(rename_all = "camelCase")]
    Hello {
        #[serde(default, alias = "version")]
        protocol_version: u32,
        #[serde(default)]
        user_id: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
        #[serde(default)]
        limits: HelloLimits,
    },
    Pong,
    /// New messages pushed in real time.
    #[serde(rename_all = "camelCase")]
    Messages {
        group_id: String,
        #[serde(default)]
        messages: Vec<MessageDto>,
        /// More messages are available for this group via REST paging.
        #[serde(default)]
        has_more: bool,
    },
    /// Group lifecycle event. The payload fields sit directly on the frame beside
    /// `type` and `event`; a nested `payload` object is accepted as well.
    Group {
        event: String,
        #[serde(flatten)]
        fields: Map<String, Value>,
    },
    /// The user's read position moved on another of their devices.
    #[serde(rename_all = "camelCase")]
    Read {
        group_id: String,
        read_seq: i64,
    },
    /// Protocol-level error (e.g. version mismatch).
    Error {
        code: String,
        #[serde(default)]
        detail: Option<String>,
    },
}

/// Connection limits announced in the `hello` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HelloLimits {
    #[serde(default)]
    pub heartbeat_seconds: Option<u32>,
    #[serde(default)]
    pub max_connections_per_device: Option<u32>,
    #[serde(default)]
    pub max_frame_bytes: Option<u32>,
}

/// Typed view of a `group` frame.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupEvent {
    /// Someone joined. The official frame carries `userId` and `role`; `member` is
    /// only present on servers that embed the full member object.
    MemberAdded {
        group_id: String,
        user_id: String,
        role: Option<Role>,
        member: Option<MemberDto>,
    },
    /// Someone left (`reason: "left"`) or was removed (`"removed"`).
    MemberRemoved {
        group_id: String,
        user_id: String,
        reason: Option<String>,
    },
    RoleChanged {
        group_id: String,
        user_id: String,
        role: Role,
    },
    GroupUpdated {
        group_id: String,
        name: Option<String>,
        who_can_post: Option<Permission>,
        who_can_add_members: Option<Permission>,
    },
    GroupDeleted {
        group_id: String,
    },
    /// Event kind this crate does not know about.
    Unknown {
        event: String,
        group_id: Option<String>,
        payload: Map<String, Value>,
    },
}

impl GroupEvent {
    /// Wire `event` values.
    pub const MEMBER_ADDED: &'static str = "member-added";
    pub const MEMBER_REMOVED: &'static str = "member-removed";
    pub const ROLE_CHANGED: &'static str = "role-changed";
    pub const GROUP_UPDATED: &'static str = "group-updated";
    pub const GROUP_DELETED: &'static str = "group-deleted";

    pub fn kind(&self) -> &str {
        match self {
            GroupEvent::MemberAdded { .. } => Self::MEMBER_ADDED,
            GroupEvent::MemberRemoved { .. } => Self::MEMBER_REMOVED,
            GroupEvent::RoleChanged { .. } => Self::ROLE_CHANGED,
            GroupEvent::GroupUpdated { .. } => Self::GROUP_UPDATED,
            GroupEvent::GroupDeleted { .. } => Self::GROUP_DELETED,
            GroupEvent::Unknown { event, .. } => event,
        }
    }

    pub fn group_id(&self) -> Option<&str> {
        match self {
            GroupEvent::MemberAdded { group_id, .. }
            | GroupEvent::MemberRemoved { group_id, .. }
            | GroupEvent::RoleChanged { group_id, .. }
            | GroupEvent::GroupUpdated { group_id, .. }
            | GroupEvent::GroupDeleted { group_id } => Some(group_id),
            GroupEvent::Unknown { group_id, .. } => group_id.as_deref(),
        }
    }

    /// Parse `event` + its fields from a `group` frame (or a pending-events entry).
    /// Fields nested under a `payload` object are merged in.
    pub fn parse(event: &str, mut payload: Map<String, Value>) -> Self {
        if let Some(Value::Object(inner)) = payload.remove("payload") {
            for (k, v) in inner {
                payload.entry(k).or_insert(v);
            }
        }
        let gid = payload
            .get("groupId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let str_field = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_owned);
        let unknown = |payload: Map<String, Value>| GroupEvent::Unknown {
            event: event.to_owned(),
            group_id: gid.clone(),
            payload,
        };
        match (event, gid.clone()) {
            (Self::MEMBER_ADDED, Some(group_id)) => {
                let member = payload
                    .get("member")
                    .cloned()
                    .and_then(|m| serde_json::from_value::<MemberDto>(m).ok());
                let user_id =
                    str_field("userId").or_else(|| member.as_ref().map(|m| m.user_id.clone()));
                let role = str_field("role")
                    .and_then(|r| Role::parse(&r))
                    .or_else(|| member.as_ref().map(|m| m.role));
                match user_id {
                    Some(user_id) => GroupEvent::MemberAdded {
                        group_id,
                        user_id,
                        role,
                        member,
                    },
                    None => unknown(payload),
                }
            }
            (Self::MEMBER_REMOVED, Some(group_id)) => match str_field("userId") {
                Some(user_id) => GroupEvent::MemberRemoved {
                    group_id,
                    user_id,
                    reason: str_field("reason"),
                },
                None => unknown(payload),
            },
            (Self::ROLE_CHANGED, Some(group_id)) => {
                match (
                    str_field("userId"),
                    str_field("role").and_then(|r| Role::parse(&r)),
                ) {
                    (Some(user_id), Some(role)) => GroupEvent::RoleChanged {
                        group_id,
                        user_id,
                        role,
                    },
                    _ => unknown(payload),
                }
            }
            (Self::GROUP_UPDATED, Some(group_id)) => {
                let settings: GroupSettings = payload
                    .get("settings")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                GroupEvent::GroupUpdated {
                    group_id,
                    name: str_field("name"),
                    who_can_post: settings
                        .who_can_post
                        .or_else(|| str_field("whoCanPost").map(Permission)),
                    who_can_add_members: settings
                        .who_can_add_members
                        .or_else(|| str_field("whoCanAddMembers").map(Permission)),
                }
            }
            (Self::GROUP_DELETED, Some(group_id)) => GroupEvent::GroupDeleted { group_id },
            _ => unknown(payload),
        }
    }
}

// ---------------------------------------------------------------------------
// Events & state
// ---------------------------------------------------------------------------

/// Why the socket went down.
#[derive(Debug, Clone, PartialEq)]
pub enum DisconnectReason {
    Closed {
        code: Option<u16>,
        message: Option<String>,
    },
    Error {
        cause: Option<String>,
    },
    /// Server demanded a client update; the socket will not reconnect.
    UpdateRequired,
    /// The token is dead (401 at handshake, close code 4001) or the device was
    /// blocked (403 `device_blocked`); the socket will not reconnect.
    SessionRevoked {
        blocked: bool,
    },
}

impl DisconnectReason {
    pub fn is_too_many_connections(&self) -> bool {
        matches!(self, DisconnectReason::Closed { code: Some(c), .. } if *c == WS_CLOSE_TOO_MANY_CONNECTIONS)
    }
}

/// Events emitted by [`TzibburSocket`] (the `SocketEvent` shared flow).
#[derive(Debug, Clone, PartialEq)]
pub enum SocketEvent {
    /// Server `hello` received (emitted just before [`SocketEvent::Connected`]).
    Hello {
        protocol_version: u32,
        user_id: Option<String>,
        device_id: Option<String>,
        limits: HelloLimits,
    },
    Connected,
    Disconnected {
        reason: DisconnectReason,
    },
    /// A batch of undelivered messages. Ack the last seq on the socket.
    Messages {
        group_id: String,
        messages: Vec<MessageDto>,
        has_more: bool,
    },
    GroupEvent(GroupEvent),
    /// The user's read position moved elsewhere (another device or a REST call).
    Read {
        group_id: String,
        read_seq: i64,
    },
}

/// Connection / sync state (the `SyncState` sealed class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SyncState {
    #[default]
    Idle,
    Connecting,
    Connected,
    BackingOff,
    UpdateRequired,
}

impl SyncState {
    pub fn is_connected(self) -> bool {
        self == SyncState::Connected
    }
}

fn is_update_required_code(code: &str) -> bool {
    let c = code.to_ascii_lowercase();
    c.contains("update")
        || c.contains("version")
        || c.contains("upgrade")
        || c.contains("unsupported-client")
}

// ---------------------------------------------------------------------------
// Socket
// ---------------------------------------------------------------------------

struct Shared {
    client: TzibburClient,
    url: Url,
    events: broadcast::Sender<SocketEvent>,
    state_tx: watch::Sender<SyncState>,
    outbound_tx: mpsc::Sender<ClientFrame>,
    /// Held by the running connection loop; frames queued while disconnected
    /// are delivered once a connection is up.
    outbound_rx: tokio::sync::Mutex<mpsc::Receiver<ClientFrame>>,
}

/// Auto-reconnecting WebSocket connection.
pub struct TzibburSocket {
    shared: Arc<Shared>,
    task: Mutex<Option<(JoinHandle<()>, watch::Sender<bool>)>>,
}

impl TzibburSocket {
    /// Build a socket that authenticates with `client`'s current token and
    /// connects to the `/v1/ws` endpoint derived from its base URL.
    pub fn new(client: TzibburClient) -> Result<Self> {
        let url = client.ws_url()?;
        Ok(Self::with_url(client, url))
    }

    pub fn with_url(client: TzibburClient, url: Url) -> Self {
        let (events, _) = broadcast::channel(WS_EVENT_BUFFER);
        let (state_tx, _) = watch::channel(SyncState::Idle);
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        Self {
            shared: Arc::new(Shared {
                client,
                url,
                events,
                state_tx,
                outbound_tx,
                outbound_rx: tokio::sync::Mutex::new(outbound_rx),
            }),
            task: Mutex::new(None),
        }
    }

    pub fn url(&self) -> &Url {
        &self.shared.url
    }

    /// Subscribe to socket events. Slow subscribers may observe `Lagged`.
    pub fn subscribe(&self) -> broadcast::Receiver<SocketEvent> {
        self.shared.events.subscribe()
    }

    pub fn state(&self) -> SyncState {
        *self.shared.state_tx.borrow()
    }

    pub fn watch_state(&self) -> watch::Receiver<SyncState> {
        self.shared.state_tx.subscribe()
    }

    /// Start the connection loop. No-op if already running. Does nothing if the
    /// server previously demanded an update (state stays `UpdateRequired`).
    pub fn start(&self) {
        let mut task = self.task.lock();
        if task.is_some() {
            return;
        }
        if self.state() == SyncState::UpdateRequired {
            tracing::warn!("socket start ignored: UpdateRequired");
            return;
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let shared = self.shared.clone();
        let handle = tokio::spawn(async move {
            run_loop(shared.clone(), stop_rx).await;
            let _ = shared.state_tx.send_if_modified(|s| {
                if *s != SyncState::UpdateRequired {
                    *s = SyncState::Idle;
                    true
                } else {
                    false
                }
            });
        });
        *task = Some((handle, stop_tx));
    }

    /// Stop the connection loop and close the socket. `UpdateRequired` is preserved.
    pub async fn stop(&self) {
        let taken = self.task.lock().take();
        if let Some((handle, stop_tx)) = taken {
            let _ = stop_tx.send(true);
            let _ = handle.await;
        }
        let _ = self.shared.state_tx.send_if_modified(|s| {
            if *s != SyncState::UpdateRequired {
                *s = SyncState::Idle;
                true
            } else {
                false
            }
        });
    }

    pub fn is_running(&self) -> bool {
        self.task.lock().is_some()
    }

    /// Queue an `ack` frame.
    pub async fn ack(&self, group_id: &str, seq: i64) -> Result<()> {
        self.send(ClientFrame::Ack {
            group_id: group_id.to_owned(),
            seq,
        })
        .await
    }

    /// Queue a `read` frame (the user read `group_id` up to `seq`).
    pub async fn read(&self, group_id: &str, seq: i64) -> Result<()> {
        self.send(ClientFrame::Read {
            group_id: group_id.to_owned(),
            seq,
        })
        .await
    }

    /// Drop the current connection and connect again (the server then redelivers
    /// every un-acked batch). No-op when the socket is not running.
    pub async fn reconnect(&self) {
        if self.is_running() {
            self.stop().await;
            self.start();
        }
    }

    /// Queue a `ping` frame.
    pub async fn ping(&self) -> Result<()> {
        self.send(ClientFrame::Ping).await
    }

    pub async fn send(&self, frame: ClientFrame) -> Result<()> {
        self.shared
            .outbound_tx
            .send(frame)
            .await
            .map_err(|_| AppError::WebSocket("socket outbound channel closed".into()))
    }
}

fn set_state(shared: &Shared, s: SyncState) {
    let _ = shared.state_tx.send_if_modified(|cur| {
        if *cur != s {
            *cur = s;
            true
        } else {
            false
        }
    });
}

fn emit(shared: &Shared, ev: SocketEvent) {
    let _ = shared.events.send(ev);
}

/// What the connection loop does after a connection ends.
enum After {
    /// Reconnect after the normal jittered backoff.
    Backoff,
    /// Reconnect, but wait at least this long first.
    WaitAtLeast(Duration),
    /// Do not reconnect.
    Stop,
}

fn retry_after(resp: &http::Response<Option<Vec<u8>>>) -> Option<Duration> {
    resp.headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

async fn run_loop(shared: Arc<Shared>, mut stop_rx: watch::Receiver<bool>) {
    let mut outbound_rx = shared.outbound_rx.lock().await;
    let mut attempt: u32 = 0;
    loop {
        if *stop_rx.borrow() {
            break;
        }
        set_state(&shared, SyncState::Connecting);

        let token = match shared.client.token().await {
            Some(t) => t,
            None => {
                tracing::warn!("socket: no token, stopping");
                break;
            }
        };
        let mut req = match shared.url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "socket: bad url");
                break;
            }
        };
        if let Ok(v) = format!("Bearer {token}").parse() {
            req.headers_mut().insert(http::header::AUTHORIZATION, v);
        }
        if let Ok(v) = shared.client.user_agent().parse() {
            req.headers_mut().insert(http::header::USER_AGENT, v);
        }

        let connect = tokio::select! {
            r = tokio_tungstenite::connect_async(req) => r,
            _ = stop_rx.changed() => break,
        };

        let ws = match connect {
            Ok((ws, _resp)) => ws,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let status = resp.status();
                if status == http::StatusCode::UNAUTHORIZED || status == http::StatusCode::FORBIDDEN
                {
                    let blocked = status == http::StatusCode::FORBIDDEN;
                    tracing::warn!(
                        status = status.as_u16(),
                        "socket: refused at handshake, invalidating session"
                    );
                    emit(
                        &shared,
                        SocketEvent::Disconnected {
                            reason: DisconnectReason::SessionRevoked { blocked },
                        },
                    );
                    shared.client.notify_session_invalidated();
                    break;
                }
                let wait = if status == http::StatusCode::TOO_MANY_REQUESTS {
                    retry_after(&resp)
                } else {
                    None
                };
                tracing::warn!(
                    status = status.as_u16(),
                    ?wait,
                    attempt,
                    "socket: handshake refused"
                );
                emit(
                    &shared,
                    SocketEvent::Disconnected {
                        reason: DisconnectReason::Error {
                            cause: Some(format!("HTTP {status}")),
                        },
                    },
                );
                if !backoff_wait(&shared, &mut stop_rx, &mut attempt, wait).await {
                    break;
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, attempt, "socket: connect failed");
                emit(
                    &shared,
                    SocketEvent::Disconnected {
                        reason: DisconnectReason::Error {
                            cause: Some(e.to_string()),
                        },
                    },
                );
                if !backoff_wait(&shared, &mut stop_rx, &mut attempt, None).await {
                    break;
                }
                continue;
            }
        };

        let (mut sink, mut stream) = ws.split();
        let mut last_outbound = Instant::now();
        let mut conn = ConnState {
            got_hello: false,
            heartbeat: PING_AFTER_OUTBOUND_SILENCE,
        };
        let reason: DisconnectReason;

        loop {
            let ping_at = last_outbound + conn.heartbeat;
            tokio::select! {
                _ = stop_rx.changed() => {
                    let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "client stop".into() }))).await;
                    reason = DisconnectReason::Closed { code: Some(1000), message: Some("client stop".into()) };
                    break;
                }
                frame = outbound_rx.recv() => {
                    match frame {
                        Some(f) => {
                            let txt = serde_json::to_string(&f).unwrap_or_default();
                            tracing::debug!(frame = %txt, "socket: -> outbound frame");
                            if let Err(e) = sink.send(Message::Text(txt.into())).await {
                                reason = DisconnectReason::Error { cause: Some(e.to_string()) };
                                break;
                            }
                            last_outbound = Instant::now();
                        }
                        None => {
                            reason = DisconnectReason::Error { cause: Some("outbound channel closed".into()) };
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(ping_at)) => {
                    let txt = serde_json::to_string(&ClientFrame::Ping).unwrap_or_default();
                    if let Err(e) = sink.send(Message::Text(txt.into())).await {
                        reason = DisconnectReason::Error { cause: Some(e.to_string()) };
                        break;
                    }
                    last_outbound = Instant::now();
                }
                msg = stream.next() => {
                    let frame = match msg {
                        None => { reason = DisconnectReason::Closed { code: None, message: None }; break; }
                        Some(Err(e)) => { reason = DisconnectReason::Error { cause: Some(e.to_string()) }; break; }
                        Some(Ok(Message::Close(cf))) => {
                            let (code, message) = cf.map(|c| (Some(u16::from(c.code)), Some(c.reason.to_string()))).unwrap_or((None, None));
                            reason = DisconnectReason::Closed { code, message };
                            break;
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = sink.send(Message::Pong(p)).await;
                            last_outbound = Instant::now();
                            None
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => None,
                        Some(Ok(Message::Binary(b))) => decode_frame(&b),
                        Some(Ok(Message::Text(t))) => decode_frame(t.as_bytes()),
                    };
                    if let Some(f) = frame {
                        if let Some(r) = handle_frame(&shared, f, &mut conn, &mut attempt) {
                            reason = r;
                            break;
                        }
                    }
                }
            }
        }

        let after = match &reason {
            DisconnectReason::UpdateRequired => {
                set_state(&shared, SyncState::UpdateRequired);
                After::Stop
            }
            DisconnectReason::Closed {
                code: Some(WS_CLOSE_SESSION_REVOKED),
                ..
            } => {
                tracing::warn!("socket: closed with 4001 (session revoked)");
                After::Stop
            }
            r if r.is_too_many_connections() => {
                tracing::warn!("socket: closed with 4029 (too many connections for this device)");
                After::WaitAtLeast(TOO_MANY_CONNECTIONS_WAIT)
            }
            _ => After::Backoff,
        };
        let revoked = matches!(
            reason,
            DisconnectReason::Closed {
                code: Some(WS_CLOSE_SESSION_REVOKED),
                ..
            }
        );
        emit(
            &shared,
            SocketEvent::Disconnected {
                reason: if revoked {
                    DisconnectReason::SessionRevoked { blocked: false }
                } else {
                    reason
                },
            },
        );
        if revoked {
            shared.client.notify_session_invalidated();
        }
        if *stop_rx.borrow() {
            break;
        }
        let min_wait = match after {
            After::Stop => break,
            After::WaitAtLeast(d) => Some(d),
            After::Backoff => None,
        };
        if !backoff_wait(&shared, &mut stop_rx, &mut attempt, min_wait).await {
            break;
        }
    }
}

/// Per-connection state.
struct ConnState {
    got_hello: bool,
    /// Ping after this much outbound silence (`hello.limits.heartbeatSeconds`).
    heartbeat: Duration,
}

/// Decode a server frame leniently: unknown `type`s that still carry `messages`
/// (or a single `message`) are treated as message pushes; anything else is
/// logged at WARN with its keys so a protocol change is visible in the logs.
fn decode_frame(bytes: &[u8]) -> Option<ServerFrame> {
    if let Ok(f) = serde_json::from_slice::<ServerFrame>(bytes) {
        return Some(f);
    }
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "socket: non-JSON frame");
            return None;
        }
    };
    let ty = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let group_id = v.get("groupId").and_then(Value::as_str).map(str::to_owned);
    let mut list: Vec<MessageDto> = Vec::new();
    if let Some(arr) = v.get("messages").and_then(Value::as_array) {
        list = arr
            .iter()
            .filter_map(|m| serde_json::from_value(m.clone()).ok())
            .collect();
    } else if let Some(m) = v.get("message").filter(|m| m.is_object()) {
        if let Ok(m) = serde_json::from_value::<MessageDto>(m.clone()) {
            list.push(m);
        }
    } else if v.get("seq").is_some() && v.get("body").is_some() {
        if let Ok(m) = serde_json::from_value::<MessageDto>(v.clone()) {
            list.push(m);
        }
    }
    if !list.is_empty() {
        let gid = group_id.or_else(|| list[0].group_id.clone());
        if let Some(gid) = gid {
            tracing::info!(%ty, n = list.len(), "socket: message push in non-standard frame shape");
            return Some(ServerFrame::Messages {
                group_id: gid,
                messages: list,
                has_more: false,
            });
        }
    }
    let keys = v
        .as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    tracing::warn!(%ty, keys, "socket: unknown frame type (ignored)");
    None
}

/// Returns `Some(reason)` when the connection must be torn down.
fn handle_frame(
    shared: &Shared,
    frame: ServerFrame,
    conn: &mut ConnState,
    attempt: &mut u32,
) -> Option<DisconnectReason> {
    match frame {
        ServerFrame::Hello {
            protocol_version,
            user_id,
            device_id,
            limits,
        } => {
            if protocol_version > WS_PROTOCOL_VERSION {
                tracing::warn!(
                    server = protocol_version,
                    client = WS_PROTOCOL_VERSION,
                    "socket: server speaks a newer protocol; update required"
                );
                return Some(DisconnectReason::UpdateRequired);
            }
            if let Some(hb) = limits.heartbeat_seconds.filter(|s| *s > 0) {
                conn.heartbeat = Duration::from_secs(hb as u64);
            }
            conn.got_hello = true;
            *attempt = 0;
            emit(
                shared,
                SocketEvent::Hello {
                    protocol_version,
                    user_id,
                    device_id,
                    limits,
                },
            );
            set_state(shared, SyncState::Connected);
            emit(shared, SocketEvent::Connected);
            None
        }
        ServerFrame::Pong => None,
        ServerFrame::Messages {
            group_id,
            messages,
            has_more,
        } => {
            if !conn.got_hello {
                // Some servers may skip hello; treat first payload as connected.
                conn.got_hello = true;
                *attempt = 0;
                set_state(shared, SyncState::Connected);
                emit(shared, SocketEvent::Connected);
            }
            let messages = messages
                .into_iter()
                .map(|mut m| {
                    m.group_id.get_or_insert_with(|| group_id.clone());
                    m
                })
                .collect();
            emit(
                shared,
                SocketEvent::Messages {
                    group_id,
                    messages,
                    has_more,
                },
            );
            None
        }
        ServerFrame::Group { event, fields } => {
            emit(
                shared,
                SocketEvent::GroupEvent(GroupEvent::parse(&event, fields)),
            );
            None
        }
        ServerFrame::Read { group_id, read_seq } => {
            emit(shared, SocketEvent::Read { group_id, read_seq });
            None
        }
        ServerFrame::Error { code, detail } => {
            tracing::warn!(%code, ?detail, "socket: error frame");
            if is_update_required_code(&code) {
                Some(DisconnectReason::UpdateRequired)
            } else if code == "push_failed" {
                // The server could not load our pending messages: reconnect to retry.
                Some(DisconnectReason::Error { cause: Some(code) })
            } else {
                // ack_failed / read_failed / rate_limited / bad frames: not fatal.
                None
            }
        }
    }
}

/// Sleep with backoff (at least `min_wait` when given); returns `false` if stop was
/// requested meanwhile.
async fn backoff_wait(
    shared: &Shared,
    stop_rx: &mut watch::Receiver<bool>,
    attempt: &mut u32,
    min_wait: Option<Duration>,
) -> bool {
    set_state(shared, SyncState::BackingOff);
    let mut delay: Duration = backoff_delay(*attempt);
    if let Some(min) = min_wait {
        delay = delay.max(min);
    }
    *attempt = attempt.saturating_add(1);
    tracing::debug!(?delay, attempt = *attempt, "socket: backing off");
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = stop_rx.changed() => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let s = serde_json::to_string(&ClientFrame::Ack {
            group_id: "g".into(),
            seq: 7,
        })
        .unwrap();
        assert_eq!(s, r#"{"type":"ack","groupId":"g","seq":7}"#);
        assert_eq!(
            serde_json::to_string(&ClientFrame::Ping).unwrap(),
            r#"{"type":"ping"}"#
        );
        let f: ServerFrame = serde_json::from_str(
            r#"{"type":"hello","protocolVersion":1,"userId":"u","deviceId":"d","limits":{"heartbeatSeconds":30,"maxConnectionsPerDevice":3,"maxFrameBytes":16384}}"#,
        )
        .unwrap();
        assert!(matches!(
            f,
            ServerFrame::Hello {
                protocol_version: 1,
                limits: HelloLimits {
                    heartbeat_seconds: Some(30),
                    ..
                },
                ..
            }
        ));
        let f: ServerFrame = serde_json::from_str(r#"{"type":"hello","version":1}"#).unwrap();
        assert!(matches!(
            f,
            ServerFrame::Hello {
                protocol_version: 1,
                ..
            }
        ));
        let f: ServerFrame = serde_json::from_str(
            r#"{"type":"group","event":"role-changed","payload":{"groupId":"g","userId":"u","role":"ADMIN"}}"#,
        )
        .unwrap();
        if let ServerFrame::Group { event, fields } = f {
            assert_eq!(
                GroupEvent::parse(&event, fields),
                GroupEvent::RoleChanged {
                    group_id: "g".into(),
                    user_id: "u".into(),
                    role: Role::Admin
                }
            );
        } else {
            panic!();
        }
    }

    #[test]
    fn official_group_frames() {
        let parse = |j: &str| match serde_json::from_str::<ServerFrame>(j).unwrap() {
            ServerFrame::Group { event, fields } => GroupEvent::parse(&event, fields),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            parse(
                r#"{"type":"group","event":"member-added","groupId":"g","userId":"u","role":"member","joinedSeq":4,"actorId":"a"}"#
            ),
            GroupEvent::MemberAdded {
                group_id: "g".into(),
                user_id: "u".into(),
                role: Some(Role::Member),
                member: None
            }
        );
        assert_eq!(
            parse(
                r#"{"type":"group","event":"member-removed","groupId":"g","userId":"u","actorId":null,"reason":"left"}"#
            ),
            GroupEvent::MemberRemoved {
                group_id: "g".into(),
                user_id: "u".into(),
                reason: Some("left".into())
            }
        );
        assert_eq!(
            parse(
                r#"{"type":"group","event":"group-updated","groupId":"g","name":"N","settings":{"whoCanPost":"admins","whoCanAddMembers":"everyone"},"actorId":"a"}"#
            ),
            GroupEvent::GroupUpdated {
                group_id: "g".into(),
                name: Some("N".into()),
                who_can_post: Some(Permission::admins()),
                who_can_add_members: Some(Permission::everyone())
            }
        );
        let f: ServerFrame =
            serde_json::from_str(r#"{"type":"read","groupId":"g","readSeq":9}"#).unwrap();
        assert_eq!(
            f,
            ServerFrame::Read {
                group_id: "g".into(),
                read_seq: 9
            }
        );
        assert_eq!(
            serde_json::to_string(&ClientFrame::Read {
                group_id: "g".into(),
                seq: 3
            })
            .unwrap(),
            r#"{"type":"read","groupId":"g","seq":3}"#
        );
    }

    #[test]
    fn update_required_codes() {
        assert!(is_update_required_code("update-required"));
        assert!(is_update_required_code("version-mismatch"));
        assert!(!is_update_required_code("bad-frame"));
    }
}

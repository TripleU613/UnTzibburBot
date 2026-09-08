//! WebSocket protocol (`wss://api.tzibbur.me/v1/ws`, protocol version 1).
//!
//! ```text
//! Idle → Connecting → Connected ⟲ BackingOff
//!                  ↘ UpdateRequired
//! ```
//!
//! * Bearer token is sent as an `Authorization` header at handshake.
//! * A 401 at handshake invalidates the session (no reconnect).
//! * Close code 4029 means "too many connections" and is retried with backoff.
//! * The client sends `ping` after [`PING_AFTER_OUTBOUND_SILENCE`] of outbound silence.
//! * An `error` frame whose code indicates a version mismatch moves the socket
//!   to [`SyncState::UpdateRequired`], which is sticky across `stop()`/`start()`.

use crate::backoff::backoff_delay;
use crate::constants::{
    PING_AFTER_OUTBOUND_SILENCE, WS_CLOSE_TOO_MANY_CONNECTIONS, WS_EVENT_BUFFER,
    WS_PROTOCOL_VERSION,
};
use crate::error::{AppError, Result};
use crate::http::TzibburClient;
use crate::models::{MemberDto, MessageDto, Permission, Role};
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

/// Client → server frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientFrame {
    /// Sent after outbound silence.
    Ping,
    /// Mark messages in `group_id` read up to `seq`.
    #[serde(rename_all = "camelCase")]
    Ack { group_id: String, seq: i64 },
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
    /// Group lifecycle event.
    Group {
        event: String,
        #[serde(default)]
        payload: Map<String, Value>,
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
    MemberAdded {
        group_id: String,
        member: MemberDto,
    },
    MemberRemoved {
        group_id: String,
        user_id: String,
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

    /// Parse `event` + `payload` from a `group` frame (or a pending-events entry).
    pub fn parse(event: &str, payload: Map<String, Value>) -> Self {
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
                match payload
                    .get("member")
                    .cloned()
                    .map(serde_json::from_value::<MemberDto>)
                {
                    Some(Ok(member)) => GroupEvent::MemberAdded { group_id, member },
                    _ => unknown(payload),
                }
            }
            (Self::MEMBER_REMOVED, Some(group_id)) => match str_field("userId") {
                Some(user_id) => GroupEvent::MemberRemoved { group_id, user_id },
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
            (Self::GROUP_UPDATED, Some(group_id)) => GroupEvent::GroupUpdated {
                group_id,
                name: str_field("name"),
                who_can_post: str_field("whoCanPost").map(Permission),
                who_can_add_members: str_field("whoCanAddMembers").map(Permission),
            },
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
    Messages {
        group_id: String,
        messages: Vec<MessageDto>,
    },
    GroupEvent(GroupEvent),
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
        for (k, v) in shared.client.device().headers() {
            if let (Ok(name), Ok(val)) = (
                http::header::HeaderName::from_bytes(k.as_bytes()),
                v.parse::<http::HeaderValue>(),
            ) {
                req.headers_mut().insert(name, val);
            }
        }
        if let Ok(v) = WS_PROTOCOL_VERSION.to_string().parse() {
            req.headers_mut().insert("X-Tzibbur-Protocol-Version", v);
        }

        let connect = tokio::select! {
            r = tokio_tungstenite::connect_async(req) => r,
            _ = stop_rx.changed() => break,
        };

        let ws = match connect {
            Ok((ws, _resp)) => ws,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp))
                if resp.status() == http::StatusCode::UNAUTHORIZED =>
            {
                tracing::warn!("socket: 401 at handshake, invalidating session");
                emit(
                    &shared,
                    SocketEvent::Disconnected {
                        reason: DisconnectReason::Error {
                            cause: Some("401 Unauthorized".into()),
                        },
                    },
                );
                shared.client.notify_session_invalidated();
                break;
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
                if !backoff_wait(&shared, &mut stop_rx, &mut attempt).await {
                    break;
                }
                continue;
            }
        };

        let (mut sink, mut stream) = ws.split();
        let mut last_outbound = Instant::now();
        let mut got_hello = false;
        let reason: DisconnectReason;
        let mut update_required = false;

        loop {
            let ping_at = last_outbound + PING_AFTER_OUTBOUND_SILENCE;
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
                            if let Err(e) = sink.send(Message::Text(txt)).await {
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
                    if let Err(e) = sink.send(Message::Text(txt)).await {
                        reason = DisconnectReason::Error { cause: Some(e.to_string()) };
                        break;
                    }
                    last_outbound = Instant::now();
                }
                msg = stream.next() => {
                    match msg {
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
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                        Some(Ok(Message::Binary(b))) => {
                            match serde_json::from_slice::<ServerFrame>(&b) {
                                Ok(f) => if let Some(r) = handle_frame(&shared, f, &mut got_hello, &mut attempt) { update_required = true; reason = r; break; },
                                Err(e) => tracing::debug!(error = %e, "socket: unparseable binary frame"),
                            }
                        }
                        Some(Ok(Message::Text(t))) => {
                            match serde_json::from_str::<ServerFrame>(&t) {
                                Ok(f) => if let Some(r) = handle_frame(&shared, f, &mut got_hello, &mut attempt) { update_required = true; reason = r; break; },
                                Err(e) => tracing::debug!(error = %e, frame = %t, "socket: unknown frame"),
                            }
                        }
                    }
                }
            }
        }

        if reason.is_too_many_connections() {
            tracing::warn!("socket: closed with 4029 (too many connections)");
        }
        emit(&shared, SocketEvent::Disconnected { reason });

        if update_required {
            set_state(&shared, SyncState::UpdateRequired);
            break;
        }
        if *stop_rx.borrow() {
            break;
        }
        if !backoff_wait(&shared, &mut stop_rx, &mut attempt).await {
            break;
        }
    }
}

/// Returns `Some(reason)` when the connection must be torn down.
fn handle_frame(
    shared: &Shared,
    frame: ServerFrame,
    got_hello: &mut bool,
    attempt: &mut u32,
) -> Option<DisconnectReason> {
    match frame {
        ServerFrame::Hello {
            protocol_version,
            user_id,
            device_id,
            limits,
        } => {
            if protocol_version != WS_PROTOCOL_VERSION {
                tracing::warn!(
                    server = protocol_version,
                    client = WS_PROTOCOL_VERSION,
                    "socket: protocol version differs"
                );
            }
            *got_hello = true;
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
            if has_more {
                tracing::debug!(%group_id, "socket: messages frame has more; catch-up will page");
            }
            if !*got_hello {
                // Some servers may skip hello; treat first payload as connected.
                *got_hello = true;
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
            emit(shared, SocketEvent::Messages { group_id, messages });
            None
        }
        ServerFrame::Group { event, payload } => {
            emit(
                shared,
                SocketEvent::GroupEvent(GroupEvent::parse(&event, payload)),
            );
            None
        }
        ServerFrame::Error { code, detail } => {
            tracing::warn!(%code, ?detail, "socket: error frame");
            if is_update_required_code(&code) {
                Some(DisconnectReason::UpdateRequired)
            } else {
                None
            }
        }
    }
}

/// Sleep with backoff; returns `false` if stop was requested meanwhile.
async fn backoff_wait(
    shared: &Shared,
    stop_rx: &mut watch::Receiver<bool>,
    attempt: &mut u32,
) -> bool {
    set_state(shared, SyncState::BackingOff);
    let delay: Duration = backoff_delay(*attempt);
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
        if let ServerFrame::Group { event, payload } = f {
            assert_eq!(
                GroupEvent::parse(&event, payload),
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
    fn update_required_codes() {
        assert!(is_update_required_code("update-required"));
        assert!(is_update_required_code("version-mismatch"));
        assert!(!is_update_required_code("bad-frame"));
    }
}

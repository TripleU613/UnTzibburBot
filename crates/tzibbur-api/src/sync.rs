//! `SyncEngine`: ties the WebSocket, REST catch-up, group reconciliation and the
//! outbox together on top of a [`LocalStore`].
//!
//! Delivery follows the official store-and-forward contract
//! (<https://api.tzibbur.me/integration>, §9–§10):
//!
//! * The WebSocket is the only live transport. After `hello` the server pushes every
//!   undelivered batch by itself; each `messages` frame is stored and then acked **on
//!   the socket** with its last seq, which releases the next batch for that group.
//! * On every (re)connect the group list is re-fetched once (`GET /v1/groups`), because
//!   group events are not replayed. Nothing else is fetched: no per-group paging and
//!   no periodic sweeps.
//! * Only while the socket is down does the engine fall back to `GET /v1/pending` +
//!   REST acks, at most once per [`SyncEngine::fallback_poll_interval`].
//!
//! ```text
//! Idle ──start()──▶ Connecting ──hello──▶ Connected
//!                                             │
//!                               error(UpdateRequired)
//!                                             ▼
//!                                     UpdateRequired ──stop()──▶ Idle
//!                                                     (UpdateRequired preserved across stop/start)
//! ```

use crate::error::{AppError, Result};
use crate::http::TzibburClient;
use crate::models::{GroupDto, MessagesQuery, PendingResponse, Role};
use crate::outbox::OutboxDispatcher;
use crate::store::{
    GroupEntity, LocalStore, MemberEntity, MessageEntity, OutboxEntity, StoreBatchOutcome,
};
pub use crate::ws::{DisconnectReason, SyncState};
use crate::ws::{GroupEvent, SocketEvent, TzibburSocket};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// Ref-counted set of group IDs currently observed by the UI. The engine only
/// eagerly refreshes members for observed groups on `member-added`.
#[derive(Debug, Default)]
pub struct MemberObservationRegistry {
    counts: Mutex<HashMap<String, usize>>,
}

impl MemberObservationRegistry {
    pub fn retain(&self, group_id: &str) {
        *self.counts.lock().entry(group_id.to_owned()).or_insert(0) += 1;
    }
    pub fn release(&self, group_id: &str) {
        let mut c = self.counts.lock();
        if let Some(n) = c.get_mut(group_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.remove(group_id);
            }
        }
    }
    pub fn is_observed(&self, group_id: &str) -> bool {
        self.counts.lock().contains_key(group_id)
    }
    pub fn observed_group_ids(&self) -> Vec<String> {
        self.counts.lock().keys().cloned().collect()
    }
}

/// High-level events for consumers (a UI, or a bridge).
#[derive(Debug, Clone, PartialEq)]
pub enum SyncEvent {
    StateChanged(SyncState),
    /// Truly new (non-echo) messages were stored for a group.
    NewMessages {
        group_id: String,
        messages: Vec<MessageEntity>,
    },
    /// One of our own messages was confirmed via echo.
    EchoConfirmed {
        group_id: String,
        client_message_id: String,
        server_id: String,
        seq: i64,
    },
    GroupChanged {
        group_id: String,
    },
    GroupDeleted {
        group_id: String,
    },
    MembersChanged {
        group_id: String,
    },
    /// A member joined (from a `member-added` event).
    MemberAdded {
        group_id: String,
        member: MemberEntity,
    },
    /// A member left or was removed; `display_name` is the cached name if known.
    MemberRemoved {
        group_id: String,
        user_id: String,
        display_name: Option<String>,
    },
    RoleChanged {
        group_id: String,
        user_id: String,
        role: Role,
        display_name: Option<String>,
    },
    /// The account was added to a group (a `member-added` about ourselves); the
    /// group row has been fetched.
    GroupJoined {
        group_id: String,
    },
    /// A full catch-up / group reconciliation completed.
    CaughtUp,
    UpdateRequired,
}

/// Selects which groups' members are refreshed on `member-added`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRefreshPolicy {
    /// Only groups retained in the [`MemberObservationRegistry`] (mobile behaviour).
    ObservedOnly,
    /// Every group (useful for a headless bridge).
    All,
}

pub struct SyncEngine {
    client: TzibburClient,
    store: Arc<dyn LocalStore>,
    socket: Arc<TzibburSocket>,
    outbox: Arc<OutboxDispatcher>,
    pub observed: Arc<MemberObservationRegistry>,
    self_user_id: Mutex<Option<String>>,
    member_refresh: MemberRefreshPolicy,
    state_tx: watch::Sender<SyncState>,
    events: broadcast::Sender<SyncEvent>,
    task: Mutex<Option<(JoinHandle<()>, watch::Sender<bool>)>>,
    pending_limit: Option<u32>,
    /// While the socket is down, how often to fall back to `GET /v1/pending`.
    fallback_poll_interval: std::time::Duration,
}

/// Where a batch came from, which decides how it is acknowledged: a batch delivered
/// on the socket must be acked on the socket, a REST-pulled one over REST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckVia {
    Socket,
    Rest,
    /// History paging: nothing to acknowledge.
    None,
}

/// Default for [`SyncEngine::fallback_poll_interval`].
pub const DEFAULT_FALLBACK_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

impl SyncEngine {
    pub fn new(client: TzibburClient, store: Arc<dyn LocalStore>) -> Result<Arc<Self>> {
        let socket = Arc::new(TzibburSocket::new(client.clone())?);
        Ok(Self::with_parts(
            client,
            store,
            socket,
            MemberRefreshPolicy::ObservedOnly,
        ))
    }

    pub fn with_parts(
        client: TzibburClient,
        store: Arc<dyn LocalStore>,
        socket: Arc<TzibburSocket>,
        member_refresh: MemberRefreshPolicy,
    ) -> Arc<Self> {
        let outbox = OutboxDispatcher::new(client.clone(), store.clone());
        let (state_tx, _) = watch::channel(SyncState::Idle);
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            client,
            store,
            socket,
            outbox,
            observed: Arc::new(MemberObservationRegistry::default()),
            self_user_id: Mutex::new(None),
            member_refresh,
            state_tx,
            events,
            task: Mutex::new(None),
            pending_limit: None,
            fallback_poll_interval: DEFAULT_FALLBACK_POLL_INTERVAL,
        })
    }

    /// While the socket is down, how often the engine polls `GET /v1/pending`.
    pub fn fallback_poll_interval(&self) -> std::time::Duration {
        self.fallback_poll_interval
    }

    /// The signed-in user's id, used for unread accounting and echo labelling.
    pub fn set_self_user_id(&self, id: Option<String>) {
        *self.self_user_id.lock() = id;
    }
    pub fn self_user_id(&self) -> Option<String> {
        self.self_user_id.lock().clone()
    }

    pub fn client(&self) -> &TzibburClient {
        &self.client
    }
    pub fn store(&self) -> &Arc<dyn LocalStore> {
        &self.store
    }
    pub fn socket(&self) -> &Arc<TzibburSocket> {
        &self.socket
    }
    pub fn outbox(&self) -> &Arc<OutboxDispatcher> {
        &self.outbox
    }

    pub fn sync_state(&self) -> SyncState {
        *self.state_tx.borrow()
    }
    pub fn watch_sync_state(&self) -> watch::Receiver<SyncState> {
        self.state_tx.subscribe()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    fn emit(&self, ev: SyncEvent) {
        let _ = self.events.send(ev);
    }

    fn set_state(&self, s: SyncState) {
        let changed = self.state_tx.send_if_modified(|cur| {
            if *cur != s {
                *cur = s;
                true
            } else {
                false
            }
        });
        if changed {
            self.emit(SyncEvent::StateChanged(s));
            if s == SyncState::UpdateRequired {
                self.emit(SyncEvent::UpdateRequired);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle (SyncController)
    // -----------------------------------------------------------------------

    /// Start socket + outbox and the event loop, then `poke()`.
    pub fn start(self: &Arc<Self>) {
        let mut task = self.task.lock();
        if task.is_some() {
            return;
        }
        if self.sync_state() == SyncState::UpdateRequired {
            tracing::warn!("sync: start ignored, UpdateRequired");
            return;
        }
        self.set_state(SyncState::Connecting);
        self.socket.start();
        self.outbox.start();
        let (stop_tx, stop_rx) = watch::channel(false);
        let me = self.clone();
        let handle = tokio::spawn(async move { me.event_loop(stop_rx).await });
        *task = Some((handle, stop_tx));
        self.outbox.poke();
    }

    /// Stop socket, outbox and the loop. `UpdateRequired` is preserved.
    pub async fn stop(&self) {
        let taken = self.task.lock().take();
        if let Some((handle, stop_tx)) = taken {
            let _ = stop_tx.send(true);
            let _ = handle.await;
        }
        self.socket.stop().await;
        self.outbox.stop().await;
        if self.sync_state() != SyncState::UpdateRequired {
            self.set_state(SyncState::Idle);
        }
    }

    pub fn is_running(&self) -> bool {
        self.task.lock().is_some()
    }

    /// Pull-to-refresh: re-fetch the group list and, only when the socket is down,
    /// catch up over REST. While connected the socket already delivers everything.
    pub async fn refresh_now(&self) -> Result<()> {
        self.reconcile_groups().await?;
        if self.sync_state() != SyncState::Connected {
            self.rest_catch_up().await?;
        } else {
            self.emit(SyncEvent::CaughtUp);
        }
        self.outbox.poke();
        Ok(())
    }

    async fn event_loop(self: Arc<Self>, mut stop_rx: watch::Receiver<bool>) {
        let mut events = self.socket.subscribe();
        let mut state_rx = self.socket.watch_state();
        let mut outbox_events = self.outbox.subscribe();
        let mut fallback = tokio::time::interval(self.fallback_poll_interval);
        fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        fallback.tick().await; // first tick fires immediately; skip it
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = fallback.tick() => {
                    // Only while the socket is down: the socket delivers everything otherwise.
                    let s = self.sync_state();
                    if s != SyncState::Connected && s != SyncState::UpdateRequired {
                        if let Err(e) = self.rest_catch_up().await {
                            tracing::debug!(error = %e, "sync: fallback poll failed");
                        }
                    }
                }
                ob = outbox_events.recv() => {
                    if let Ok(crate::outbox::OutboxEvent::Confirmed { client_message_id, message }) = ob {
                        self.emit(SyncEvent::EchoConfirmed {
                            group_id: message.group_id.clone().unwrap_or_default(),
                            client_message_id,
                            server_id: message.id,
                            seq: message.seq,
                        });
                    }
                }
                Ok(()) = state_rx.changed() => {
                    let s = *state_rx.borrow();
                    self.set_state(s);
                }
                ev = events.recv() => match ev {
                    Ok(ev) => {
                        if let Err(e) = self.handle_socket_event(ev).await {
                            tracing::warn!(error = %e, "sync: failed to handle socket event");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Dropped frames were never acked; a fresh connection makes the
                        // server redeliver them (never a REST ack for socket batches).
                        tracing::warn!(missed = n, "sync: lagged behind socket events, reconnecting");
                        self.socket.reconnect().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    async fn handle_socket_event(&self, ev: SocketEvent) -> Result<()> {
        match ev {
            SocketEvent::Connected => {
                self.set_state(SyncState::Connected);
                // Group events are never replayed, so re-fetch the list once per connect.
                // The backlog itself arrives on the socket without asking.
                if let Err(e) = self.reconcile_groups().await {
                    tracing::warn!(error = %e, "sync: group reconcile on connect failed");
                }
                self.emit(SyncEvent::CaughtUp);
                self.outbox.poke();
            }
            SocketEvent::Disconnected { reason } => {
                if reason == DisconnectReason::UpdateRequired {
                    self.set_state(SyncState::UpdateRequired);
                }
            }
            SocketEvent::Hello { .. } => {}
            SocketEvent::Messages {
                group_id, messages, ..
            } => {
                let batch = messages
                    .into_iter()
                    .map(|m| MessageEntity::from_dto(m, &group_id))
                    .collect();
                self.apply_incoming(&group_id, batch, AckVia::Socket)
                    .await?;
            }
            SocketEvent::GroupEvent(ge) => self.apply_group_event(ge).await?,
            SocketEvent::Read { group_id, read_seq } => {
                self.store.mark_read(&group_id, read_seq)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Applying data
    // -----------------------------------------------------------------------

    /// Ensure the group row exists locally (fetching it if needed), store a batch,
    /// then acknowledge it as `ack` says (the last seq of the batch, after storing).
    pub async fn apply_incoming(
        &self,
        group_id: &str,
        batch: Vec<MessageEntity>,
        ack: AckVia,
    ) -> Result<StoreBatchOutcome> {
        let last_seq = batch.iter().map(|m| m.seq).max();
        if self.store.get_group(group_id)?.is_none() {
            match self.client.get_group(group_id).await {
                Ok(g) => self.store.upsert_group(&GroupEntity::from(g))?,
                Err(AppError::NotFound { .. }) => {
                    self.store.mark_group_deleted(group_id)?;
                    // Not a member: the server ignores the ack, but a socket batch must
                    // still be answered or nothing more arrives for the group.
                    if let Some(seq) = last_seq {
                        self.ack_delivered(group_id, seq, ack).await;
                    }
                    return Ok(StoreBatchOutcome::default());
                }
                Err(e) => {
                    tracing::warn!(error = %e, group_id, "sync: could not fetch unknown group")
                }
            }
        }
        let batch_len = batch.len();
        let outcome = self.store.store_incoming_batch(group_id, batch)?;
        tracing::debug!(
            group_id,
            batch = batch_len,
            inserted = outcome.inserted,
            max_seq = ?outcome.plan.max_seq,
            echoes = outcome.plan.echo_confirmations.len(),
            "sync: batch applied"
        );
        // Delivery acknowledgement, only now that the batch is durably stored. The server
        // keeps one un-acked socket batch in flight per group and sends nothing more for
        // it until this arrives, so a duplicate-only batch is acked too.
        if let Some(seq) = last_seq {
            self.ack_delivered(group_id, seq, ack).await;
        }
        for echo in &outcome.plan.echo_confirmations {
            self.emit(SyncEvent::EchoConfirmed {
                group_id: group_id.to_owned(),
                client_message_id: echo.client_message_id.clone(),
                server_id: echo.server_id.clone(),
                seq: echo.seq,
            });
        }
        if !outcome.plan.new_message_ids.is_empty() {
            let ids: std::collections::HashSet<&str> = outcome
                .plan
                .new_message_ids
                .iter()
                .map(String::as_str)
                .collect();
            let messages: Vec<MessageEntity> = outcome
                .plan
                .to_insert
                .iter()
                .filter(|m| ids.contains(m.id.as_str()))
                .cloned()
                .collect();
            self.emit(SyncEvent::NewMessages {
                group_id: group_id.to_owned(),
                messages,
            });
        }
        Ok(outcome)
    }

    /// Apply a `group` frame / pending event to the store.
    pub async fn apply_group_event(&self, ge: GroupEvent) -> Result<()> {
        match ge {
            GroupEvent::MemberAdded {
                group_id,
                user_id,
                role,
                member,
            } => {
                if self.self_user_id().as_deref() == Some(user_id.as_str()) {
                    // We were added to a group: its messages start flowing on this socket.
                    if self.refresh_group(&group_id).await?.is_some() {
                        let _ = self.refresh_members(&group_id).await;
                        self.emit(SyncEvent::GroupJoined { group_id });
                    }
                    return Ok(());
                }
                let refresh = match self.member_refresh {
                    MemberRefreshPolicy::All => true,
                    MemberRefreshPolicy::ObservedOnly => self.observed.is_observed(&group_id),
                };
                if let Some(m) = member {
                    self.store
                        .upsert_member(&MemberEntity::from_dto(m, &group_id))?;
                }
                self.store.set_group_member_count(&group_id, 1)?;
                // The official frame names only the user; the member list has the name.
                if refresh {
                    if let Err(e) = self.refresh_members(&group_id).await {
                        tracing::warn!(error = %e, group_id, "sync: member refresh failed");
                    }
                }
                let entity = self
                    .store
                    .members(&group_id)
                    .ok()
                    .and_then(|ms| ms.into_iter().find(|m| m.user_id == user_id));
                let member = entity.unwrap_or_else(|| MemberEntity {
                    group_id: group_id.clone(),
                    user_id: user_id.clone(),
                    display_name: String::new(),
                    phone_e164: None,
                    role: role.unwrap_or_default(),
                    joined_at: crate::models::now_epoch_ms(),
                });
                self.emit(SyncEvent::MemberAdded {
                    group_id: group_id.clone(),
                    member,
                });
                self.emit(SyncEvent::MembersChanged { group_id });
            }
            GroupEvent::MemberRemoved {
                group_id, user_id, ..
            } => {
                let display_name = self.store.members(&group_id).ok().and_then(|ms| {
                    ms.into_iter()
                        .find(|m| m.user_id == user_id)
                        .map(|m| m.display_name)
                });
                self.emit(SyncEvent::MemberRemoved {
                    group_id: group_id.clone(),
                    user_id: user_id.clone(),
                    display_name,
                });
                self.store.delete_member(&group_id, &user_id)?;
                self.store.set_group_member_count(&group_id, -1)?;
                if self.self_user_id().as_deref() == Some(user_id.as_str()) {
                    self.store.mark_group_deleted(&group_id)?;
                    self.emit(SyncEvent::GroupDeleted {
                        group_id: group_id.clone(),
                    });
                }
                self.emit(SyncEvent::MembersChanged { group_id });
            }
            GroupEvent::RoleChanged {
                group_id,
                user_id,
                role,
            } => {
                self.store.set_role(&group_id, &user_id, role)?;
                let display_name = self.store.members(&group_id).ok().and_then(|ms| {
                    ms.into_iter()
                        .find(|m| m.user_id == user_id)
                        .map(|m| m.display_name)
                });
                self.emit(SyncEvent::RoleChanged {
                    group_id: group_id.clone(),
                    user_id: user_id.clone(),
                    role,
                    display_name,
                });
                if self.self_user_id().as_deref() == Some(user_id.as_str()) {
                    self.store.set_my_role(&group_id, role)?;
                    self.emit(SyncEvent::GroupChanged {
                        group_id: group_id.clone(),
                    });
                }
                self.emit(SyncEvent::MembersChanged { group_id });
            }
            GroupEvent::GroupUpdated {
                group_id,
                name,
                who_can_post,
                who_can_add_members,
            } => {
                self.store.apply_group_updated(
                    &group_id,
                    name.as_deref(),
                    who_can_post.as_ref(),
                    who_can_add_members.as_ref(),
                )?;
                self.emit(SyncEvent::GroupChanged { group_id });
            }
            GroupEvent::GroupDeleted { group_id } => {
                self.store.mark_group_deleted(&group_id)?;
                self.emit(SyncEvent::GroupDeleted { group_id });
            }
            GroupEvent::Unknown {
                event, group_id, ..
            } => {
                tracing::debug!(event, ?group_id, "sync: ignoring unknown group event");
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // REST-driven sync
    // -----------------------------------------------------------------------

    /// `GET /v1/pending` and apply everything it contains, acking each group over
    /// REST; pulls again while any group reports `hasMore`. For use while no socket
    /// is open (never REST-ack what an open socket delivered).
    pub async fn rest_catch_up(&self) -> Result<PendingResponse> {
        let mut total = PendingResponse::default();
        // Bounded: each round drains up to `limit` messages per group.
        for _ in 0..50 {
            let page = self.client.pending(self.pending_limit).await?;
            tracing::debug!(
                buckets = page.messages.len(),
                messages = page.message_count(),
                "sync: pending page"
            );
            for g in &page.groups {
                self.store.upsert_group(&GroupEntity::from(g.clone()))?;
            }
            for bucket in &page.messages {
                let batch: Vec<MessageEntity> = bucket
                    .messages
                    .iter()
                    .cloned()
                    .map(|m| MessageEntity::from_dto(m, &bucket.group_id))
                    .collect();
                self.apply_incoming(&bucket.group_id, batch, AckVia::Rest)
                    .await?;
            }
            for ev in &page.events {
                self.apply_group_event(GroupEvent::parse(&ev.event, ev.payload.clone()))
                    .await?;
            }
            let more = page
                .messages
                .iter()
                .any(|b| b.has_more && !b.messages.is_empty())
                || (page.has_more && page.next_cursor.is_some());
            total.messages.extend(page.messages);
            total.groups.extend(page.groups);
            total.events.extend(page.events);
            if !more {
                break;
            }
        }
        self.emit(SyncEvent::CaughtUp);
        Ok(total)
    }

    /// Acknowledge delivery up to `seq` on the transport that delivered the batch.
    /// Errors are logged, never fatal: an un-acked batch is simply redelivered.
    pub async fn ack_delivered(&self, group_id: &str, seq: i64, via: AckVia) {
        if seq <= 0 {
            return;
        }
        let r = match via {
            AckVia::Socket => self.socket.ack(group_id, seq).await,
            AckVia::Rest => self.client.ack(group_id, seq).await,
            AckVia::None => return,
        };
        match r {
            Ok(()) => tracing::debug!(group_id, seq, ?via, "sync: delivery ack sent"),
            Err(e) => tracing::warn!(error = %e, group_id, seq, "sync: delivery ack failed"),
        }
    }

    /// History paging: `GET /v1/groups/{id}/messages?afterSeq=` from the newest stored
    /// seq until the server reports no `nextAfterSeq`. Returns messages stored. This is
    /// for scrollback (e.g. after `#unmute`); it is never run automatically.
    pub async fn catch_up_group(&self, group_id: &str) -> Result<usize> {
        let mut after = self.store.max_seq(group_id)?;
        let mut stored = 0;
        for _ in 0..1000 {
            let q = MessagesQuery {
                after_seq: after,
                before_seq: None,
                limit: Some(100),
            };
            let page = match self.client.get_messages_page(group_id, &q).await {
                Ok(p) => p,
                Err(AppError::NotFound { .. }) => {
                    self.store.mark_group_deleted(group_id)?;
                    return Ok(stored);
                }
                Err(e) => return Err(e),
            };
            if page.items.is_empty() {
                break;
            }
            let newest = page.items.iter().map(|m| m.seq).max();
            let batch = page
                .items
                .into_iter()
                .map(|m| MessageEntity::from_dto(m, group_id))
                .collect();
            stored += self
                .apply_incoming(group_id, batch, AckVia::None)
                .await?
                .inserted;
            match page.next_after_seq {
                Some(next) => after = Some(next),
                None => break,
            }
            if newest.is_none() {
                break;
            }
        }
        Ok(stored)
    }

    /// `GET /v1/groups` (all pages) → upsert + mark deleted anything absent.
    pub async fn reconcile_groups(&self) -> Result<Vec<GroupDto>> {
        let groups = self.client.list_all_groups().await?;
        let entities: Vec<GroupEntity> = groups.iter().cloned().map(GroupEntity::from).collect();
        self.store.reconcile_groups(&entities)?;
        Ok(groups)
    }

    /// Alias for [`Self::reconcile_groups`] (`GroupRepository.refreshGroups`).
    pub async fn refresh_groups(&self) -> Result<Vec<GroupDto>> {
        self.reconcile_groups().await
    }

    /// Fetch a single group; a 404 marks it deleted locally (`DropGroupOnNotFound`).
    pub async fn refresh_group(&self, group_id: &str) -> Result<Option<GroupEntity>> {
        match self.client.get_group(group_id).await {
            Ok(g) => {
                let e = GroupEntity::from(g);
                self.store.upsert_group(&e)?;
                Ok(Some(e))
            }
            Err(AppError::NotFound { .. }) => {
                self.store.mark_group_deleted(group_id)?;
                self.emit(SyncEvent::GroupDeleted {
                    group_id: group_id.to_owned(),
                });
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// `GET /v1/groups/{id}/members` (all pages) → `replaceMembers`.
    pub async fn refresh_members(&self, group_id: &str) -> Result<Vec<MemberEntity>> {
        let members = match self.client.list_all_members(group_id).await {
            Ok(m) => m,
            Err(AppError::NotFound { .. }) => {
                self.store.mark_group_deleted(group_id)?;
                self.emit(SyncEvent::GroupDeleted {
                    group_id: group_id.to_owned(),
                });
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let entities: Vec<MemberEntity> = members
            .into_iter()
            .map(|m| MemberEntity::from_dto(m, group_id))
            .collect();
        self.store.replace_members(group_id, &entities)?;
        self.emit(SyncEvent::MembersChanged {
            group_id: group_id.to_owned(),
        });
        Ok(entities)
    }

    /// Load the initial page (or newer messages) for a group via REST.
    pub async fn load_latest_messages(
        &self,
        group_id: &str,
        limit: u32,
    ) -> Result<StoreBatchOutcome> {
        let after = self.store.max_seq(group_id)?;
        let mut q = MessagesQuery::default().limit(limit);
        q.after_seq = after;
        let msgs = match self.client.get_messages(group_id, &q).await {
            Ok(m) => m,
            Err(AppError::NotFound { .. }) => {
                self.store.mark_group_deleted(group_id)?;
                return Ok(StoreBatchOutcome::default());
            }
            Err(e) => return Err(e),
        };
        let batch = msgs
            .into_iter()
            .map(|m| MessageEntity::from_dto(m, group_id))
            .collect();
        self.apply_incoming(group_id, batch, AckVia::None).await
    }

    /// `MessageRepository.loadOlderMessages`: page backwards from the oldest stored seq.
    pub async fn load_older_messages(&self, group_id: &str, limit: u32) -> Result<usize> {
        let before = self.store.min_seq(group_id)?;
        let q = MessagesQuery {
            after_seq: None,
            before_seq: before,
            limit: Some(limit),
        };
        let msgs = self.client.get_messages(group_id, &q).await?;
        let batch: Vec<MessageEntity> = msgs
            .into_iter()
            .map(|m| MessageEntity::from_dto(m, group_id))
            .collect();
        self.store.insert_messages(&batch)
    }

    // -----------------------------------------------------------------------
    // User actions
    // -----------------------------------------------------------------------

    /// Queue a message for delivery through the outbox.
    pub fn send_message(&self, group_id: &str, body: &str) -> Result<OutboxEntity> {
        self.outbox.enqueue(group_id, body)
    }

    /// Move the user's read position (per user; clears the badge on their other
    /// devices): a `read` frame when connected, `POST /v1/groups/{id}/read` otherwise.
    /// This is not a delivery ack; the engine acks stored batches by itself.
    pub async fn mark_read(&self, group_id: &str, seq: i64) -> Result<()> {
        self.store.mark_read(group_id, seq)?;
        if self.sync_state() == SyncState::Connected {
            self.socket.read(group_id, seq).await
        } else {
            self.client.mark_read(group_id, seq).await
        }
    }

    /// Local mute flag. (Server-side mute is the in-chat `#mute` command.)
    pub fn mute_group(&self, group_id: &str, muted: bool) -> Result<()> {
        self.store.set_muted(group_id, muted)
    }

    /// `POST /v1/groups/{id}/leave` then mark deleted locally.
    pub async fn leave_group(&self, group_id: &str) -> Result<()> {
        self.client.leave_group(group_id).await?;
        self.store.mark_group_deleted(group_id)?;
        self.emit(SyncEvent::GroupDeleted {
            group_id: group_id.to_owned(),
        });
        Ok(())
    }

    /// `DELETE /v1/groups/{id}` then mark deleted locally.
    pub async fn delete_group(&self, group_id: &str) -> Result<()> {
        self.client.delete_group(group_id).await?;
        self.store.mark_group_deleted(group_id)?;
        self.emit(SyncEvent::GroupDeleted {
            group_id: group_id.to_owned(),
        });
        Ok(())
    }
}

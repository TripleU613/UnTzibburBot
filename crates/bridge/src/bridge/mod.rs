//! Per-account runtime: owns the Tzibbur connection for one connected account
//! and mirrors its groups into Telegram topics.
//!
//! ```text
//! Tzibbur WS/REST ─▶ tzibbur_api::SyncEngine ─▶ SyncEvent ─▶ AccountRuntime ─▶ Telegram topic
//! Telegram topic  ─▶ telegram handlers ─▶ AccountRuntime::send_text ─▶ outbox ─▶ Tzibbur
//! ```
//!
//! One runtime per connected account; events for an account are processed
//! sequentially, which keeps per-conversation ordering.

pub mod format;

use crate::config::Config;
use crate::crypto::SessionCipher;
use crate::store::{Account, AccountSettings, AccountStatus, Conversation, Store};
use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use teloxide::adaptors::Throttle;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode, ThreadId,
};
use tokio::sync::mpsc;
use tzibbur_api::http::SessionInvalidationListener;
use tzibbur_api::models::{GroupKind, Role};
use tzibbur_api::outbox::OutboxEvent;
use tzibbur_api::prelude::*;

pub type BridgeBot = Throttle<Bot>;

/// Live Tzibbur limit observed on the server (the app's compiled default is 2000).
pub const MAX_OUTBOUND_CHARS: usize = 1000;

/// Shared services every runtime needs.
pub struct Shared {
    pub cfg: Config,
    pub store: Store,
    pub cipher: SessionCipher,
    pub bot: BridgeBot,
    /// `getMe.has_topics_enabled` — topic mode must be enabled for the bot in @BotFather.
    pub bot_topics_enabled: AtomicBool,
}

/// All running account runtimes, keyed by bridge account id.
#[derive(Default)]
pub struct Registry {
    runtimes: DashMap<i64, Arc<AccountRuntime>>,
}

impl Registry {
    pub fn get(&self, account_id: i64) -> Option<Arc<AccountRuntime>> {
        self.runtimes.get(&account_id).map(|r| r.clone())
    }
    pub fn insert(&self, rt: Arc<AccountRuntime>) {
        self.runtimes.insert(rt.account_id, rt);
    }
    pub async fn remove(&self, account_id: i64) {
        if let Some((_, rt)) = self.runtimes.remove(&account_id) {
            rt.stop().await;
        }
    }
    pub fn len(&self) -> usize {
        self.runtimes.len()
    }
    pub async fn stop_all(&self) {
        let ids: Vec<i64> = self.runtimes.iter().map(|r| *r.key()).collect();
        for id in ids {
            self.remove(id).await;
        }
    }
}

enum Cmd {
    SessionInvalidated,
    Stop,
}

struct InvalidationHook(mpsc::UnboundedSender<Cmd>);
impl SessionInvalidationListener for InvalidationHook {
    fn on_session_invalidated(&self) {
        let _ = self.0.send(Cmd::SessionInvalidated);
    }
}

pub struct AccountRuntime {
    pub account_id: i64,
    pub telegram_chat_id: i64,
    pub tzibbur_user_id: String,
    shared: Arc<Shared>,
    client: TzibburClient,
    local: Arc<dyn LocalStore>,
    sync: Arc<SyncEngine>,
    settings: RwLock<AccountSettings>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cmd_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<Cmd>>>,
    task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Set once we told the user that topics are unavailable, to avoid nagging.
    topics_warning_sent: AtomicBool,
    db_path: PathBuf,
}

impl AccountRuntime {
    /// Build (but do not start) a runtime for a connected account.
    pub fn build(
        shared: Arc<Shared>,
        account: &Account,
        token: String,
        telegram_chat_id: i64,
    ) -> Result<Arc<Self>> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let client = TzibburClient::builder()
            .base_url(shared.cfg.tzibbur_base_url.clone())
            .token(token)
            .user_agent(format!(
                "tzibbur-telegram-bridge/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .session_listener(Arc::new(InvalidationHook(cmd_tx.clone())))
            .build()?;
        let dir = shared.cfg.data_dir.join("accounts");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let db_path = dir.join(format!("{}.db", account.id));
        let local: Arc<dyn LocalStore> = Arc::new(SqliteStore::open(&db_path)?);
        let socket = Arc::new(TzibburSocket::new(client.clone())?);
        let sync = SyncEngine::with_parts(
            client.clone(),
            local.clone(),
            socket,
            MemberRefreshPolicy::All,
        );
        sync.set_self_user_id(Some(account.tzibbur_user_id.clone()));
        Ok(Arc::new(Self {
            account_id: account.id,
            telegram_chat_id,
            tzibbur_user_id: account.tzibbur_user_id.clone(),
            shared,
            client,
            local,
            sync,
            settings: RwLock::new(account.settings()),
            cmd_tx,
            cmd_rx: parking_lot::Mutex::new(Some(cmd_rx)),
            task: parking_lot::Mutex::new(None),
            topics_warning_sent: AtomicBool::new(false),
            db_path,
        }))
    }

    pub fn client(&self) -> &TzibburClient {
        &self.client
    }
    pub fn local(&self) -> &Arc<dyn LocalStore> {
        &self.local
    }
    pub fn sync_state(&self) -> SyncState {
        self.sync.sync_state()
    }
    pub fn settings(&self) -> AccountSettings {
        self.settings.read().clone()
    }
    pub fn set_settings(&self, s: AccountSettings) {
        *self.settings.write() = s;
    }

    /// Start the sync engine and the event loop.
    pub fn start(self: &Arc<Self>) {
        let mut task = self.task.lock();
        if task.is_some() {
            return;
        }
        let rx = self.cmd_rx.lock().take().expect("runtime started twice");
        let me = self.clone();
        *task = Some(tokio::spawn(async move { me.run(rx).await }));
    }

    pub async fn stop(&self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        let handle = self.task.lock().take();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h).await;
        }
        self.sync.stop().await;
    }

    /// Stop and delete the local cache (disconnect).
    pub async fn stop_and_wipe(&self) {
        self.stop().await;
        let _ = self.local.clear_all();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.db_path.display(), suffix));
        }
    }

    async fn run(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut events = self.sync.subscribe();
        let mut outbox = self.sync.outbox().subscribe();
        self.sync.start();
        tracing::info!(account = self.account_id, "runtime started");
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::SessionInvalidated) => {
                        tracing::warn!(account = self.account_id, "session invalidated (401)");
                        self.handle_session_invalidated().await;
                        break;
                    }
                    Some(Cmd::Stop) | None => break,
                },
                ev = events.recv() => match ev {
                    Ok(ev) => {
                        if let Err(e) = self.handle_sync_event(ev).await {
                            tracing::warn!(account = self.account_id, error = %e, "sync event handling failed");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(account = self.account_id, missed = n, "lagged; re-reconciling");
                        let _ = self.reconcile_topics().await;
                    }
                    Err(_) => break,
                },
                ob = outbox.recv() => if let Ok(ob) = ob {
                    if let Err(e) = self.handle_outbox_event(ob).await {
                        tracing::warn!(account = self.account_id, error = %e, "outbox event handling failed");
                    }
                },
            }
        }
        tracing::info!(account = self.account_id, "runtime stopped");
    }

    // -----------------------------------------------------------------------
    // Inbound (Tzibbur → Telegram)
    // -----------------------------------------------------------------------

    async fn handle_sync_event(&self, ev: SyncEvent) -> Result<()> {
        match ev {
            SyncEvent::CaughtUp => {
                self.reconcile_topics().await?;
            }
            SyncEvent::NewMessages { group_id, messages } => {
                self.forward_messages(&group_id, messages).await?
            }
            SyncEvent::EchoConfirmed {
                client_message_id,
                server_id,
                seq,
                ..
            } => {
                self.shared
                    .store
                    .confirm_outbound(&client_message_id, &server_id, seq)
                    .await?;
            }
            SyncEvent::GroupChanged { group_id } => {
                if let (Some(conv), Some(g)) = (
                    self.shared
                        .store
                        .conversation_by_group(self.account_id, &group_id)
                        .await?,
                    self.local.get_group(&group_id)?,
                ) {
                    if conv.name.as_deref() != Some(g.name.as_str()) {
                        self.rename_topic(&conv, &g.name).await?;
                    }
                }
            }
            SyncEvent::GroupDeleted { group_id } => {
                if let Some(conv) = self
                    .shared
                    .store
                    .conversation_by_group(self.account_id, &group_id)
                    .await?
                {
                    if !conv.closed {
                        self.send_note(&conv, "🚫 This group was deleted or you were removed from it. The topic is now closed.")
                            .await
                            .ok();
                        if let Some(t) = conv.topic_id() {
                            let _ = self
                                .shared
                                .bot
                                .close_forum_topic(
                                    ChatId(self.telegram_chat_id),
                                    ThreadId(MessageId(t)),
                                )
                                .await;
                        }
                        self.shared
                            .store
                            .set_conversation_closed(conv.id, true)
                            .await?;
                    }
                }
            }
            SyncEvent::UpdateRequired => {
                self.notify_user(
                    "⚠️ Tzibbur says this client version is no longer supported. The bridge needs an update before it can reconnect.",
                    None,
                )
                .await;
            }
            SyncEvent::StateChanged(s) => {
                tracing::debug!(account = self.account_id, ?s, "sync state")
            }
            SyncEvent::MembersChanged { .. } => {}
        }
        Ok(())
    }

    /// Create topics for groups that have none yet and fix names. Called after
    /// every catch-up (initial import and reconnects).
    pub async fn reconcile_topics(&self) -> Result<usize> {
        if !self.settings().auto_topics {
            return Ok(0);
        }
        let mut created = 0;
        for g in self.local.groups()? {
            match self
                .shared
                .store
                .conversation_by_group(self.account_id, &g.id)
                .await?
            {
                Some(conv) => {
                    if conv.name.as_deref() != Some(g.name.as_str()) {
                        self.rename_topic(&conv, &g.name).await.ok();
                    }
                    if conv.closed {
                        self.shared
                            .store
                            .set_conversation_closed(conv.id, false)
                            .await?;
                        if let Some(t) = conv.topic_id() {
                            let _ = self
                                .shared
                                .bot
                                .reopen_forum_topic(
                                    ChatId(self.telegram_chat_id),
                                    ThreadId(MessageId(t)),
                                )
                                .await;
                        }
                    }
                }
                None => {
                    self.ensure_conversation(&g).await?;
                    created += 1;
                }
            }
        }
        Ok(created)
    }

    /// Find or create the topic mapping for a group. On creation, imports the
    /// most recent messages already in the local cache as history.
    pub async fn ensure_conversation(&self, g: &GroupEntity) -> Result<Conversation> {
        if let Some(c) = self
            .shared
            .store
            .conversation_by_group(self.account_id, &g.id)
            .await?
        {
            return Ok(c);
        }
        let topic_id = self.create_topic(&g.name).await;
        // Everything already cached counts as history: import the tail, bookmark the rest.
        let max_seq = self.local.max_seq(&g.id)?.unwrap_or(0);
        // Only messages whose text is still in the cache can be imported; redacted ones are skipped.
        let history: Vec<MessageEntity> = self
            .local
            .thread(&g.id, self.shared.cfg.history_import)?
            .into_iter()
            .filter(|m| !m.body.is_empty())
            .collect();
        let bookmark = history.first().map(|m| m.seq - 1).unwrap_or(max_seq).max(0);
        let conv = self
            .shared
            .store
            .create_conversation(
                self.account_id,
                &g.id,
                self.telegram_chat_id,
                topic_id,
                &g.name,
                g.kind.as_str(),
                bookmark,
            )
            .await?;
        tracing::info!(account = self.account_id, group = %g.id, topic = ?topic_id, "topic mapped");
        if topic_id.is_none() {
            self.warn_topics_unavailable().await;
        }
        if !history.is_empty() {
            self.send_note(
                &conv,
                &format!("🕘 Importing the last {} message(s)…", history.len()),
            )
            .await
            .ok();
            self.forward_to(&conv, history).await?;
        }
        Ok(conv)
    }

    async fn create_topic(&self, name: &str) -> Option<i32> {
        if !self.shared.bot_topics_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let name: String = name.chars().take(128).collect();
        match self
            .shared
            .bot
            .create_forum_topic(ChatId(self.telegram_chat_id), name)
            .await
        {
            Ok(t) => Some(t.thread_id.0 .0),
            Err(e) => {
                tracing::warn!(account = self.account_id, error = %e, "createForumTopic failed; falling back to flat messages");
                None
            }
        }
    }

    async fn warn_topics_unavailable(&self) {
        if self.topics_warning_sent.swap(true, Ordering::Relaxed) {
            return;
        }
        let text = if self.shared.bot_topics_enabled.load(Ordering::Relaxed) {
            "ℹ️ I couldn't create a topic in this chat, so your Tzibbur groups will arrive as regular messages tagged with the group name. To send a message to a group, reply to one of its messages. If Telegram offers “Enable topics” for this chat, turn it on and run /reconnect."
        } else {
            "ℹ️ Topic mode is not enabled for this bot (the bot owner must enable Topics in @BotFather). Your groups will arrive as regular messages tagged with the group name; reply to a message to answer in that group."
        };
        self.notify_user(text, None).await;
    }

    async fn forward_messages(&self, group_id: &str, messages: Vec<MessageEntity>) -> Result<()> {
        let Some(g) = self.local.get_group(group_id)? else {
            return Ok(());
        };
        if !self.settings().auto_topics
            && self
                .shared
                .store
                .conversation_by_group(self.account_id, group_id)
                .await?
                .is_none()
        {
            return Ok(());
        }
        let conv = self.ensure_conversation(&g).await?;
        // `ensure_conversation` may have just imported these as history.
        let conv = self
            .shared
            .store
            .conversation(conv.id)
            .await?
            .unwrap_or(conv);
        self.forward_to(&conv, messages).await
    }

    async fn forward_to(
        &self,
        conv: &Conversation,
        mut messages: Vec<MessageEntity>,
    ) -> Result<()> {
        messages.sort_by_key(|m| m.seq);
        let members = self.local.members(&conv.group_id).unwrap_or_default();
        let settings = self.settings();
        let mut conv = conv.clone();
        let mut max_seq = conv.last_forwarded_seq;
        let mut conv_ref = conv.clone();
        for m in messages {
            if m.seq <= conv.last_forwarded_seq {
                continue;
            }
            if self
                .shared
                .store
                .message_by_tzibbur_id(conv.id, &m.id)
                .await?
                .is_some()
            {
                continue; // idempotent across restarts
            }
            if m.body.is_empty() {
                continue; // already redacted ⇒ already delivered
            }
            let label = format::sender_label(
                &m,
                &members,
                Some(&self.tzibbur_user_id),
                settings.show_phone_numbers,
            );
            let prefix = if conv.topic_id().is_none() {
                conv.name.as_deref()
            } else {
                None
            };
            let text = format::render_inbound(&label, &m.body, prefix);
            for part in format::chunk(&text, 4000) {
                let sent = self.send_to_conv(&conv_ref, &part).await?;
                self.shared
                    .store
                    .record_inbound(conv.id, &m.id, m.seq, sent.id.0)
                    .await?;
                // A missing topic may have been recreated by send_to_conv.
                if let Some(c) = self.shared.store.conversation(conv.id).await? {
                    conv_ref = c;
                }
            }
            max_seq = max_seq.max(m.seq);
        }
        if max_seq > conv.last_forwarded_seq {
            self.shared
                .store
                .advance_forwarded_seq(&conv, max_seq)
                .await?;
            conv.last_forwarded_seq = max_seq;
            // Privacy: once delivered to Telegram, the text has no reason to stay on the server.
            if let Err(e) = self.local.redact_messages(&conv.group_id, max_seq) {
                tracing::warn!(error = %e, "redaction failed");
            }
            if settings.auto_mark_read {
                if let Err(e) = self.sync.mark_read(&conv.group_id, max_seq).await {
                    tracing::debug!(error = %e, "auto mark read failed");
                }
            }
        }
        Ok(())
    }

    /// Send HTML text into a conversation's topic, recreating the topic if it was deleted.
    async fn send_to_conv(&self, conv: &Conversation, html: &str) -> Result<Message> {
        let chat = ChatId(self.telegram_chat_id);
        let mut req = self
            .shared
            .bot
            .send_message(chat, html)
            .parse_mode(ParseMode::Html);
        if let Some(t) = conv.topic_id() {
            req = req.message_thread_id(ThreadId(MessageId(t)));
        }
        match req.await {
            Ok(m) => Ok(m),
            Err(e) if conv.topic_id().is_some() && is_missing_thread(&e) => {
                tracing::warn!(
                    account = self.account_id,
                    conv = conv.id,
                    "topic missing; recreating"
                );
                let new_topic = self
                    .create_topic(conv.name.as_deref().unwrap_or("Tzibbur"))
                    .await;
                self.shared
                    .store
                    .set_conversation_topic(conv.id, new_topic)
                    .await?;
                let mut req = self
                    .shared
                    .bot
                    .send_message(chat, html)
                    .parse_mode(ParseMode::Html);
                if let Some(t) = new_topic {
                    req = req.message_thread_id(ThreadId(MessageId(t)));
                }
                Ok(req.await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn send_note(&self, conv: &Conversation, text: &str) -> Result<()> {
        self.send_to_conv(conv, &format!("<i>{}</i>", format::escape_html(text)))
            .await?;
        Ok(())
    }

    async fn rename_topic(&self, conv: &Conversation, name: &str) -> Result<()> {
        if let Some(t) = conv.topic_id() {
            let short: String = name.chars().take(128).collect();
            self.shared
                .bot
                .edit_forum_topic(ChatId(self.telegram_chat_id), ThreadId(MessageId(t)))
                .name(short)
                .await
                .ok();
        }
        self.shared.store.set_conversation_name(conv.id, name).await
    }

    /// Plain message to the user's private chat (no topic).
    pub async fn notify_user(&self, text: &str, keyboard: Option<InlineKeyboardMarkup>) {
        let mut req = self
            .shared
            .bot
            .send_message(ChatId(self.telegram_chat_id), text);
        if let Some(k) = keyboard {
            req = req.reply_markup(k);
        }
        if let Err(e) = req.await {
            tracing::warn!(account = self.account_id, error = %e, "notify failed");
        }
    }

    async fn handle_session_invalidated(&self) {
        let _ = self
            .shared
            .store
            .set_account_status(self.account_id, AccountStatus::ReauthRequired)
            .await;
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "🔄 Reconnect",
            "reconnect",
        )]]);
        self.notify_user("⚠️ Your Tzibbur session expired or was revoked. Messages are paused until you reconnect.", Some(kb)).await;
        self.sync.stop().await;
    }

    // -----------------------------------------------------------------------
    // User-triggered actions
    // -----------------------------------------------------------------------

    /// Pull-to-refresh equivalent: catch up over REST, reconcile groups, create missing topics.
    pub async fn sync_refresh(&self) -> Result<()> {
        self.sync
            .refresh_now()
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        self.reconcile_topics().await?;
        Ok(())
    }

    pub async fn sync_refresh_members(&self, group_id: &str) -> Result<()> {
        self.sync
            .refresh_members(group_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    /// Leave the group behind a conversation and close its topic.
    pub async fn leave_group(&self, conv: &Conversation) -> Result<()> {
        self.sync
            .leave_group(&conv.group_id)
            .await
            .map_err(anyhow::Error::new)?;
        self.send_note(conv, "🚪 You left this group.").await.ok();
        if let Some(t) = conv.topic_id() {
            let _ = self
                .shared
                .bot
                .close_forum_topic(ChatId(self.telegram_chat_id), ThreadId(MessageId(t)))
                .await;
        }
        self.shared
            .store
            .set_conversation_closed(conv.id, true)
            .await
    }

    // -----------------------------------------------------------------------
    // Outbound (Telegram → Tzibbur)
    // -----------------------------------------------------------------------

    /// Queue a text message for a group. Returns the outbox client id.
    pub async fn send_text(
        &self,
        conv: &Conversation,
        text: &str,
        telegram_message_id: i32,
    ) -> Result<String> {
        let group = self
            .local
            .get_group(&conv.group_id)?
            .ok_or_else(|| anyhow!("group not in local cache yet; try again in a moment"))?;
        if group.kind == GroupKind::System {
            return Err(anyhow!(
                "this is a system announcement thread; nobody can post here"
            ));
        }
        if !group.who_can_post.allows(group.my_role) && group.my_role != Role::Admin {
            return Err(anyhow!("only admins can post in this group"));
        }
        let n = text.chars().count();
        if n > MAX_OUTBOUND_CHARS {
            return Err(anyhow!(
                "message is {n} characters; Tzibbur allows {MAX_OUTBOUND_CHARS}"
            ));
        }
        let row = self
            .sync
            .send_message(&conv.group_id, text)
            .map_err(|e| anyhow!(e.to_string()))?;
        self.shared
            .store
            .record_outbound(conv.id, &row.client_message_id, telegram_message_id)
            .await?;
        Ok(row.client_message_id)
    }

    async fn handle_outbox_event(&self, ev: OutboxEvent) -> Result<()> {
        match ev {
            OutboxEvent::Confirmed {
                client_message_id,
                message,
            } => {
                let _ = self.local.redact_confirmed_outbox();
                let _ = self
                    .local
                    .redact_messages(&message.group_id.clone().unwrap_or_default(), message.seq);
                self.shared
                    .store
                    .confirm_outbound(&client_message_id, &message.id, message.seq)
                    .await?;
                if let Some(map) = self
                    .shared
                    .store
                    .message_by_client_id(&client_message_id)
                    .await?
                {
                    if let Some(tg) = map
                        .telegram_message_id
                        .as_deref()
                        .and_then(|s| s.parse::<i32>().ok())
                    {
                        self.react(tg, "👍").await;
                    }
                }
            }
            OutboxEvent::Rejected {
                client_message_id,
                code,
            } => {
                if let Some(map) = self
                    .shared
                    .store
                    .message_by_client_id(&client_message_id)
                    .await?
                {
                    if let (Some(conv), Some(tg)) = (
                        self.shared.store.conversation(map.conversation).await?,
                        map.telegram_message_id
                            .as_deref()
                            .and_then(|s| s.parse::<i32>().ok()),
                    ) {
                        let reason = match code.as_str() {
                            "forbidden" => "you are not allowed to post in this group".to_owned(),
                            "not-found" => "the group no longer exists".to_owned(),
                            "invalid-message" => format!(
                                "the message was rejected (max {MAX_OUTBOUND_CHARS} characters)"
                            ),
                            "unauthorized" => "your session expired; use /reconnect".to_owned(),
                            other => format!("error `{other}`"),
                        };
                        let mut req = self
                            .shared
                            .bot
                            .send_message(
                                ChatId(self.telegram_chat_id),
                                format!("❌ Not delivered: {reason}."),
                            )
                            .reply_parameters(teloxide::types::ReplyParameters::new(MessageId(tg)));
                        if let Some(t) = conv.topic_id() {
                            req = req.message_thread_id(ThreadId(MessageId(t)));
                        }
                        req.await.ok();
                        self.react(tg, "👎").await;
                    }
                }
            }
            OutboxEvent::Rescheduled { .. } => {}
        }
        Ok(())
    }

    async fn react(&self, telegram_message_id: i32, emoji: &str) {
        use teloxide::types::ReactionType;
        let r = self
            .shared
            .bot
            .set_message_reaction(
                ChatId(self.telegram_chat_id),
                MessageId(telegram_message_id),
            )
            .reaction(vec![ReactionType::Emoji {
                emoji: emoji.to_owned(),
            }])
            .await;
        if let Err(e) = r {
            tracing::debug!(error = %e, "reaction failed (ignored)");
        }
    }
}

fn is_missing_thread(e: &teloxide::RequestError) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("thread not found")
        || s.contains("topic_deleted")
        || s.contains("message thread not found")
        || s.contains("topic not found")
}

// ---------------------------------------------------------------------------
// Startup helpers
// ---------------------------------------------------------------------------

/// Decrypt the stored session and start a runtime for `account`.
pub async fn start_runtime(
    shared: &Arc<Shared>,
    registry: &Registry,
    account: &Account,
) -> Result<Arc<AccountRuntime>> {
    if let Some(existing) = registry.get(account.id) {
        return Ok(existing);
    }
    let blob = account
        .encrypted_session
        .as_deref()
        .ok_or_else(|| anyhow!("account {} has no session", account.id))?;
    let token = shared.cipher.decrypt(blob)?;
    let bridge_user = shared
        .store
        .user(account.user)
        .await?
        .ok_or_else(|| anyhow!("bridge user {} missing", account.user))?;
    let chat_id: i64 = bridge_user
        .telegram_user_id
        .parse()
        .context("telegram_user_id")?;
    let rt = AccountRuntime::build(shared.clone(), account, token, chat_id)?;
    rt.start();
    registry.insert(rt.clone());
    Ok(rt)
}

/// Start runtimes for every connected account in Directus.
pub async fn start_all(shared: &Arc<Shared>, registry: &Registry) -> Result<usize> {
    let accounts = shared.store.connected_accounts().await?;
    let mut n = 0;
    for a in accounts {
        match start_runtime(shared, registry, &a).await {
            Ok(_) => n += 1,
            Err(e) => tracing::error!(account = a.id, error = %e, "could not start runtime"),
        }
    }
    Ok(n)
}

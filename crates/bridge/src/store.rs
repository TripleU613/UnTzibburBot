//! Bridge persistence on Directus: users, connected accounts, conversation
//! (group ↔ topic) mappings and message-id mappings.

use crate::directus::{Directus, Filter, Names};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Directus may return integers as numbers or strings depending on the DB; accept both.
fn de_i64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    match &v {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("not i64")),
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        Value::Null => Ok(0),
        _ => Err(serde::de::Error::custom("bad int")),
    }
}
fn de_opt_i64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Value::deserialize(d)?;
    match &v {
        Value::Number(n) => Ok(n.as_i64()),
        Value::String(s) if !s.is_empty() => s.parse().map(Some).map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}
/// Booleans come back as 0/1 on SQLite-backed Directus.
fn de_bool_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match &v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => matches!(s.as_str(), "1" | "true" | "TRUE"),
        _ => false,
    })
}
/// Relations may come back as the id or as an expanded object.
fn de_fk<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    match &v {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("not i64")),
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        Value::Object(o) => o
            .get("id")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| serde::de::Error::custom("fk object without id")),
        _ => Err(serde::de::Error::custom("bad fk")),
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeUser {
    #[serde(deserialize_with = "de_i64_lenient")]
    pub id: i64,
    pub telegram_user_id: String,
    #[serde(default)]
    pub telegram_username: Option<String>,
    #[serde(default)]
    pub telegram_first_name: Option<String>,
    #[serde(default)]
    pub settings: Option<Value>,
}

impl BridgeUser {
    fn setting_str(&self, key: &str) -> Option<String> {
        self.settings
            .as_ref()?
            .get(key)?
            .as_str()
            .map(str::to_owned)
    }
    /// Preferred language code, if the user chose one.
    pub fn lang(&self) -> Option<String> {
        self.setting_str("lang")
    }
    /// Account used by main-thread commands when the user has several.
    pub fn active_account(&self) -> Option<i64> {
        self.settings.as_ref()?.get("active_account")?.as_i64()
    }

    /// Thread id of the user's "Tzibbur" control topic, if created.
    pub fn home_topic_id(&self) -> Option<i32> {
        self.settings
            .as_ref()
            .and_then(|s| s.get("home_topic_id"))
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|x| x.parse().ok()))
            })
            .map(|v| v as i32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Connected,
    ReauthRequired,
    Disconnected,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Connected => "connected",
            AccountStatus::ReauthRequired => "reauth_required",
            AccountStatus::Disconnected => "disconnected",
        }
    }
}

/// Per-account user preferences (stored as JSON in `settings`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSettings {
    /// Legacy (no longer shown): the server's ack is a delivery ack and is always sent.
    pub auto_mark_read: bool,
    /// Create Telegram topics for new Tzibbur groups automatically.
    pub auto_topics: bool,
    /// Show the sender's phone number when no display name is known.
    pub show_phone_numbers: bool,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            auto_mark_read: false,
            auto_topics: true,
            show_phone_numbers: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(deserialize_with = "de_i64_lenient")]
    pub id: i64,
    #[serde(deserialize_with = "de_fk")]
    pub user: i64,
    pub tzibbur_user_id: String,
    #[serde(default)]
    pub phone_e164: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub encrypted_session: Option<String>,
    pub status: AccountStatus,
    #[serde(default)]
    pub settings: Option<Value>,
}

impl Account {
    pub fn settings(&self) -> AccountSettings {
        self.settings
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    #[serde(deserialize_with = "de_i64_lenient")]
    pub id: i64,
    #[serde(deserialize_with = "de_fk")]
    pub account: i64,
    pub group_id: String,
    pub telegram_chat_id: String,
    /// `None` when topics are unavailable (fallback: plain messages tagged with the group name).
    #[serde(default)]
    pub telegram_topic_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    pub last_forwarded_seq: i64,
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub closed: bool,
}

impl Conversation {
    pub fn topic_id(&self) -> Option<i32> {
        self.telegram_topic_id
            .as_deref()
            .and_then(|s| s.parse().ok())
    }
    pub fn chat_id(&self) -> i64 {
        self.telegram_chat_id.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Tzibbur → Telegram
    In,
    /// Telegram → Tzibbur
    Out,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMap {
    #[serde(deserialize_with = "de_i64_lenient")]
    pub id: i64,
    #[serde(deserialize_with = "de_fk")]
    pub conversation: i64,
    #[serde(default)]
    pub tzibbur_message_id: Option<String>,
    #[serde(default)]
    pub client_message_id: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub seq: Option<i64>,
    #[serde(default)]
    pub telegram_message_id: Option<String>,
    pub direction: Direction,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Store {
    d: Directus,
    n: Names,
}

impl Store {
    pub fn new(d: Directus, names: Names) -> Self {
        Self { d, n: names }
    }

    // ---- users ----

    pub async fn upsert_user(
        &self,
        telegram_user_id: i64,
        username: Option<&str>,
        first_name: Option<&str>,
    ) -> Result<BridgeUser> {
        let tid = telegram_user_id.to_string();
        if let Some(u) = self
            .d
            .first::<BridgeUser>(&self.n.users, &[Filter::eq("telegram_user_id", &tid)])
            .await?
        {
            if u.telegram_username.as_deref() != username
                || u.telegram_first_name.as_deref() != first_name
            {
                let _: Value = self
                    .d
                    .update(&self.n.users, u.id, &json!({"telegram_username": username, "telegram_first_name": first_name, "updated_at": now()}))
                    .await?;
            }
            return Ok(u);
        }
        self.d
            .create(
                &self.n.users,
                &json!({
                    "telegram_user_id": tid, "telegram_username": username, "telegram_first_name": first_name,
                    "settings": {}, "created_at": now(), "updated_at": now()
                }),
            )
            .await
            .context("create user")
    }

    pub async fn users_with_home_topic(&self) -> Result<Vec<BridgeUser>> {
        let all: Vec<BridgeUser> = self.d.list(&self.n.users, &[], None, None).await?;
        Ok(all
            .into_iter()
            .filter(|u| u.home_topic_id().is_some())
            .collect())
    }

    /// Merge one key into the user's settings JSON.
    pub async fn set_user_setting(&self, user_id: i64, key: &str, value: Value) -> Result<()> {
        let current: Option<BridgeUser> = self.d.get(&self.n.users, user_id).await?;
        let mut settings = current
            .and_then(|u| u.settings)
            .unwrap_or_else(|| json!({}));
        if !settings.is_object() {
            settings = json!({});
        }
        settings[key] = value;
        let _: Value = self
            .d
            .update(
                &self.n.users,
                user_id,
                &json!({"settings": settings, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    /// All connected accounts of a user, oldest first.
    pub async fn accounts_for_user(&self, user_id: i64) -> Result<Vec<Account>> {
        let mut list: Vec<Account> = self
            .d
            .list(
                &self.n.accounts,
                &[Filter::eq("user", user_id)],
                None,
                Some("id"),
            )
            .await?;
        list.retain(|a| a.status == AccountStatus::Connected);
        Ok(list)
    }

    pub async fn set_home_topic(&self, user_id: i64, topic_id: Option<i32>) -> Result<()> {
        let current: Option<BridgeUser> = self.d.get(&self.n.users, user_id).await?;
        let mut settings = current
            .and_then(|u| u.settings)
            .unwrap_or_else(|| json!({}));
        if !settings.is_object() {
            settings = json!({});
        }
        settings["home_topic_id"] = match topic_id {
            Some(t) => json!(t),
            None => Value::Null,
        };
        let _: Value = self
            .d
            .update(
                &self.n.users,
                user_id,
                &json!({"settings": settings, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    pub async fn user(&self, id: i64) -> Result<Option<BridgeUser>> {
        self.d.get(&self.n.users, id).await
    }

    pub async fn user_by_telegram_id(&self, telegram_user_id: i64) -> Result<Option<BridgeUser>> {
        self.d
            .first(
                &self.n.users,
                &[Filter::eq("telegram_user_id", telegram_user_id)],
            )
            .await
    }

    // ---- accounts ----

    pub async fn account_for_user(&self, user_id: i64) -> Result<Option<Account>> {
        // One Tzibbur account per Telegram user (multi-account is Phase 3).
        let mut list: Vec<Account> = self
            .d
            .list(
                &self.n.accounts,
                &[Filter::eq("user", user_id)],
                None,
                Some("-id"),
            )
            .await?;
        list.sort_by_key(|a| match a.status {
            AccountStatus::Connected => 0,
            AccountStatus::ReauthRequired => 1,
            AccountStatus::Disconnected => 2,
        });
        Ok(list.into_iter().next())
    }

    pub async fn connected_accounts(&self) -> Result<Vec<Account>> {
        self.d
            .list(
                &self.n.accounts,
                &[Filter::eq("status", "connected")],
                None,
                Some("id"),
            )
            .await
    }

    pub async fn connect_account(
        &self,
        user_id: i64,
        tzibbur_user_id: &str,
        phone: Option<&str>,
        display_name: &str,
        device_id: &str,
        encrypted_session: &str,
    ) -> Result<Account> {
        let existing: Option<Account> = self
            .d
            .first(
                &self.n.accounts,
                &[
                    Filter::eq("user", user_id),
                    Filter::eq("tzibbur_user_id", tzibbur_user_id),
                ],
            )
            .await?;
        let patch = json!({
            "phone_e164": phone, "display_name": display_name, "device_id": device_id,
            "encrypted_session": encrypted_session, "status": "connected",
            "connected_at": now(), "updated_at": now()
        });
        match existing {
            Some(a) => self
                .d
                .update(&self.n.accounts, a.id, &patch)
                .await
                .context("reconnect account"),
            None => {
                let mut body = patch;
                body["user"] = json!(user_id);
                body["tzibbur_user_id"] = json!(tzibbur_user_id);
                body["settings"] = serde_json::to_value(AccountSettings::default())?;
                self.d
                    .create(&self.n.accounts, &body)
                    .await
                    .context("create account")
            }
        }
    }

    pub async fn set_account_status(&self, id: i64, status: AccountStatus) -> Result<()> {
        let mut patch = json!({"status": status.as_str(), "updated_at": now()});
        if status != AccountStatus::Connected {
            patch["encrypted_session"] = Value::Null;
        }
        let _: Value = self.d.update(&self.n.accounts, id, &patch).await?;
        Ok(())
    }

    pub async fn update_account_settings(&self, id: i64, settings: &AccountSettings) -> Result<()> {
        let _: Value = self
            .d
            .update(
                &self.n.accounts,
                id,
                &json!({"settings": settings, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    pub async fn update_account_display_name(&self, id: i64, name: &str) -> Result<()> {
        let _: Value = self
            .d
            .update(
                &self.n.accounts,
                id,
                &json!({"display_name": name, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    /// Drop every conversation and message mapping of an account, keep the account row.
    pub async fn purge_account_mappings(&self, id: i64) -> Result<()> {
        let convs: Vec<Conversation> = self.conversations_for_account(id).await?;
        for c in convs {
            self.d
                .delete_where(&self.n.messages, &[Filter::eq("conversation", c.id)])
                .await?;
        }
        self.d
            .delete_where(&self.n.conversations, &[Filter::eq("account", id)])
            .await
    }

    /// Full deletion: mappings + account row.
    pub async fn purge_account(&self, id: i64) -> Result<()> {
        let convs: Vec<Conversation> = self.conversations_for_account(id).await?;
        for c in convs {
            self.d
                .delete_where(&self.n.messages, &[Filter::eq("conversation", c.id)])
                .await?;
        }
        self.d
            .delete_where(&self.n.conversations, &[Filter::eq("account", id)])
            .await?;
        self.d.delete(&self.n.accounts, id).await
    }

    // ---- conversations ----

    pub async fn conversations_for_account(&self, account_id: i64) -> Result<Vec<Conversation>> {
        self.d
            .list(
                &self.n.conversations,
                &[Filter::eq("account", account_id)],
                None,
                Some("id"),
            )
            .await
    }

    pub async fn conversation_by_group(
        &self,
        account_id: i64,
        group_id: &str,
    ) -> Result<Option<Conversation>> {
        self.d
            .first(
                &self.n.conversations,
                &[
                    Filter::eq("account", account_id),
                    Filter::eq("group_id", group_id),
                ],
            )
            .await
    }

    /// Tenant-safe lookup: chat id AND topic id (never topic alone).
    pub async fn conversation_by_topic(
        &self,
        telegram_chat_id: i64,
        topic_id: i32,
    ) -> Result<Option<Conversation>> {
        self.d
            .first(
                &self.n.conversations,
                &[
                    Filter::eq("telegram_chat_id", telegram_chat_id),
                    Filter::eq("telegram_topic_id", topic_id),
                ],
            )
            .await
    }

    pub async fn conversation(&self, id: i64) -> Result<Option<Conversation>> {
        self.d.get(&self.n.conversations, id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_conversation(
        &self,
        account_id: i64,
        group_id: &str,
        telegram_chat_id: i64,
        topic_id: Option<i32>,
        name: &str,
        kind: &str,
        last_forwarded_seq: i64,
    ) -> Result<Conversation> {
        self.d
            .create(
                &self.n.conversations,
                &json!({
                    "account": account_id, "group_id": group_id, "telegram_chat_id": telegram_chat_id.to_string(),
                    "telegram_topic_id": topic_id.map(|t| t.to_string()), "name": name, "kind": kind,
                    "last_forwarded_seq": last_forwarded_seq, "closed": false, "created_at": now(), "updated_at": now()
                }),
            )
            .await
            .context("create conversation")
    }

    pub async fn set_conversation_topic(&self, id: i64, topic_id: Option<i32>) -> Result<()> {
        let _: Value = self
            .d
            .update(&self.n.conversations, id, &json!({"telegram_topic_id": topic_id.map(|t| t.to_string()), "closed": false, "updated_at": now()}))
            .await?;
        Ok(())
    }

    pub async fn set_conversation_name(&self, id: i64, name: &str) -> Result<()> {
        let _: Value = self
            .d
            .update(
                &self.n.conversations,
                id,
                &json!({"name": name, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    pub async fn set_conversation_closed(&self, id: i64, closed: bool) -> Result<()> {
        let _: Value = self
            .d
            .update(
                &self.n.conversations,
                id,
                &json!({"closed": closed, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    /// Monotonic: only moves forward.
    pub async fn advance_forwarded_seq(&self, conv: &Conversation, seq: i64) -> Result<()> {
        if seq <= conv.last_forwarded_seq {
            return Ok(());
        }
        let _: Value = self
            .d
            .update(
                &self.n.conversations,
                conv.id,
                &json!({"last_forwarded_seq": seq, "updated_at": now()}),
            )
            .await?;
        Ok(())
    }

    /// Row counts for the operator's /stats (no content).
    pub async fn counts(&self) -> Result<(usize, usize, usize, usize)> {
        #[derive(Deserialize)]
        struct Id {
            #[allow(dead_code)]
            id: Value,
        }
        let u: Vec<Id> = self.d.list(&self.n.users, &[], None, None).await?;
        let a: Vec<Id> = self
            .d
            .list(
                &self.n.accounts,
                &[Filter::eq("status", "connected")],
                None,
                None,
            )
            .await?;
        let c: Vec<Id> = self.d.list(&self.n.conversations, &[], None, None).await?;
        let m: Vec<Id> = self.d.list(&self.n.messages, &[], None, None).await?;
        Ok((u.len(), a.len(), c.len(), m.len()))
    }

    // ---- messages ----

    pub async fn record_inbound(
        &self,
        conv_id: i64,
        tzibbur_message_id: &str,
        seq: i64,
        telegram_message_id: i32,
    ) -> Result<()> {
        let _: Value = self
            .d
            .create(
                &self.n.messages,
                &json!({
                    "conversation": conv_id, "tzibbur_message_id": tzibbur_message_id, "seq": seq,
                    "telegram_message_id": telegram_message_id.to_string(), "direction": "in", "created_at": now()
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn record_outbound(
        &self,
        conv_id: i64,
        client_message_id: &str,
        telegram_message_id: i32,
    ) -> Result<MessageMap> {
        self.d
            .create(
                &self.n.messages,
                &json!({
                    "conversation": conv_id, "client_message_id": client_message_id,
                    "telegram_message_id": telegram_message_id.to_string(), "direction": "out", "created_at": now()
                }),
            )
            .await
            .context("record outbound")
    }

    pub async fn confirm_outbound(
        &self,
        client_message_id: &str,
        tzibbur_message_id: &str,
        seq: i64,
    ) -> Result<()> {
        if let Some(m) = self
            .d
            .first::<MessageMap>(
                &self.n.messages,
                &[Filter::eq("client_message_id", client_message_id)],
            )
            .await?
        {
            let _: Value = self
                .d
                .update(
                    &self.n.messages,
                    m.id,
                    &json!({"tzibbur_message_id": tzibbur_message_id, "seq": seq}),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn message_by_tzibbur_id(
        &self,
        conv_id: i64,
        tzibbur_message_id: &str,
    ) -> Result<Option<MessageMap>> {
        self.d
            .first(
                &self.n.messages,
                &[
                    Filter::eq("conversation", conv_id),
                    Filter::eq("tzibbur_message_id", tzibbur_message_id),
                ],
            )
            .await
    }

    /// Resolve a Telegram message (in this chat) back to its conversation — used to
    /// route replies when topics are unavailable.
    pub async fn conversation_by_telegram_message(
        &self,
        telegram_chat_id: i64,
        telegram_message_id: i32,
    ) -> Result<Option<Conversation>> {
        let maps: Vec<MessageMap> = self
            .d
            .list(
                &self.n.messages,
                &[Filter::eq("telegram_message_id", telegram_message_id)],
                Some(10),
                None,
            )
            .await?;
        for m in maps {
            if let Some(c) = self.conversation(m.conversation).await? {
                if c.chat_id() == telegram_chat_id {
                    return Ok(Some(c));
                }
            }
        }
        Ok(None)
    }

    pub async fn message_by_client_id(
        &self,
        client_message_id: &str,
    ) -> Result<Option<MessageMap>> {
        self.d
            .first(
                &self.n.messages,
                &[Filter::eq("client_message_id", client_message_id)],
            )
            .await
    }
}

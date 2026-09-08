//! Process-wide state shared by Telegram handlers, the Mini App server and runtimes.

use crate::bridge::{start_runtime, AccountRuntime, Registry, Shared};
use crate::store::{Account, AccountStatus, BridgeUser};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use teloxide::types::User as TgUser;
use tzibbur_api::models::Session;

pub struct App {
    pub shared: Arc<Shared>,
    pub registry: Registry,
    pub bot_username: String,
    /// Last activity per chat in a multi-step flow; flows go stale after 10 minutes.
    pub dialogue_activity: dashmap::DashMap<i64, std::time::Instant>,
    /// Over-limit messages waiting for the user to confirm splitting: (chat, message id) -> (conv id, text, when).
    pub pending_splits: dashmap::DashMap<(i64, i32), (i64, String, std::time::Instant)>,
    pub started_at: std::time::Instant,
    /// Sign-in attempts per Telegram user (sliding one-hour window), to slow down OTP abuse.
    pub auth_attempts: dashmap::DashMap<(i64, AuthStep), Vec<std::time::Instant>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AuthStep {
    /// Requesting a code (sends an SMS to the phone).
    Start,
    /// Submitting a code.
    Verify,
}

impl AuthStep {
    fn limit_per_hour(self) -> usize {
        match self {
            AuthStep::Start => 5,
            AuthStep::Verify => 15,
        }
    }
}

/// A multi-step flow (sign-in, new group, rename) is abandoned after this long.
pub const DIALOGUE_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl App {
    /// Count an attempt and report whether this Telegram user is still within the hourly limit.
    pub fn allow_auth(&self, telegram_user_id: i64, step: AuthStep) -> bool {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(3600);
        let mut entry = self
            .auth_attempts
            .entry((telegram_user_id, step))
            .or_default();
        entry.retain(|t| now.duration_since(*t) < window);
        if entry.len() >= step.limit_per_hour() {
            return false;
        }
        entry.push(now);
        true
    }

    /// Record activity for a chat's flow and report whether the flow is still fresh.
    pub fn touch_dialogue(&self, chat_id: i64) -> bool {
        let now = std::time::Instant::now();
        let fresh = self
            .dialogue_activity
            .get(&chat_id)
            .map(|t| now.duration_since(*t) < DIALOGUE_TTL)
            .unwrap_or(true);
        self.dialogue_activity.insert(chat_id, now);
        fresh
    }
    pub fn clear_dialogue(&self, chat_id: i64) {
        self.dialogue_activity.remove(&chat_id);
    }
}

impl App {
    /// The bridge user row for a Telegram user, created on first contact.
    pub async fn bridge_user(&self, tg: &TgUser) -> Result<BridgeUser> {
        self.shared
            .store
            .upsert_user(
                tg.id.0 as i64,
                tg.username.as_deref(),
                Some(tg.first_name.as_str()),
            )
            .await
    }

    /// The account main-thread commands act on: the user's chosen active account if it is
    /// connected, else the single/most relevant one.
    pub async fn account_for(&self, tg_user_id: i64) -> Result<Option<Account>> {
        let Some(u) = self.shared.store.user_by_telegram_id(tg_user_id).await? else {
            return Ok(None);
        };
        let connected = self.shared.store.accounts_for_user(u.id).await?;
        if let Some(active) = u.active_account() {
            if let Some(a) = connected.iter().find(|a| a.id == active) {
                return Ok(Some(a.clone()));
            }
        }
        if let Some(a) = connected.into_iter().next() {
            return Ok(Some(a));
        }
        self.shared.store.account_for_user(u.id).await
    }

    /// All connected accounts of a Telegram user.
    pub async fn accounts_for(&self, tg_user_id: i64) -> Result<Vec<Account>> {
        let Some(u) = self.shared.store.user_by_telegram_id(tg_user_id).await? else {
            return Ok(vec![]);
        };
        self.shared.store.accounts_for_user(u.id).await
    }

    /// Language for a Telegram user: their setting, else their Telegram client language.
    pub async fn lang_for(&self, tg: &TgUser) -> crate::i18n::Lang {
        let stored = self
            .shared
            .store
            .user_by_telegram_id(tg.id.0 as i64)
            .await
            .ok()
            .flatten()
            .and_then(|u| u.lang());
        crate::i18n::Lang::from_code(stored.as_deref().or(tg.language_code.as_deref()))
    }

    /// Persist a freshly verified Tzibbur session (encrypted) and start its runtime.
    pub async fn connect(
        &self,
        tg: &TgUser,
        session: Session,
        add: bool,
    ) -> Result<(Account, Arc<AccountRuntime>)> {
        let user = self.bridge_user(tg).await?;
        // Replace mode: stop and retire the previous account (unless it is the same identity).
        // Add mode: keep existing accounts running.
        let prev = self.shared.store.account_for_user(user.id).await?;
        if let Some(prev) = prev.filter(|p| !add || p.tzibbur_user_id == session.user.id) {
            self.registry.remove(prev.id).await;
            if prev.tzibbur_user_id != session.user.id {
                // Different Tzibbur identity: the old account's topics would otherwise linger in
                // this chat next to the new ones. Remove them and their mappings.
                let removed = crate::bridge::retire_account_topics(&self.shared, prev.id)
                    .await
                    .unwrap_or(0);
                tracing::info!(
                    account = prev.id,
                    removed,
                    "retired topics of the previous account"
                );
                self.shared
                    .store
                    .set_account_status(prev.id, AccountStatus::Disconnected)
                    .await?;
            }
        }
        let blob = self
            .shared
            .cipher
            .encrypt(&session.token, &session.user.id)?;
        let account = self
            .shared
            .store
            .connect_account(
                user.id,
                &session.user.id,
                session.user.phone_e164.as_deref(),
                &session.user.display_name,
                &session.device.id,
                &blob,
            )
            .await?;
        let rt = start_runtime(&self.shared, &self.registry, &account).await?;
        self.shared
            .store
            .set_user_setting(user.id, "active_account", serde_json::json!(account.id))
            .await
            .ok();
        Ok((account, rt))
    }

    /// Stop forwarding and drop the session. Mappings are kept unless `purge`.
    pub async fn disconnect(&self, account: &Account, purge: bool) -> Result<()> {
        if let Some(rt) = self.registry.get(account.id) {
            rt.stop_and_wipe().await;
            self.registry.remove(account.id).await;
        }
        if purge {
            self.shared.store.purge_account(account.id).await
        } else {
            self.shared
                .store
                .set_account_status(account.id, AccountStatus::Disconnected)
                .await
        }
    }

    /// (account id, sync state) for every running runtime.
    pub fn accounts_for_all_running(&self) -> Vec<(i64, tzibbur_api::SyncState)> {
        self.registry
            .snapshot()
            .into_iter()
            .map(|rt| (rt.account_id, rt.sync_state()))
            .collect()
    }

    pub fn runtime(&self, account_id: i64) -> Result<Arc<AccountRuntime>> {
        self.registry
            .get(account_id)
            .ok_or_else(|| anyhow!("account {account_id} is not running"))
    }
}

#[cfg(test)]
mod auth_limit_tests {
    use super::AuthStep;

    #[test]
    fn limits_are_sane() {
        assert!(AuthStep::Start.limit_per_hour() < AuthStep::Verify.limit_per_hour());
    }
}

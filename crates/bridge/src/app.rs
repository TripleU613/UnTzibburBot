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

    pub async fn account_for(&self, tg_user_id: i64) -> Result<Option<Account>> {
        let Some(u) = self.shared.store.user_by_telegram_id(tg_user_id).await? else {
            return Ok(None);
        };
        self.shared.store.account_for_user(u.id).await
    }

    /// Persist a freshly verified Tzibbur session (encrypted) and start its runtime.
    pub async fn connect(
        &self,
        tg: &TgUser,
        session: Session,
    ) -> Result<(Account, Arc<AccountRuntime>)> {
        let user = self.bridge_user(tg).await?;
        // One account per Telegram user: stop any previous runtime first.
        if let Some(prev) = self.shared.store.account_for_user(user.id).await? {
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
        let blob = self.shared.cipher.encrypt(&session.token)?;
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

    pub fn runtime(&self, account_id: i64) -> Result<Arc<AccountRuntime>> {
        self.registry
            .get(account_id)
            .ok_or_else(|| anyhow!("account {account_id} is not running"))
    }
}

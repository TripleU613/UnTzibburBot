//! Environment-driven configuration.

use anyhow::{bail, Context, Result};
use base64::Engine;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// Telegram bot token (`TELOXIDE_TOKEN`).
    pub telegram_token: String,
    /// Directus base URL (`DIRECTUS_URL`), e.g. `http://directus:8055`.
    pub directus_url: url::Url,
    /// Directus static token (`DIRECTUS_TOKEN`) with admin rights (schema bootstrap + items).
    pub directus_token: String,
    /// 32-byte key, base64 (`BRIDGE_MASTER_KEY`), used to encrypt Tzibbur sessions at rest.
    pub master_key: [u8; 32],
    /// Tzibbur API base (`TZIBBUR_BASE_URL`), default production.
    pub tzibbur_base_url: String,
    /// Directory for per-account SQLite caches (`BRIDGE_DATA_DIR`).
    pub data_dir: PathBuf,
    /// If set, receive Telegram updates via webhook at this public HTTPS URL (`BRIDGE_PUBLIC_URL`);
    /// otherwise long-poll.
    pub public_url: Option<url::Url>,
    /// Bind address for the webhook/health server (`BRIDGE_LISTEN`), default `0.0.0.0:8080`.
    pub listen: std::net::SocketAddr,
    /// How many recent messages per group to import when a topic is first created (`BRIDGE_HISTORY_IMPORT`).
    pub history_import: u32,
    /// Prefix for Directus collections (`BRIDGE_COLLECTION_PREFIX`), default `bridge_`.
    pub collection_prefix: String,
    /// Country assumed for phone numbers typed without a country code (`BRIDGE_DEFAULT_REGION`), default `US`.
    pub default_region: String,
    /// How the bridge presents itself to Tzibbur (`TZIBBUR_PLATFORM`, `TZIBBUR_DEVICE_MODEL`,
    /// `TZIBBUR_APP_VERSION`, `TZIBBUR_OS_VERSION`); defaults to the Android app on a Pixel 7.
    pub device: tzibbur_api::DeviceInfo,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let telegram_token = env("TELOXIDE_TOKEN")
            .or_else(|| env("TELEGRAM_BOT_TOKEN"))
            .context("TELOXIDE_TOKEN is required")?;
        let directus_url: url::Url = env("DIRECTUS_URL")
            .unwrap_or_else(|| "http://localhost:8055".into())
            .parse()
            .context("DIRECTUS_URL is not a valid URL")?;
        let directus_token = env("DIRECTUS_TOKEN").context("DIRECTUS_TOKEN is required")?;
        let key_b64 = env("BRIDGE_MASTER_KEY").context(
            "BRIDGE_MASTER_KEY is required (32 random bytes, base64). Generate with: openssl rand -base64 32",
        )?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(key_b64.as_bytes())
            .context("BRIDGE_MASTER_KEY is not valid base64")?;
        if key.len() != 32 {
            bail!(
                "BRIDGE_MASTER_KEY must decode to exactly 32 bytes, got {}",
                key.len()
            );
        }
        let mut master_key = [0u8; 32];
        master_key.copy_from_slice(&key);
        let public_url = match env("BRIDGE_PUBLIC_URL") {
            Some(u) => Some(
                u.parse::<url::Url>()
                    .context("BRIDGE_PUBLIC_URL is not a valid URL")?,
            ),
            None => None,
        };
        Ok(Config {
            telegram_token,
            directus_url,
            directus_token,
            master_key,
            tzibbur_base_url: env("TZIBBUR_BASE_URL")
                .unwrap_or_else(|| tzibbur_api::constants::DEFAULT_BASE_URL.into()),
            data_dir: env("BRIDGE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./data")),
            public_url,
            listen: env("BRIDGE_LISTEN")
                .unwrap_or_else(|| "0.0.0.0:8080".into())
                .parse()
                .context("BRIDGE_LISTEN")?,
            history_import: env("BRIDGE_HISTORY_IMPORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            collection_prefix: env("BRIDGE_COLLECTION_PREFIX").unwrap_or_else(|| "bridge_".into()),
            default_region: env("BRIDGE_DEFAULT_REGION").unwrap_or_else(|| "US".into()),
            device: {
                let d = tzibbur_api::DeviceInfo::android();
                tzibbur_api::DeviceInfo {
                    platform: env("TZIBBUR_PLATFORM").unwrap_or(d.platform),
                    model: env("TZIBBUR_DEVICE_MODEL").unwrap_or(d.model),
                    app_version: env("TZIBBUR_APP_VERSION").unwrap_or(d.app_version),
                    os_version: env("TZIBBUR_OS_VERSION").unwrap_or(d.os_version),
                }
            },
        })
    }
}

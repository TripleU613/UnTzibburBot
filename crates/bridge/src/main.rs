//! Tzibbur ↔ Telegram bridge.
//!
//! One public Telegram bot; each Telegram user connects their own Tzibbur
//! account; every Tzibbur group becomes a topic in the user's private chat
//! with the bot. Directus holds the mappings; per-account SQLite caches hold
//! the Tzibbur sync state.

mod app;
mod bridge;
mod config;
mod crypto;
mod directus;
mod miniapp;
mod phone;
mod store;
mod telegram;

use anyhow::{Context, Result};
use app::App;
use bridge::{Registry, Shared};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;
use teloxide::update_listeners::webhooks;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tzibbur_api=info,teloxide=warn,hyper=warn"));
    if std::env::var("LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false)
    {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    let cfg = config::Config::from_env()?;
    tracing::info!(directus = %cfg.directus_url, tzibbur = %cfg.tzibbur_base_url, data_dir = %cfg.data_dir.display(), "starting bridge");
    std::fs::create_dir_all(&cfg.data_dir).context("create data dir")?;

    // Directus: wait until reachable, then make sure the collections exist.
    let directus = directus::Directus::new(cfg.directus_url.clone(), cfg.directus_token.clone())?;
    wait_for_directus(&directus).await?;
    directus
        .ensure_schema(&cfg.collection_prefix)
        .await
        .context("directus schema bootstrap")?;
    let store = store::Store::new(directus, directus::collection_names(&cfg.collection_prefix));

    // Telegram.
    let bot = Bot::new(&cfg.telegram_token).throttle(Limits::default());
    let me = bot
        .get_me()
        .await
        .context("telegram getMe (bad TELOXIDE_TOKEN?)")?;
    let topics_enabled = raw_has_topics_enabled(&cfg.telegram_token).await;
    tracing::info!(bot = %me.username(), topics_enabled, "telegram ready");
    if !topics_enabled {
        tracing::warn!("Topic mode is OFF for this bot. Enable it in @BotFather → Bot Settings → Threaded Mode, or groups arrive as flat messages.");
    }

    let shared = Arc::new(Shared {
        cfg: cfg.clone(),
        store,
        cipher: crypto::SessionCipher::new(cfg.master_key),
        bot: bot.clone(),
        bot_topics_enabled: AtomicBool::new(topics_enabled),
    });
    let app = Arc::new(App {
        shared: shared.clone(),
        registry: Registry::default(),
        bot_username: me.username().to_owned(),
    });

    telegram::setup_bot_profile(&bot, &app)
        .await
        .context("set bot commands")?;
    let started = bridge::start_all(&shared, &app.registry).await?;
    tracing::info!(accounts = started, "runtimes started");

    // HTTP: health (+ Mini App) and, in webhook mode, Telegram updates.
    let router = miniapp::router(app.clone());
    let mut dispatcher = Dispatcher::builder(bot.clone(), telegram::schema())
        .dependencies(telegram::deps(app.clone()))
        .default_handler(|_| async {})
        .error_handler(LoggingErrorHandler::with_custom_text("handler error"))
        .enable_ctrlc_handler()
        .build();

    match &cfg.public_url {
        Some(public) => {
            let mut hook = public.clone();
            hook.set_path(&format!(
                "{}/telegram/webhook",
                public.path().trim_end_matches('/')
            ));
            let (listener, stop_flag, tg_router) = webhooks::axum_to_router(
                bot.clone(),
                webhooks::Options::new(cfg.listen, hook.clone()),
            )
            .await?;
            let router = router.merge(tg_router);
            let listen = cfg.listen;
            tokio::spawn(async move {
                let l = tokio::net::TcpListener::bind(listen).await.expect("bind");
                axum::serve(l, router)
                    .with_graceful_shutdown(stop_flag)
                    .await
                    .expect("http server");
            });
            tracing::info!(%hook, "webhook mode");
            dispatcher
                .dispatch_with_listener(
                    listener,
                    LoggingErrorHandler::with_custom_text("update listener error"),
                )
                .await;
        }
        None => {
            let listen = cfg.listen;
            tokio::spawn(async move {
                let l = tokio::net::TcpListener::bind(listen).await.expect("bind");
                tracing::info!(%listen, "health/miniapp server (long-polling mode)");
                axum::serve(l, router).await.expect("http server");
            });
            dispatcher.dispatch().await;
        }
    }

    tracing::info!("shutting down runtimes");
    app.registry.stop_all().await;
    Ok(())
}

async fn wait_for_directus(d: &directus::Directus) -> Result<()> {
    let mut delay = std::time::Duration::from_secs(1);
    for attempt in 1..=30 {
        match d.ping().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "directus not ready yet");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(15));
            }
        }
    }
    anyhow::bail!("Directus did not become reachable")
}

/// teloxide's `User` predates `has_topics_enabled`; read it from the raw getMe JSON.
async fn raw_has_topics_enabled(token: &str) -> bool {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    match reqwest::get(&url).await.and_then(|r| r.error_for_status()) {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["result"]["has_topics_enabled"].as_bool())
            .unwrap_or(false),
        Err(_) => false,
    }
}

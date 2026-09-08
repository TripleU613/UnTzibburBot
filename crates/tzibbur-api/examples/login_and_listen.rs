//! End-to-end demo: phone login (OTP), then stream live messages.
//!
//! ```sh
//! TZIBBUR_PHONE=+972501234567 TZIBBUR_NAME="Bridge" cargo run --example login_and_listen
//! ```
//! Set `TZIBBUR_BASE_URL` to point at a different server.

use std::io::{BufRead, Write};
use std::sync::Arc;
use tzibbur_api::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tzibbur_api=debug".parse().unwrap()),
        )
        .init();

    let data_dir = std::env::var("TZIBBUR_DATA_DIR").unwrap_or_else(|_| ".tzibbur".into());
    std::fs::create_dir_all(&data_dir).ok();

    let local: Arc<dyn LocalStore> = Arc::new(SqliteStore::open(format!("{data_dir}/local.db"))?);
    let session_store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::new(
        format!("{data_dir}/session.json"),
        Arc::new(AesGcmCipher::from_secret(
            std::env::var("TZIBBUR_SECRET")
                .unwrap_or_else(|_| "dev-secret-change-me".into())
                .as_bytes(),
        )),
    ));

    // Client + session manager wired so a 401 wipes everything.
    let listener = LateBoundListener::new();
    let mut builder = TzibburClient::builder().session_listener(listener.clone());
    if let Ok(base) = std::env::var("TZIBBUR_BASE_URL") {
        builder = builder.base_url(base);
    }
    let client = builder.build()?;
    let manager = SessionManager::new(client.clone(), session_store, local.clone());
    listener.bind(&manager);

    let sync = SyncEngine::with_parts(
        client.clone(),
        local.clone(),
        Arc::new(TzibburSocket::new(client.clone())?),
        MemberRefreshPolicy::All,
    );
    manager.attach_sync(sync.clone());

    // Sign in if needed.
    if !manager.load().await?.is_signed_in() {
        let phone = std::env::var("TZIBBUR_PHONE").unwrap_or_else(|_| prompt("Phone (E.164): "));
        let name = std::env::var("TZIBBUR_NAME").ok();
        let challenge = manager.start_auth(&phone, name.as_deref(), None).await?;
        println!(
            "OTP sent (challenge {}), resend after {:?}s",
            challenge.challenge_id, challenge.resend_after_seconds
        );
        let code = prompt("Code: ");
        let session = manager
            .verify_auth(&VerifyAuthRequest {
                challenge_id: challenge.challenge_id,
                code,
                phone,
                display_name: name,
                region: None,
                ..Default::default()
            })
            .await?;
        println!(
            "Signed in as {} ({})",
            session.user.display_name, session.user.id
        );
    }

    let me = client.me().await?;
    println!(
        "Hello {} — {} groups",
        me.display_name,
        client.list_all_groups().await?.len()
    );

    let mut events = sync.subscribe();
    sync.start();
    loop {
        match events.recv().await {
            Ok(SyncEvent::NewMessages { group_id, messages }) => {
                let group = local
                    .get_group(&group_id)?
                    .map(|g| g.name)
                    .unwrap_or(group_id.clone());
                for m in messages {
                    println!("[{group}] {}: {}", m.sender_id, m.body);
                }
            }
            Ok(SyncEvent::StateChanged(s)) => println!("sync state: {s:?}"),
            Ok(SyncEvent::UpdateRequired) => {
                eprintln!("server demands a client update; stopping");
                break;
            }
            Ok(other) => tracing::debug!(?other, "event"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    sync.stop().await;
    Ok(())
}

fn prompt(label: &str) -> String {
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).ok();
    s.trim().to_owned()
}

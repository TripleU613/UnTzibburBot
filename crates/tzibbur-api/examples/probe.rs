//! Read-only probe of the live API with an existing token.
//! `TZIBBUR_TOKEN=... cargo run --example probe`

use std::sync::Arc;
use std::time::Duration;
use tzibbur_api::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tzibbur_api=info".parse().unwrap()),
        )
        .init();
    let token = std::env::var("TZIBBUR_TOKEN").expect("TZIBBUR_TOKEN");
    let mut b = TzibburClient::builder().token(token);
    if let Ok(base) = std::env::var("TZIBBUR_BASE_URL") {
        b = b.base_url(base);
    }
    let client = b.build()?;

    println!("== GET /v1/me");
    match client.me().await {
        Ok(u) => println!("{u:#?}"),
        Err(e) => println!("ERR {e}"),
    }
    println!("== GET /v1/me/devices");
    match client.devices().await {
        Ok(d) => println!("{} device(s): {d:#?}", d.len()),
        Err(e) => println!("ERR {e}"),
    }
    println!("== GET /v1/groups/categories");
    match client.group_categories().await {
        Ok(c) => println!("{:?}", c.categories),
        Err(e) => println!("ERR {e}"),
    }
    println!("== GET /v1/groups");
    let groups = match client.list_all_groups().await {
        Ok(g) => {
            for x in &g {
                println!(
                    "- {} | {} | kind={:?} role={:?} members={} post={} add={} extra={}",
                    x.id,
                    x.name,
                    x.kind,
                    x.my_role,
                    x.member_count,
                    x.who_can_post,
                    x.who_can_add_members,
                    serde_json::to_string(&x.extra).unwrap_or_default()
                );
            }
            g
        }
        Err(e) => {
            println!("ERR {e}");
            vec![]
        }
    };
    if let Some(g) = groups.first() {
        println!("== GET /v1/groups/{}/members", g.id);
        match client.list_all_members(&g.id).await {
            Ok(m) => {
                for x in &m {
                    println!(
                        "- {} | {} | {:?} | {:?}",
                        x.user_id, x.display_name, x.role, x.phone_e164
                    );
                }
            }
            Err(e) => println!("ERR {e}"),
        }
        println!("== GET /v1/groups/{}/messages?limit=5", g.id);
        match client
            .get_messages(&g.id, &MessagesQuery::default().limit(5))
            .await
        {
            Ok(m) => {
                for x in &m {
                    println!(
                        "- seq={} {} : {} (created={:?}, extra={})",
                        x.seq,
                        x.sender_id,
                        x.body,
                        x.created_at,
                        serde_json::to_string(&x.extra).unwrap_or_default()
                    );
                }
            }
            Err(e) => println!("ERR {e}"),
        }
    }
    println!("== GET /v1/pending");
    match client.pending(Some(20)).await {
        Ok(p) => println!(
            "{} message(s), {} group(s), {} event(s), extra={}",
            p.message_count(),
            p.groups.len(),
            p.events.len(),
            serde_json::to_string(&p.extra).unwrap_or_default()
        ),
        Err(e) => println!("ERR {e}"),
    }
    println!("== GET /v1/legal/terms");
    match client.legal(LegalDocKey::Terms).await {
        Ok(d) => println!(
            "checksum={} len={} extra={}",
            d.checksum,
            d.markdown.len(),
            serde_json::to_string(&d.extra).unwrap_or_default()
        ),
        Err(e) => println!("ERR {e}"),
    }

    println!("== WS connect (8s)");
    let sock = Arc::new(TzibburSocket::new(client.clone())?);
    let mut ev = sock.subscribe();
    sock.start();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            e = ev.recv() => match e {
                Ok(SocketEvent::Messages { group_id, messages }) => println!("ws messages for {group_id}: {}", messages.len()),
                Ok(e) => println!("ws event: {e:?}"),
                Err(_) => break,
            }
        }
    }
    println!("ws state: {:?}", sock.state());
    sock.stop().await;
    Ok(())
}

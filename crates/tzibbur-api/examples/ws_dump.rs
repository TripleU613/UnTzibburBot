//! Dump raw WebSocket frames for a while. `TZIBBUR_TOKEN=... cargo run -p tzibbur-api --example ws_dump [seconds]`
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let token = std::env::var("TZIBBUR_TOKEN").expect("TZIBBUR_TOKEN");
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let mut req = "wss://api.tzibbur.me/v1/ws".into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    req.headers_mut()
        .insert("X-Platform", "android".parse().unwrap());
    let (ws, resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    println!("handshake {}", resp.status());
    let (mut sink, mut stream) = ws.split();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut last_ping = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::time::sleep(Duration::from_secs(25)) , if last_ping.elapsed() > Duration::from_secs(25) => {
                let _ = sink.send(Message::Text(r#"{"type":"ping"}"#.to_owned())).await; last_ping = tokio::time::Instant::now(); println!("-> ping");
            }
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => println!("{} <- {}", chrono::Utc::now().format("%H:%M:%S"), t.chars().take(600).collect::<String>()),
                Some(Ok(Message::Ping(_))) => { let _ = sink.send(Message::Pong(vec![])).await; println!("<- ws-ping (ponged)"); }
                Some(Ok(Message::Close(c))) => { println!("<- close {c:?}"); break; }
                Some(Ok(other)) => println!("<- {other:?}"),
                Some(Err(e)) => { println!("!! {e}"); break; }
                None => { println!("<- eof"); break; }
            }
        }
    }
}

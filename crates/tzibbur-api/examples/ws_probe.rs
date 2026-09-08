//! Raw WS probe with configurable headers and an optional ack.
//! `TZIBBUR_TOKEN=.. EXTRA_HEADERS="K: V|K: V" cargo run --example ws_probe -- <secs> [ack:GROUP:SEQ]`
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
        .unwrap_or(8);
    let ack = std::env::args().nth(2).and_then(|a| {
        let p: Vec<&str> = a.split(':').collect();
        (p.len() == 3 && p[0] == "ack").then(|| (p[1].to_owned(), p[2].to_owned()))
    });
    let mut req = "wss://api.tzibbur.me/v1/ws".into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    if let Ok(extra) = std::env::var("EXTRA_HEADERS") {
        for kv in extra.split('|') {
            if let Some((k, v)) = kv.split_once(':') {
                let name = http::header::HeaderName::from_bytes(k.trim().as_bytes()).unwrap();
                req.headers_mut().insert(name, v.trim().parse().unwrap());
                println!("hdr {}: {}", k.trim(), v.trim());
            }
        }
    }
    let (ws, resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    println!("handshake {}", resp.status());
    let (mut sink, mut stream) = ws.split();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut acked = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let ty = t.split("\"type\":\"").nth(1).unwrap_or("").split('"').next().unwrap_or("").to_owned();
                    let seqs: Vec<&str> = t.match_indices("\"seq\":").map(|(i, _)| t[i + 6..].split(|c: char| !c.is_ascii_digit()).next().unwrap_or("")).collect();
                    println!("<- {ty} seqs={seqs:?}");
                    if !acked { if let Some((g, s)) = &ack { let f = format!(r#"{{"type":"ack","groupId":"{g}","seq":{s}}}"#); sink.send(Message::Text(f.clone().into())).await.unwrap(); println!("-> {f}"); acked = true; } }
                }
                Some(Ok(Message::Ping(_))) => { let _ = sink.send(Message::Pong(vec![].into())).await; }
                Some(Ok(Message::Close(c))) => { println!("<- close {c:?}"); break; }
                Some(Err(e)) => { println!("!! {e}"); break; }
                _ => {}
            }
        }
    }
}

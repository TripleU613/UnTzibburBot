//! Re-encrypt session blobs from one BRIDGE_MASTER_KEY to another (data migration helper).
//! `OLD_KEY=<b64> NEW_KEY=<b64> cargo run -p tzibbur-api --example recrypt < blobs.txt`
use base64::Engine;
use std::io::BufRead;
use tzibbur_api::session::{AesGcmCipher, SecretCipher};
fn key(name: &str) -> [u8; 32] {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(std::env::var(name).expect(name))
        .expect("b64");
    let mut k = [0u8; 32];
    k.copy_from_slice(&raw);
    k
}
fn main() {
    let old = AesGcmCipher::new(key("OLD_KEY"));
    let new = AesGcmCipher::new(key("NEW_KEY"));
    let b64 = base64::engine::general_purpose::STANDARD;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let plain = old
            .decrypt(&b64.decode(line).expect("b64"))
            .expect("decrypt with OLD_KEY");
        println!("{}", b64.encode(new.encrypt(&plain).unwrap()));
    }
}

# UnTzibburBot — Tzibbur ↔ Telegram bridge

Pure-Rust workspace, shipped as one Docker image, with Directus as the database.

| Crate | What |
|---|---|
| [`crates/bridge`](crates/bridge/README.md) | The Telegram bot (`teloxide`). Each user's Tzibbur groups become topics in their private chat with the bot; messages flow both ways. Directus stores users, accounts (encrypted session), topic and message mappings. Mini App login, Telegram Stars, Docker Compose stack. |
| [`crates/tzibbur-api`](#tzibbur-api) | Client library for the Tzibbur service (REST, WebSocket, SQLite cache, sync engine, outbox), reverse-engineered from the Android app and verified against the live server. |

```sh
cp .env.example .env          # bot token, Directus secrets, BRIDGE_MASTER_KEY
docker compose up -d --build  # postgres + directus + bridge
```

See [`crates/bridge/README.md`](crates/bridge/README.md) for commands, configuration and the architecture.
The original design notes are in `tzibbur-re.md` (protocol) and the architecture doc this replaces Cloudflare Workers/D1/Durable Objects with a single Rust process + Directus.

---

# tzibbur-api

Rust client for the **Tzibbur** group-messaging service, reconstructed from the
reverse-engineered Android app (`com.tzibbur.app` 0.1.0, see `tzibbur-re.md`).
It covers every layer the reference describes:

| Reference section | Module | What's there |
|---|---|---|
| REST API | `http` | `TzibburClient` with all 26 endpoints (auth, me, devices, contacts, legal, groups, categories, members, messages, ack, pending) |
| WebSocket Protocol | `ws` | `TzibburSocket`: protocol v1 frames (`ping`/`ack` out, `hello`/`pong`/`messages`/`group`/`error` in), typed `GroupEvent`s, auto-reconnect with jittered backoff, 4029 handling, 401 → session invalidation, sticky `UpdateRequired` |
| Database Schema | `store` | Exact Room schema v2 (5 tables, indices, FK cascade, 1→2 migration) in SQLite via `rusqlite`; every DAO method from the reference on the `LocalStore` trait |
| Sync Engine | `sync`, `reconcile`, `outbox`, `backoff` | `SyncEngine` state machine, `restCatchUp` (`GET /v1/pending`), `reconcileGroups`, `BatchReconciler` with echo detection, `OutboxDispatcher` (`BACKOFF_BASE`/`CAP`, `FALLBACK_POLL`, `IN_FLIGHT_STALE`, `Step::{Processed,Idle,WaitUntil}`), `MemberObservationRegistry` |
| Auth & Session | `session` | `SessionStore` (file-backed, token AES-GCM encrypted in the Android wire format `[ivLen][iv][ct]`), `SessionState`, `SessionManager` (= `SessionRepository` + `SessionScopeManager` wipe-on-401), `SyncLifecycle`, `AppPrefsStore`, `LegalStore` |
| Error Handling | `error` | RFC 7807 `ProblemDto` → all 20 `AppError` subtypes selected by the `type` URI suffix, with status fallback, `requestId`, `Retry-After` |
| Domain Layer | `validation`, `models`, `store` | `TextValidation` use cases (64/100/2000 code points), `can_post`/`can_add_members`, `OutgoingState`, OTP parsing, phone normalisation |

## Quick start

```rust
use std::sync::Arc;
use tzibbur_api::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Log in (phone + OTP).
    let client = TzibburClient::new()?;                       // https://api.tzibbur.me
    let ch = client.start_auth(&StartAuthRequest {
        phone: "+972501234567".into(), display_name: Some("Bridge".into()), region: Some("IL".into()),
    }).await?;
    let session = client.verify_auth(&VerifyAuthRequest {
        challenge_id: ch.challenge_id, code: "123456".into(),
        phone: "+972501234567".into(), display_name: Some("Bridge".into()), region: Some("IL".into()),
    }).await?;                                                // token now installed on `client`

    // 2. Local store + sync engine.
    let store: Arc<dyn LocalStore> = Arc::new(SqliteStore::open("tzibbur.db")?);
    let sync = SyncEngine::new(client.clone(), store.clone())?;
    sync.set_self_user_id(Some(session.user.id));
    let mut events = sync.subscribe();
    sync.start();

    // 3. React to live traffic.
    while let Ok(ev) = events.recv().await {
        match ev {
            SyncEvent::NewMessages { group_id, messages } => {
                for m in messages { println!("[{group_id}] {}: {}", m.sender_id, m.body); }
                sync.mark_read(&group_id, messages_max_seq(&store, &group_id)?).await?;
            }
            SyncEvent::UpdateRequired => break,
            _ => {}
        }
    }
    sync.stop().await;
    Ok(())
}

fn messages_max_seq(store: &Arc<dyn LocalStore>, g: &str) -> Result<i64> {
    Ok(store.max_seq(g)?.unwrap_or(0))
}
```

Sending goes through the outbox exactly like the app does:

```rust
let row = sync.send_message("group-id", "hello")?;   // validated, persisted, dispatched with backoff
// later: store.get_outbox(&row.client_message_id)?.outgoing_state() → Pending | Sent | Failed
```

Runnable examples:

```sh
# Log in with phone + OTP, then stream messages
TZIBBUR_PHONE=+972501234567 TZIBBUR_NAME=Bridge cargo run --example login_and_listen
# Read-only probe of every GET endpoint + a WS connect with an existing token
TZIBBUR_TOKEN=... cargo run --example probe
```

## Architecture

```
TzibburClient (REST) ──┐
                       ├── SyncEngine ── SqliteStore (LocalStore)
TzibburSocket (WS) ────┤        │
                       │   OutboxDispatcher ── POST /messages with backoff
SessionManager ────────┘   (wipe on 401 → stop sync, clear DB, clear session)
```

* Everything is `tokio`-based; the store is synchronous SQLite behind a mutex
  (cheap local calls) and emits `StoreChange` notifications as the analogue of
  Room `Flow`s.
* `SyncState` (`Idle / Connecting / Connected / BackingOff / UpdateRequired`)
  is exposed as a `watch` channel; `UpdateRequired` survives `stop()`/`start()`.
* `SyncEvent` is a `broadcast` channel for consumers (UI or a bridge).

## Verified against the live server

The models were checked against `https://api.tzibbur.me` with a real account
(`examples/probe.rs`, read-only). Where the decompiled reference and the wire
disagree, the wire wins and the app's spelling is still accepted:

| Item | Live behaviour |
|---|---|
| Enums | Lowercase: `role: "admin"/"member"`, `kind: "system"`, settings `"everyone"`. Parsing is case-insensitive; serialization is lowercase. |
| Group object | `role`, `readSeq`, `unreadCount`, nested `settings: {whoCanPost, whoCanAddMembers}`, `limits: {memberCap: 100, messageMaxLength: 1000, minMembersToPost: 0}`. Flattened onto `GroupDto`; `GroupDto::message_max_length()` prefers the server limit over the app's 2000. |
| Lists | `{"items": [...], "nextCursor": null}` for groups, members, devices. Messages use `{"items", "nextAfterSeq", "nextBeforeSeq"}` (`MessagesPage`). |
| `GET /v1/pending` | `{"groups": [{"groupId", "deliveredSeq", "hasMore", "messages": [...]}]}`. When `hasMore` is set the sync engine pages that group over REST. |
| `GET /v1/legal/{key}` | `{"document": {"key", "text", "checksum"}}`; unwrapped into `LegalDocument`. |
| `POST /v1/contacts/check` | `{"registered": ["+1555…"]}` (plain E.164 strings). |
| Problem `type` | Underscore slugs (`urn:tzibbur:error:validation_failed`, `not_found`); normalised to the app's hyphenated names before mapping. `errors` is an array of `{path, message}`. |
| Users / members | Carry `kind: "person" | "service"` (the Tzibbur System sender is a `service`). |
| Devices | `deviceModel`, `registeredAt`, `lastSeenAt`, `userId`, `imei`, `serialNumber`. |
| WS `hello` | `{"protocolVersion": 1, "userId", "deviceId", "limits": {"heartbeatSeconds": 30, "maxConnectionsPerDevice": 3, "maxFrameBytes": 16384}}`, surfaced as `SocketEvent::Hello`. The server also sends WebSocket-level pings. |
| WS `messages` | Carries `hasMore` alongside `groupId` and `messages`. |
| Timestamps | RFC 3339 strings; epoch milliseconds are accepted too. |

Still unconfirmed (no write traffic was sent): the exact slug for an
admin-only permission (`Permission::ADMINS` = `"admins"` is a guess), and
whether `POST /v1/groups` expects the flat `whoCanPost` fields or a nested
`settings` object.

## Testing

```sh
cargo test
```

* 22 unit tests (error mapping, live DTO shapes, reconciler, backoff, validation,
  AES-GCM framing, SQLite schema/migration/DAO semantics, session file store).
* 3 end-to-end tests in `tests/e2e_mock_server.rs` run an in-process mock of
  the API (axum, REST + WebSocket) and drive the real client, socket, sync
  engine, outbox and session wipe.

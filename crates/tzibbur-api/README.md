# tzibbur-api

Rust client for the **Tzibbur** group-messaging service (used by [UnTzibburBot](../../README.md)), built against
Tzibbur's official contract: the [integration guide](https://api.tzibbur.me/integration) and the
[OpenAPI spec](https://api.tzibbur.me/docs/openapi.json). How this crate uses it, and what it sends when, is in
[`docs/tzibbur-api.md`](../../docs/tzibbur-api.md).

Delivery is push-only: one WebSocket per device, each batch acked on the socket. While the socket is up the crate
makes no periodic requests; `GET /v1/pending` is only a fallback while it is down.

| Reference section | Module | What's there |
|---|---|---|
| REST API | `http` | `TzibburClient`: auth, me, devices, capabilities, sessions, contacts, legal, groups, categories, members, messages (incl. in-chat command replies), read, ack, pending |
| WebSocket Protocol | `ws` | `TzibburSocket`: protocol v1 frames (`ping`/`ack`/`read` out, `hello`/`pong`/`messages`/`group`/`read`/`error` in), typed `GroupEvent`s, auto-reconnect with jittered backoff, 4001/401/403 → session invalidation, 4029 and 429 back-off, sticky `UpdateRequired` |
| Database Schema | `store` | Exact Room schema v2 (5 tables, indices, FK cascade, 1→2 migration) in SQLite via `rusqlite`; every DAO method from the reference on the `LocalStore` trait |
| Sync Engine | `sync`, `reconcile`, `outbox`, `backoff` | `SyncEngine` state machine (socket-first delivery, acks on the delivering transport, `GET /v1/pending` only while offline), `reconcileGroups`, `BatchReconciler` with echo detection, `OutboxDispatcher` (idempotent retries on `clientMessageId`, `Retry-After`, command replies), `MemberObservationRegistry` |
| Auth & Session | `session` | `SessionStore` (file-backed, token AES-GCM encrypted in the Android wire format `[ivLen][iv][ct]`), `SessionState`, `SessionManager` (= `SessionRepository` + `SessionScopeManager` wipe-on-401), `SyncLifecycle`, `AppPrefsStore`, `LegalStore` |
| Error Handling | `error` | RFC 9457 `ProblemDto` → typed `AppError`s selected by the `type` code (incl. `posting_not_allowed`, `admin_required`, `device_blocked` with `errors.reason`), with status fallback, `requestId`, `Retry-After` |
| Domain Layer | `validation`, `models`, `store` | `TextValidation` use cases (64/100/1000 code points), `can_post`/`can_add_members`, `OutgoingState`, OTP parsing, phone normalisation |

## Quick start

```rust
use std::sync::Arc;
use tzibbur_api::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Log in (phone + OTP).
    let client = TzibburClient::builder()                     // https://api.tzibbur.me
        .device(DeviceInfo::new("android", "My Tzibbur client"))
        .build()?;
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
# Read-only look at an account: every GET endpoint + a short WS connect
TZIBBUR_TOKEN=... cargo run --example probe
# Every endpoint, including writes, against a throwaway group
TZIBBUR_TOKEN=... cargo run --example live_smoke
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

## Wire notes

The official guide is the reference. The models also accept a few older shapes seen
before it existed (bare message replies, nested `payload` on group frames, flat
`whoCanPost` fields, uppercase enums, epoch-millisecond timestamps), so an older server
or a mock still parses.

## Testing

```sh
cargo test
```

* Unit tests: error mapping, official DTO and frame shapes, send/command replies,
  reconciler, backoff, validation, AES-GCM framing, SQLite schema/migration/DAO
  semantics, session file store.
* End-to-end tests in `tests/e2e_mock_server.rs` run an in-process mock of the
  official API (axum, REST + WebSocket): the socket pushes the backlog after `hello`
  and releases each next batch only after a socket ack. They drive the real client,
  socket, sync engine, outbox and session wipe, and assert that a connected session
  makes no `GET /v1/pending`, history or REST-ack requests.

//! End-to-end test against an in-process mock of the Tzibbur API (REST + WS), modelled
//! on the official protocol (<https://api.tzibbur.me/integration>): the socket pushes
//! the backlog after `hello`, holds the next batch for a group until the previous one
//! is acked on the socket, and group frames carry their fields at the top level.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tzibbur_api::prelude::*;

const TOKEN: &str = "test-token";

#[derive(Clone)]
struct Mock {
    seq: Arc<AtomicI64>,
    /// Acks received on the WebSocket.
    acks: Arc<Mutex<Vec<(String, i64)>>>,
    /// Acks received over REST (`POST /v1/groups/{id}/ack`).
    rest_acks: Arc<Mutex<Vec<(String, i64)>>>,
    /// `read` frames received on the WebSocket.
    reads: Arc<Mutex<Vec<(String, i64)>>>,
    /// Polling-style REST reads: `GET /v1/pending` and history paging.
    pending_hits: Arc<AtomicI64>,
    history_hits: Arc<AtomicI64>,
    sent: Arc<Mutex<Vec<Value>>>,
    push: Arc<Mutex<Option<mpsc::UnboundedSender<Value>>>>,
    unauthorized_hits: Arc<AtomicI64>,
}

fn problem(status: StatusCode, slug: &str) -> Response {
    let body = json!({
        "type": format!("urn:tzibbur:error:{slug}"),
        "title": slug, "status": status.as_u16(), "requestId": "req-1"
    });
    (
        status,
        [("content-type", "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

async fn auth_start(Json(body): Json<Value>) -> Response {
    assert_eq!(body["phone"], "+15551234567");
    Json(json!({"challengeId": "ch1", "expiresAtEpochMs": 1_800_000_000_000i64, "resendAfterSeconds": 30})).into_response()
}

async fn auth_verify(Json(body): Json<Value>) -> Response {
    if body["code"] != "123456" {
        return problem(StatusCode::UNAUTHORIZED, "invalid-code");
    }
    Json(json!({
        "user": {"id": "me", "displayName": "Me", "phoneE164": "+15551234567"},
        "device": {"id": "dev1", "platform": "rust"},
        "token": TOKEN
    }))
    .into_response()
}

async fn me(h: HeaderMap, State(m): State<Mock>) -> Response {
    if !authed(&h) {
        m.unauthorized_hits.fetch_add(1, Ordering::SeqCst);
        return problem(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    Json(json!({"id": "me", "displayName": "Me", "phoneE164": "+15551234567"})).into_response()
}

async fn groups(h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if !authed(&h) {
        return problem(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !q.contains_key("cursor") {
        Json(json!({"groups": [
            {"id": "g1", "name": "Alpha", "category": "family", "kind": "STANDARD",
             "whoCanPost": "EVERYONE", "whoCanAddMembers": "ADMINS", "createdAt": "2026-01-01T00:00:00Z",
             "myRole": "ADMIN", "memberCount": 2}
        ], "nextCursor": "page2"}))
        .into_response()
    } else {
        Json(json!({"groups": [
            {"id": "g2", "name": "Beta", "category": "work", "kind": "SYSTEM",
             "whoCanPost": "ADMINS", "whoCanAddMembers": "ADMINS", "createdAt": 1700000000000i64,
             "myRole": "MEMBER", "memberCount": 40}
        ], "nextCursor": null}))
        .into_response()
    }
}

async fn group(Path(id): Path<String>) -> Response {
    if id == "gone" {
        return problem(StatusCode::NOT_FOUND, "not-found");
    }
    Json(json!({"id": id, "name": "Alpha", "category": "family", "kind": "STANDARD",
        "whoCanPost": "EVERYONE", "whoCanAddMembers": "ADMINS", "createdAt": 1, "myRole": "ADMIN", "memberCount": 2}))
    .into_response()
}

async fn members(Path(_id): Path<String>) -> Response {
    Json(json!([
        {"userId": "me", "displayName": "Me", "role": "ADMIN", "joinedAt": 1},
        {"userId": "u2", "displayName": "Other", "phoneE164": "+15550000000", "role": "MEMBER", "joinedAt": 2}
    ]))
    .into_response()
}

async fn add_members(Json(body): Json<Value>) -> Response {
    let phones = body["phones"].as_array().unwrap().len();
    if phones > 100 {
        return problem(StatusCode::BAD_REQUEST, "contacts-batch-too-large");
    }
    Json(json!({"added": [{"userId": "u3", "displayName": "New", "role": "MEMBER", "joinedAt": 3}], "notFound": ["+1999"], "alreadyMember": []}))
        .into_response()
}

async fn set_role(Path((_g, uid)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    if uid == "me"
        && body["role"]
            .as_str()
            .map(|r| r.eq_ignore_ascii_case("member"))
            .unwrap_or(false)
    {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, "last-admin");
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn pending(h: HeaderMap, State(m): State<Mock>) -> Response {
    if !authed(&h) {
        return problem(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    m.pending_hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({"messages": [
        {"groupId": "g1", "messages": [
            {"id": "m1", "seq": 1, "senderId": "u2", "body": "hello from pending", "createdAt": 1000},
            {"id": "m2", "seq": 2, "senderId": "u2", "body": "second", "createdAt": 2000}
        ]}
    ]}))
    .into_response()
}

async fn send_message(
    Path(gid): Path<String>,
    State(m): State<Mock>,
    Json(body): Json<Value>,
) -> Response {
    if body["body"].as_str().unwrap_or("").starts_with("#help") {
        return Json(
            json!({"command": {"name": "help", "ok": true, "code": "help",
            "params": {}, "text": "Commands: #add, #exit, #mute, #unmute, #help"}}),
        )
        .into_response();
    }
    if body["body"].as_str().unwrap_or("").contains("BAD") {
        return problem(StatusCode::BAD_REQUEST, "invalid-message");
    }
    if body["body"].as_str().unwrap_or("").contains("FLAKY") && m.sent.lock().unwrap().is_empty() {
        m.sent.lock().unwrap().push(json!({"flaky": true}));
        return problem(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    let seq = m.seq.fetch_add(1, Ordering::SeqCst);
    let msg = json!({"id": format!("srv-{seq}"), "groupId": gid, "seq": seq, "senderId": "me",
        "body": body["body"], "clientMessageId": body["clientMessageId"], "createdAt": seq * 1000});
    m.sent.lock().unwrap().push(msg.clone());
    // Echo over WS too, as the real server does.
    if let Some(tx) = m.push.lock().unwrap().as_ref() {
        let _ = tx
            .send(json!({"type": "messages", "groupId": gid, "messages": [msg], "hasMore": false}));
    }
    (
        StatusCode::CREATED,
        Json(json!({"message": msg, "duplicate": false})),
    )
        .into_response()
}

async fn history(State(m): State<Mock>) -> Response {
    m.history_hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({"items": [], "nextAfterSeq": null, "nextBeforeSeq": null})).into_response()
}

async fn ack(Path(gid): Path<String>, State(m): State<Mock>, Json(body): Json<Value>) -> Response {
    m.rest_acks
        .lock()
        .unwrap()
        .push((gid, body["seq"].as_i64().unwrap()));
    StatusCode::NO_CONTENT.into_response()
}

async fn legal(Path(key): Path<String>) -> Response {
    Json(json!({"key": key, "markdown": "# Terms", "checksum": "abc"})).into_response()
}

async fn ws_route(h: HeaderMap, ws: WebSocketUpgrade, State(m): State<Mock>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| ws_conn(socket, m))
}

async fn ws_conn(mut socket: WebSocket, m: Mock) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    *m.push.lock().unwrap() = Some(tx);
    socket
        .send(Message::Text(
            json!({"type": "hello", "protocolVersion": 1, "userId": "me", "deviceId": "dev1",
                "limits": {"heartbeatSeconds": 30, "maxConnectionsPerDevice": 3, "maxFrameBytes": 16384}})
            .to_string(),
        ))
        .await
        .unwrap();
    // Automatic catch-up: the first batch of the backlog, with more to come.
    socket
        .send(Message::Text(
            json!({"type": "messages", "groupId": "g1", "hasMore": true, "messages": [
                {"id": "m1", "groupId": "g1", "seq": 1, "senderId": "u2", "body": "Other: hello", "createdAt": 1000},
                {"id": "m2", "groupId": "g1", "seq": 2, "senderId": "u2", "body": "Other: second", "createdAt": 2000}
            ]})
            .to_string(),
        ))
        .await
        .unwrap();
    let mut rest_of_backlog_sent = false;
    loop {
        tokio::select! {
            Some(v) = rx.recv() => { if socket.send(Message::Text(v.to_string())).await.is_err() { break; } }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    match v["type"].as_str() {
                        Some("ping") => { let _ = socket.send(Message::Text(json!({"type": "pong"}).to_string())).await; }
                        Some("ack") => {
                            let (g, seq) = (v["groupId"].as_str().unwrap().to_owned(), v["seq"].as_i64().unwrap());
                            m.acks.lock().unwrap().push((g.clone(), seq));
                            // The next batch for a group is released only by an ack on the socket.
                            if g == "g1" && seq == 2 && !rest_of_backlog_sent {
                                rest_of_backlog_sent = true;
                                for frame in [
                                    json!({"type": "messages", "groupId": "g1", "hasMore": false, "messages": [
                                        {"id": "m3", "groupId": "g1", "seq": 3, "senderId": "u2", "body": "Other: live!", "createdAt": 3000}
                                    ]}),
                                    json!({"type": "group", "event": "group-updated", "groupId": "g1", "name": "Alpha Renamed",
                                        "settings": {"whoCanPost": "everyone", "whoCanAddMembers": "admins"}, "actorId": "u2"}),
                                    json!({"type": "group", "event": "member-added", "groupId": "g1", "userId": "u9",
                                        "role": "member", "joinedSeq": 3, "actorId": "u2"}),
                                ] {
                                    let _ = socket.send(Message::Text(frame.to_string())).await;
                                }
                            }
                        }
                        Some("read") => m.reads.lock().unwrap().push((v["groupId"].as_str().unwrap().to_owned(), v["seq"].as_i64().unwrap())),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
}

async fn spawn_server() -> (String, Mock) {
    let mock = Mock {
        seq: Arc::new(AtomicI64::new(10)),
        acks: Default::default(),
        rest_acks: Default::default(),
        reads: Default::default(),
        pending_hits: Default::default(),
        history_hits: Default::default(),
        sent: Default::default(),
        push: Default::default(),
        unauthorized_hits: Default::default(),
    };
    let app = Router::new()
        .route("/v1/auth/start", post(auth_start))
        .route("/v1/auth/verify", post(auth_verify))
        .route("/v1/me", get(me))
        .route("/v1/groups", get(groups))
        .route("/v1/groups/:id", get(group))
        .route("/v1/groups/:id/members", get(members).post(add_members))
        .route(
            "/v1/groups/:id/members/:uid",
            axum::routing::patch(set_role),
        )
        .route("/v1/groups/:id/messages", post(send_message).get(history))
        .route("/v1/groups/:id/ack", post(ack))
        .route("/v1/pending", get(pending))
        .route("/v1/legal/:key", get(legal))
        .route("/v1/ws", get(ws_route))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

async fn wait_for<F: FnMut() -> bool>(mut f: F, what: &str) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn rest_auth_and_error_mapping() {
    let (base, mock) = spawn_server().await;
    let client = TzibburClient::builder().base_url(&base).build().unwrap();

    let ch = client
        .start_auth(&StartAuthRequest {
            phone: "+15551234567".into(),
            display_name: Some("Me".into()),
            region: None,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ch.challenge_id, "ch1");
    assert_eq!(ch.resend_after_seconds, Some(30));
    assert_eq!(ch.expires_at_epoch_ms, Some(1_800_000_000_000));

    let bad = client
        .verify_auth(&VerifyAuthRequest {
            challenge_id: "ch1".into(),
            code: "000000".into(),
            phone: "+15551234567".into(),
            display_name: None,
            region: None,
            ..Default::default()
        })
        .await;
    assert!(matches!(bad, Err(AppError::InvalidCode { .. })), "{bad:?}");

    // Unauthenticated call → NotSignedIn locally, no request made.
    assert!(matches!(client.me().await, Err(AppError::NotSignedIn)));

    let session = client
        .verify_auth(&VerifyAuthRequest {
            challenge_id: "ch1".into(),
            code: "123456".into(),
            phone: "+15551234567".into(),
            display_name: Some("Me".into()),
            region: None,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(session.token, TOKEN);
    assert_eq!(client.me().await.unwrap().id, "me");

    let groups = client.list_all_groups().await.unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[1].kind, GroupKind::System);
    assert_eq!(groups[0].created_at, Some(1767225600000));

    assert!(matches!(
        client.get_group("gone").await,
        Err(AppError::NotFound { .. })
    ));
    assert!(matches!(
        client.set_member_role("g1", "me", Role::Member).await,
        Err(AppError::LastAdmin { .. })
    ));
    client
        .set_member_role("g1", "u2", Role::Admin)
        .await
        .unwrap();

    let outcome = client
        .add_members("g1", &["+1555".into(), "+1999".into()], None)
        .await
        .unwrap();
    assert_eq!(outcome.added.len(), 1);
    assert_eq!(outcome.not_found, vec!["+1999".to_string()]);
    let too_many: Vec<String> = (0..101).map(|i| format!("+1{i:010}")).collect();
    assert!(matches!(
        client.add_members("g1", &too_many, None).await,
        Err(AppError::ContactsBatchTooLarge { .. })
    ));

    let doc = client.legal(LegalDocKey::Terms).await.unwrap();
    assert_eq!(doc.checksum, "abc");
    assert_eq!(doc.key.as_deref(), Some("terms"));

    // 401 fires the invalidation listener.
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f2 = fired.clone();
    let bad_client = TzibburClient::builder()
        .base_url(&base)
        .token("wrong")
        .session_listener(Arc::new(move || f2.store(true, Ordering::SeqCst)))
        .build()
        .unwrap();
    assert!(matches!(
        bad_client.me().await,
        Err(AppError::Unauthorized { .. })
    ));
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(mock.unauthorized_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sync_engine_end_to_end() {
    let (base, mock) = spawn_server().await;
    let client = TzibburClient::builder()
        .base_url(&base)
        .token(TOKEN)
        .build()
        .unwrap();
    let store: Arc<dyn LocalStore> = Arc::new(SqliteStore::in_memory().unwrap());
    let sync = SyncEngine::with_parts(
        client.clone(),
        store.clone(),
        Arc::new(TzibburSocket::new(client.clone()).unwrap()),
        MemberRefreshPolicy::All,
    );
    sync.set_self_user_id(Some("me".into()));
    let mut events = sync.subscribe();
    sync.start();

    // Wait until connected and caught up: the pushed backlog (m1, m2), then m3, which the
    // mock releases only after the first batch was acked on the socket.
    wait_for(
        || store.max_seq("g1").unwrap() == Some(3),
        "messages m1..m3",
    )
    .await;
    assert_eq!(sync.sync_state(), SyncState::Connected);
    let thread = store.thread("g1", 50).unwrap();
    assert_eq!(
        thread.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // Groups reconciled from REST, group-updated applied, member-added + refresh.
    wait_for(
        || {
            store
                .get_group("g1")
                .unwrap()
                .map(|g| g.name == "Alpha Renamed")
                .unwrap_or(false)
        },
        "rename",
    )
    .await;
    assert!(store.get_group("g2").unwrap().unwrap().is_system());
    wait_for(|| store.members("g1").unwrap().len() >= 2, "members").await;

    // Unread accounting excludes own messages and honours lastReadSeq.
    let unread = store.groups_with_unread("me").unwrap();
    let g1 = unread.iter().find(|g| g.group.id == "g1").unwrap();
    assert_eq!(g1.unread_count, 3);

    // Every socket batch is acked on the socket with its last seq.
    wait_for(
        || mock.acks.lock().unwrap().contains(&("g1".to_string(), 3)),
        "ws ack",
    )
    .await;
    assert!(mock.acks.lock().unwrap().contains(&("g1".to_string(), 2)));

    // Reading is a `read` frame, not an ack.
    sync.mark_read("g1", 3).await.unwrap();
    wait_for(
        || mock.reads.lock().unwrap().contains(&("g1".to_string(), 3)),
        "ws read",
    )
    .await;
    assert_eq!(store.get_group("g1").unwrap().unwrap().last_read_seq, 3);

    // Send through the outbox; server echoes over WS and the POST response confirms it.
    let row = sync.send_message("g1", "  hi there  ").unwrap();
    wait_for(
        || {
            store
                .get_outbox(&row.client_message_id)
                .unwrap()
                .map(|r| r.state == OutboxState::Confirmed)
                .unwrap_or(false)
        },
        "outbox confirmed",
    )
    .await;
    let confirmed = store.get_outbox(&row.client_message_id).unwrap().unwrap();
    assert_eq!(confirmed.body, "hi there");
    assert_eq!(confirmed.confirmed_seq, Some(10));
    assert_eq!(confirmed.outgoing_state(), OutgoingState::Sent);
    // Echo stored exactly once, no duplicate from WS + REST.
    assert_eq!(store.count_between("g1", 10, 10).unwrap(), 1);

    // Permanent rejection → Failed.
    let bad = sync.send_message("g1", "BAD message").unwrap();
    wait_for(
        || {
            store
                .get_outbox(&bad.client_message_id)
                .unwrap()
                .map(|r| r.error_code.is_some())
                .unwrap_or(false)
        },
        "rejection",
    )
    .await;
    let r = store.get_outbox(&bad.client_message_id).unwrap().unwrap();
    assert_eq!(r.error_code.as_deref(), Some("invalid-message"));
    assert_eq!(r.outgoing_state(), OutgoingState::Failed);

    // Transient failure → rescheduled with attemptCount incremented, then retry succeeds.
    mock.sent.lock().unwrap().clear();
    let flaky = sync.send_message("g1", "FLAKY").unwrap();
    wait_for(
        || {
            store
                .get_outbox(&flaky.client_message_id)
                .unwrap()
                .map(|r| r.attempt_count >= 1 && r.next_attempt_at.is_some())
                .unwrap_or(false)
        },
        "reschedule",
    )
    .await;
    let r = store.get_outbox(&flaky.client_message_id).unwrap().unwrap();
    assert_eq!(r.outgoing_state(), OutgoingState::Pending);
    assert_eq!(r.error_code.as_deref(), Some("internal"));
    // Fast-forward: make it due now and poke.
    store.reschedule(&flaky.client_message_id, 0, None).unwrap();
    sync.outbox().poke();
    wait_for(
        || {
            store
                .get_outbox(&flaky.client_message_id)
                .unwrap()
                .map(|r| r.state == OutboxState::Confirmed)
                .unwrap_or(false)
        },
        "flaky confirmed",
    )
    .await;

    // An in-chat command is executed, not stored: no seq, the outbox row goes away.
    let mut outbox_events = sync.outbox().subscribe();
    let cmd = sync.send_message("g1", "#help").unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(OutboxEvent::Command {
                result,
                client_message_id,
                ..
            }) = outbox_events.recv().await
            {
                if client_message_id == cmd.client_message_id {
                    return result;
                }
            }
        }
    })
    .await
    .expect("command reply");
    assert_eq!(reply.code, "help");
    assert!(store.get_outbox(&cmd.client_message_id).unwrap().is_none());
    assert_eq!(store.command_replies("g1").unwrap().len(), 1);

    // While the socket is up nothing is polled: no /v1/pending, no history paging and
    // no REST acks. (This is the load the unofficial client used to put on the server.)
    assert_eq!(
        mock.pending_hits.load(Ordering::SeqCst),
        0,
        "GET /v1/pending while connected"
    );
    assert_eq!(
        mock.history_hits.load(Ordering::SeqCst),
        0,
        "history paging while connected"
    );
    assert!(
        mock.rest_acks.lock().unwrap().is_empty(),
        "REST acks while connected"
    );

    // We saw NewMessages / EchoConfirmed events.
    let mut saw_new = false;
    let mut saw_echo = false;
    while let Ok(ev) = events.try_recv() {
        match ev {
            SyncEvent::NewMessages { .. } => saw_new = true,
            SyncEvent::EchoConfirmed { .. } => saw_echo = true,
            _ => {}
        }
    }
    assert!(saw_new && saw_echo);

    // Stop → Idle, socket closed.
    sync.stop().await;
    assert_eq!(sync.sync_state(), SyncState::Idle);
    assert!(!sync.is_running());
}

#[tokio::test]
async fn session_manager_wipes_on_401() {
    let (base, _mock) = spawn_server().await;
    let local: Arc<dyn LocalStore> = Arc::new(SqliteStore::in_memory().unwrap());
    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::default());
    session_store
        .save(&StoredSession {
            user: User {
                id: "me".into(),
                display_name: "Me".into(),
                phone_e164: None,
                ..Default::default()
            },
            device: Device {
                id: "d".into(),
                ..Default::default()
            },
            token: "stale".into(),
        })
        .await
        .unwrap();

    let listener = LateBoundListener::new();
    let client = TzibburClient::builder()
        .base_url(&base)
        .session_listener(listener.clone())
        .build()
        .unwrap();
    let manager = SessionManager::new(client.clone(), session_store.clone(), local.clone());
    listener.bind(&manager);

    assert!(manager.load().await.unwrap().is_signed_in());
    local
        .upsert_group(&GroupEntity::from(client_group()))
        .unwrap();
    assert!(matches!(
        client.me().await,
        Err(AppError::Unauthorized { .. })
    ));

    let mut st = manager.watch_state();
    tokio::time::timeout(Duration::from_secs(2), async {
        while st.borrow().is_signed_in() {
            st.changed().await.unwrap();
        }
    })
    .await
    .expect("session should be wiped");
    assert!(session_store.load().await.unwrap().is_none());
    assert!(local.groups().unwrap().is_empty());
    assert!(!client.is_signed_in().await);
}

fn client_group() -> GroupDto {
    serde_json::from_value(
        json!({"id": "g1", "name": "A", "kind": "STANDARD", "whoCanPost": "EVERYONE",
        "whoCanAddMembers": "ADMINS", "myRole": "ADMIN", "memberCount": 1}),
    )
    .unwrap()
}

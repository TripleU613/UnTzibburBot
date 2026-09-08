//! Integration test against a real Directus. Skipped unless DIRECTUS_URL and
//! DIRECTUS_TOKEN are set (e.g. a local `npx directus start` with SQLite).

#[path = "../src/directus.rs"]
#[allow(dead_code)]
mod directus;
#[path = "../src/store.rs"]
#[allow(dead_code)]
mod store;

use store::{AccountStatus, Store};

#[tokio::test]
async fn store_roundtrip_on_live_directus() {
    let (Ok(url), Ok(token)) = (
        std::env::var("DIRECTUS_URL"),
        std::env::var("DIRECTUS_TOKEN"),
    ) else {
        eprintln!("skipping: DIRECTUS_URL/DIRECTUS_TOKEN not set");
        return;
    };
    let prefix = format!("t{}_", chrono::Utc::now().timestamp_millis() % 1_000_000);
    let d = directus::Directus::new(url.parse().unwrap(), token).unwrap();
    d.ensure_schema(&prefix).await.expect("schema");
    let store = Store::new(d.clone(), directus::collection_names(&prefix));

    let user = store
        .upsert_user(424242, Some("tester"), Some("Test"))
        .await
        .unwrap();
    let again = store
        .upsert_user(424242, Some("tester2"), Some("Test"))
        .await
        .unwrap();
    assert_eq!(user.id, again.id);
    assert_eq!(
        store.user_by_telegram_id(424242).await.unwrap().unwrap().id,
        user.id
    );

    let acc = store
        .connect_account(user.id, "tz-user", Some("+1555"), "Name", "dev", "blob")
        .await
        .unwrap();
    assert_eq!(acc.status, AccountStatus::Connected);
    assert_eq!(acc.user, user.id);
    let acc2 = store
        .connect_account(user.id, "tz-user", Some("+1555"), "Name2", "dev", "blob2")
        .await
        .unwrap();
    assert_eq!(acc.id, acc2.id, "reconnect reuses the row");
    assert_eq!(store.connected_accounts().await.unwrap().len(), 1);
    assert_eq!(
        store.account_for_user(user.id).await.unwrap().unwrap().id,
        acc.id
    );

    let conv = store
        .create_conversation(acc.id, "g1", 424242, Some(77), "Group", "standard", 5)
        .await
        .unwrap();
    assert_eq!(conv.topic_id(), Some(77));
    assert_eq!(conv.last_forwarded_seq, 5);
    assert_eq!(
        store
            .conversation_by_group(acc.id, "g1")
            .await
            .unwrap()
            .unwrap()
            .id,
        conv.id
    );
    assert_eq!(
        store
            .conversation_by_topic(424242, 77)
            .await
            .unwrap()
            .unwrap()
            .id,
        conv.id
    );
    assert!(
        store.conversation_by_topic(1, 77).await.unwrap().is_none(),
        "tenant scoping"
    );
    store.advance_forwarded_seq(&conv, 9).await.unwrap();
    store.advance_forwarded_seq(&conv, 3).await.unwrap();
    assert_eq!(
        store
            .conversation(conv.id)
            .await
            .unwrap()
            .unwrap()
            .last_forwarded_seq,
        9
    );
    store.set_conversation_topic(conv.id, None).await.unwrap();
    assert!(store
        .conversation(conv.id)
        .await
        .unwrap()
        .unwrap()
        .topic_id()
        .is_none());

    store.record_inbound(conv.id, "m1", 6, 1001).await.unwrap();
    assert!(store
        .message_by_tzibbur_id(conv.id, "m1")
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        store
            .conversation_by_telegram_message(424242, 1001)
            .await
            .unwrap()
            .unwrap()
            .id,
        conv.id
    );
    let out = store
        .record_outbound(conv.id, "cmid-1", 1002)
        .await
        .unwrap();
    assert!(out.tzibbur_message_id.is_none());
    store.confirm_outbound("cmid-1", "srv-9", 10).await.unwrap();
    let m = store.message_by_client_id("cmid-1").await.unwrap().unwrap();
    assert_eq!(m.tzibbur_message_id.as_deref(), Some("srv-9"));
    assert_eq!(m.seq, Some(10));

    store
        .set_account_status(acc.id, AccountStatus::ReauthRequired)
        .await
        .unwrap();
    let a = store.account_for_user(user.id).await.unwrap().unwrap();
    assert_eq!(a.status, AccountStatus::ReauthRequired);
    assert!(
        a.encrypted_session.is_none(),
        "session cleared when not connected"
    );

    store.purge_account(acc.id).await.unwrap();
    assert!(store.account_for_user(user.id).await.unwrap().is_none());
    assert!(store
        .conversations_for_account(acc.id)
        .await
        .unwrap()
        .is_empty());

    // Clean up the test collections.
    let n = directus::collection_names(&prefix);
    for c in [n.messages, n.conversations, n.accounts, n.users] {
        let _ = reqwest::Client::new()
            .delete(format!("{}/collections/{c}", url.trim_end_matches('/')))
            .bearer_auth(std::env::var("DIRECTUS_TOKEN").unwrap())
            .send()
            .await;
    }
}

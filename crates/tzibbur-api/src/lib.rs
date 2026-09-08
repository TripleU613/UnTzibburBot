//! # tzibbur-api
//!
//! Rust client for the Tzibbur group-messaging API, reconstructed from the
//! reverse-engineered Android app (`com.tzibbur.app` 0.1.0).
//!
//! | Layer | Module |
//! |---|---|
//! | REST endpoints (`/v1/...`) | [`http`] |
//! | WebSocket protocol v1 | [`ws`] |
//! | RFC 7807 → typed errors | [`error`] |
//! | Wire DTOs | [`models`] |
//! | Local SQLite store (Room schema v2) | [`store`] |
//! | Incoming batch reconciliation / echo detection | [`reconcile`] |
//! | Outbox dispatch with backoff | [`outbox`] |
//! | Sync engine (WS + catch-up + group reconcile) | [`sync`] |
//! | Session persistence, wipe on 401, prefs, legal | [`session`] |
//! | Validation use cases | [`validation`] |
//! | Recovered constants | [`constants`] |
//!
//! ```no_run
//! use tzibbur_api::prelude::*;
//! # async fn demo() -> Result<()> {
//! let client = TzibburClient::builder().token("BEARER").build()?;
//! let store: std::sync::Arc<dyn LocalStore> = std::sync::Arc::new(SqliteStore::open("tzibbur.db")?);
//! let sync = SyncEngine::new(client.clone(), store.clone())?;
//! let mut events = sync.subscribe();
//! sync.start();
//! while let Ok(ev) = events.recv().await {
//!     if let SyncEvent::NewMessages { group_id, messages } = ev {
//!         println!("{group_id}: {} new", messages.len());
//!     }
//! }
//! # Ok(()) }
//! ```

pub mod backoff;
pub mod constants;
pub mod error;
pub mod http;
pub mod models;
pub mod outbox;
pub mod reconcile;
pub mod session;
pub mod store;
pub mod sync;
pub mod validation;
pub mod ws;

pub use error::{AppError, ProblemDto, Result};
pub use http::{ClientBuilder, DeviceInfo, SessionInvalidationListener, TzibburClient};
pub use sync::{SyncEngine, SyncEvent, SyncState};
pub use ws::{ClientFrame, DisconnectReason, GroupEvent, ServerFrame, SocketEvent, TzibburSocket};

/// Everything most callers need.
pub mod prelude {
    pub use crate::constants::*;
    pub use crate::error::{AppError, Result};
    pub use crate::http::{DeviceInfo, SessionInvalidationListener, TzibburClient};
    pub use crate::models::*;
    pub use crate::outbox::{OutboxDispatcher, OutboxEvent};
    pub use crate::session::{
        AesGcmCipher, AppPrefsStore, FileSessionStore, LateBoundListener, LegalStore,
        MemorySessionStore, PlainCipher, SessionManager, SessionState, SessionStore, StoredSession,
    };
    pub use crate::store::{
        GroupEntity, GroupWithUnread, LocalCommandReplyEntity, LocalStore, MemberEntity,
        MessageEntity, OutboxEntity, OutboxState, OutgoingState, SqliteStore, StoreChange,
    };
    pub use crate::sync::{
        MemberObservationRegistry, MemberRefreshPolicy, SyncEngine, SyncEvent, SyncState,
    };
    pub use crate::validation::*;
    pub use crate::ws::{
        ClientFrame, DisconnectReason, GroupEvent, ServerFrame, SocketEvent, TzibburSocket,
    };
}

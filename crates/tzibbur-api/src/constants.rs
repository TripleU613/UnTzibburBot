//! Constants recovered from the Tzibbur Android client.

use std::time::Duration;

/// REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.tzibbur.me";
/// WebSocket endpoint.
pub const DEFAULT_WS_URL: &str = "wss://api.tzibbur.me/v1/ws";
/// WebSocket protocol version the client speaks.
pub const WS_PROTOCOL_VERSION: u32 = 1;
/// Socket close code sent by the server when the account has too many open connections.
pub const WS_CLOSE_TOO_MANY_CONNECTIONS: u16 = 4029;
/// Capacity of the socket event buffer (`MutableSharedFlow(extraBufferCapacity=256)`).
pub const WS_EVENT_BUFFER: usize = 256;
/// Client sends a `ping` after this much outbound silence.
pub const PING_AFTER_OUTBOUND_SILENCE: Duration = Duration::from_secs(30);

// ---- Validation limits -------------------------------------------------------

/// Max display name length in Unicode code points.
pub const DEFAULT_MAX_DISPLAY_NAME: usize = 64;
/// Max group name length in Unicode code points.
pub const DEFAULT_MAX_GROUP_NAME: usize = 100;
/// Max members per group.
pub const DEFAULT_MAX_MEMBERS: usize = 200;
/// Max message body length in Unicode code points.
pub const DEFAULT_MAX_MESSAGE: usize = 2000;
/// Composer shows a "near limit" warning from this many code points.
pub const MESSAGE_NEAR_LIMIT: usize = 1800;
/// Max phone numbers per `contacts/check` or `members` add batch.
pub const DEFAULT_MAX_PHONES: usize = 100;

// ---- Auth --------------------------------------------------------------------

/// OTP code length.
pub const CODE_LENGTH: usize = 6;
/// Default wait before a resend is offered when the server did not say.
pub const DEFAULT_RESEND_WAIT: Duration = Duration::from_secs(60);
/// Default wait after a 429 on `/auth/start` when no `retryAfterSeconds` was given.
pub const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
/// After this many failed OTP attempts the UI suggests resending.
pub const SUGGEST_RESEND_AFTER_FAILURES: u32 = 3;

// ---- Outbox dispatcher -------------------------------------------------------

/// Base delay for exponential backoff.
pub const BACKOFF_BASE: Duration = Duration::from_secs(2);
/// Upper bound for backoff delay.
pub const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);
/// Fallback poll interval when idle without a signal.
pub const FALLBACK_POLL: Duration = Duration::from_secs(15);
/// `IN_FLIGHT` rows older than this are considered stale and re-dispatched.
pub const IN_FLIGHT_STALE: Duration = Duration::from_secs(2 * 60);

// ---- Local database -----------------------------------------------------------

/// Room schema version implemented by the SQLite store.
pub const DB_SCHEMA_VERSION: i32 = 2;

// ---- Legal -------------------------------------------------------------------

/// Valid `key` values for `GET /v1/legal/{key}`.
pub const LEGAL_KEYS: [&str; 2] = ["privacy", "terms"];

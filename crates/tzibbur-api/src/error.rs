//! `AppError` hierarchy and RFC 7807 Problem+JSON mapping.
//!
//! The server returns `application/problem+json`; the suffix of `type` after the
//! last `:` selects the error subtype (`urn:tzibbur:error:invalid-display-name`
//! → [`AppError::InvalidDisplayName`]). Anything unmapped falls back to the HTTP
//! status class.

use crate::constants::{DEFAULT_MAX_DISPLAY_NAME, DEFAULT_MAX_GROUP_NAME, DEFAULT_MAX_MEMBERS};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

/// RFC 7807 problem document as emitted by the Tzibbur API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProblemDto {
    #[serde(default, rename = "type")]
    pub type_uri: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Field-level validation errors.
    #[serde(default)]
    pub errors: Option<Value>,
    /// Any extra members (e.g. `maxLength`, `maxMembers`, `retryAfterSeconds`).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ProblemDto {
    /// The slug after the last `:` in `type`, lower-cased.
    pub fn code(&self) -> Option<String> {
        let t = self.type_uri.as_deref()?;
        let slug = t.rsplit(':').next().unwrap_or(t);
        let slug = slug.rsplit('/').next().unwrap_or(slug);
        if slug.is_empty() || slug == "about:blank" {
            return None;
        }
        // Live server emits `validation_failed`, the app matched `validation-failed`.
        Some(slug.to_ascii_lowercase().replace('_', "-"))
    }

    fn extra_usize(&self, key: &str) -> Option<usize> {
        self.extra
            .get(key)
            .or_else(|| self.errors.as_ref()?.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
    }
}

/// All errors surfaced by this crate. The first group mirrors the Android
/// client's `AppError` sealed hierarchy one-to-one; the trailing variants cover
/// local failures (store, JSON, socket) that the mobile app handled elsewhere.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AppError {
    // ---- 400 ----
    #[error("validation failed{}", fmt_rid(request_id))]
    ValidationFailed {
        request_id: Option<String>,
        errors: Option<Value>,
    },
    #[error(
        "invalid display name (max {max_length} code points){}",
        fmt_rid(request_id)
    )]
    InvalidDisplayName {
        max_length: usize,
        request_id: Option<String>,
    },
    #[error("display name is reserved{}", fmt_rid(request_id))]
    ReservedDisplayName { request_id: Option<String> },
    #[error(
        "invalid group name (max {max_length} code points){}",
        fmt_rid(request_id)
    )]
    InvalidGroupName {
        max_length: usize,
        request_id: Option<String>,
    },
    #[error("invalid category{}", fmt_rid(request_id))]
    InvalidCategory { request_id: Option<String> },
    #[error("invalid message body{}", fmt_rid(request_id))]
    InvalidMessage { request_id: Option<String> },
    #[error("contacts batch too large{}", fmt_rid(request_id))]
    ContactsBatchTooLarge { request_id: Option<String> },
    // ---- 409 ----
    #[error("clientMessageId reused{}", fmt_rid(request_id))]
    ClientMessageIdReused { request_id: Option<String> },
    // ---- 401 ----
    #[error("unauthorized{}", fmt_rid(request_id))]
    Unauthorized { request_id: Option<String> },
    #[error("invalid OTP code{}", fmt_rid(request_id))]
    InvalidCode { request_id: Option<String> },
    // ---- 403 / 404 ----
    #[error("forbidden{}", fmt_rid(request_id))]
    Forbidden { request_id: Option<String> },
    #[error("not found{}", fmt_rid(request_id))]
    NotFound { request_id: Option<String> },
    // ---- 422 ----
    #[error("group is full (max {max_members} members){}", fmt_rid(request_id))]
    GroupFull {
        max_members: usize,
        request_id: Option<String>,
    },
    #[error("cannot demote or remove the last admin{}", fmt_rid(request_id))]
    LastAdmin { request_id: Option<String> },
    #[error("SMS delivery failed{}", fmt_rid(request_id))]
    SmsDeliveryFailed { request_id: Option<String> },
    #[error("group too small to post{}{}", min_members.map(|m| format!(" (needs {m} members)")).unwrap_or_default(), fmt_rid(request_id))]
    GroupTooSmall {
        min_members: Option<u32>,
        request_id: Option<String>,
    },
    // ---- 429 ----
    #[error("rate limited{}{}", retry_after_seconds.map(|s| format!(" (retry after {s}s)")).unwrap_or_default(), fmt_rid(request_id))]
    RateLimited {
        retry_after_seconds: Option<u64>,
        request_id: Option<String>,
    },
    // ---- 5xx ----
    #[error("not implemented{}", fmt_rid(request_id))]
    NotImplemented { request_id: Option<String> },
    #[error("internal server error{}", fmt_rid(request_id))]
    Internal { request_id: Option<String> },
    // ---- transport ----
    #[error("network error: {message}")]
    Network {
        message: String,
        #[source]
        cause: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
    },
    #[error("unknown error{}{}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default(), fmt_rid(request_id))]
    Unknown {
        status: Option<u16>,
        request_id: Option<String>,
        detail: Option<String>,
    },

    // ---- local (not part of the mobile AppError hierarchy) ----
    #[error("local store error: {0}")]
    Store(String),
    #[error("JSON error: {0}")]
    Json(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("protocol error: {code}{}", detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default())]
    Protocol {
        code: String,
        detail: Option<String>,
    },
    #[error("client is not signed in")]
    NotSignedIn,
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

fn fmt_rid(rid: &Option<String>) -> String {
    rid.as_ref()
        .map(|r| format!(" [requestId={r}]"))
        .unwrap_or_default()
}

impl AppError {
    /// `AppError.getRequestId()`.
    pub fn request_id(&self) -> Option<&str> {
        use AppError::*;
        match self {
            ValidationFailed { request_id, .. }
            | InvalidDisplayName { request_id, .. }
            | ReservedDisplayName { request_id }
            | InvalidGroupName { request_id, .. }
            | InvalidCategory { request_id }
            | InvalidMessage { request_id }
            | ContactsBatchTooLarge { request_id }
            | ClientMessageIdReused { request_id }
            | Unauthorized { request_id }
            | InvalidCode { request_id }
            | Forbidden { request_id }
            | NotFound { request_id }
            | GroupFull { request_id, .. }
            | LastAdmin { request_id }
            | SmsDeliveryFailed { request_id }
            | GroupTooSmall { request_id, .. }
            | RateLimited { request_id, .. }
            | NotImplemented { request_id }
            | Internal { request_id }
            | Unknown { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    /// HTTP status this error corresponds to, if any.
    pub fn status(&self) -> Option<u16> {
        use AppError::*;
        Some(match self {
            ValidationFailed { .. }
            | InvalidDisplayName { .. }
            | ReservedDisplayName { .. }
            | InvalidGroupName { .. }
            | InvalidCategory { .. }
            | InvalidMessage { .. }
            | ContactsBatchTooLarge { .. } => 400,
            Unauthorized { .. } | InvalidCode { .. } => 401,
            Forbidden { .. } => 403,
            NotFound { .. } => 404,
            ClientMessageIdReused { .. } | GroupTooSmall { .. } => 409,
            GroupFull { .. } | LastAdmin { .. } | SmsDeliveryFailed { .. } => 422,
            RateLimited { .. } => 429,
            NotImplemented { .. } => 501,
            Internal { .. } => 500,
            Unknown { status, .. } => return *status,
            _ => return None,
        })
    }

    /// Stable kebab-case code for this error (mirrors the server's `type` slugs);
    /// stored in `outbox.errorCode`.
    pub fn code(&self) -> &'static str {
        use AppError::*;
        match self {
            ValidationFailed { .. } => "validation-failed",
            InvalidDisplayName { .. } => "invalid-display-name",
            ReservedDisplayName { .. } => "reserved-display-name",
            InvalidGroupName { .. } => "invalid-group-name",
            InvalidCategory { .. } => "invalid-category",
            InvalidMessage { .. } => "invalid-message",
            ContactsBatchTooLarge { .. } => "contacts-batch-too-large",
            ClientMessageIdReused { .. } => "client-message-id-reused",
            Unauthorized { .. } => "unauthorized",
            InvalidCode { .. } => "invalid-code",
            Forbidden { .. } => "forbidden",
            NotFound { .. } => "not-found",
            GroupFull { .. } => "group-full",
            LastAdmin { .. } => "last-admin",
            SmsDeliveryFailed { .. } => "sms-delivery-failed",
            GroupTooSmall { .. } => "group-too-small",
            RateLimited { .. } => "rate-limited",
            NotImplemented { .. } => "not-implemented",
            Internal { .. } => "internal",
            Network { .. } => "network",
            Unknown { .. } => "unknown",
            Store(_) => "store",
            Json(_) => "json",
            WebSocket(_) => "websocket",
            Protocol { .. } => "protocol",
            NotSignedIn => "not-signed-in",
            InvalidInput(_) => "invalid-input",
        }
    }

    /// `true` for 401 `Unauthorized` — the mobile client wipes the session on this.
    pub fn invalidates_session(&self) -> bool {
        matches!(self, AppError::Unauthorized { .. })
    }

    /// `true` when the failure is transient and the request may be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AppError::Network { .. }
                | AppError::Internal { .. }
                | AppError::RateLimited { .. }
                | AppError::WebSocket(_)
                | AppError::Unknown {
                    status: Some(500..=599),
                    ..
                }
        )
    }

    /// Seconds to wait before retrying, when the server said.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            AppError::RateLimited {
                retry_after_seconds,
                ..
            } => *retry_after_seconds,
            _ => None,
        }
    }

    pub fn network(msg: impl Into<String>) -> Self {
        AppError::Network {
            message: msg.into(),
            cause: None,
        }
    }

    /// Map a Problem+JSON document (plus the response status and optional
    /// `Retry-After` header) to a typed error. This is `ErrorMapperKt`.
    pub fn from_problem(
        status: u16,
        problem: &ProblemDto,
        retry_after_header: Option<u64>,
    ) -> Self {
        let rid = problem.request_id.clone();
        let code = problem.code().unwrap_or_default();
        match code.as_str() {
            "validation-failed" | "validation" => AppError::ValidationFailed {
                request_id: rid,
                errors: problem.errors.clone(),
            },
            "invalid-display-name" => AppError::InvalidDisplayName {
                max_length: problem
                    .extra_usize("maxLength")
                    .unwrap_or(DEFAULT_MAX_DISPLAY_NAME),
                request_id: rid,
            },
            "reserved-display-name" => AppError::ReservedDisplayName { request_id: rid },
            "invalid-group-name" => AppError::InvalidGroupName {
                max_length: problem
                    .extra_usize("maxLength")
                    .unwrap_or(DEFAULT_MAX_GROUP_NAME),
                request_id: rid,
            },
            "invalid-category" => AppError::InvalidCategory { request_id: rid },
            "invalid-message" => AppError::InvalidMessage { request_id: rid },
            "contacts-batch-too-large" => AppError::ContactsBatchTooLarge { request_id: rid },
            "client-message-id-reused" => AppError::ClientMessageIdReused { request_id: rid },
            "unauthorized" => AppError::Unauthorized { request_id: rid },
            "invalid-code" => AppError::InvalidCode { request_id: rid },
            "forbidden" => AppError::Forbidden { request_id: rid },
            "not-found" => AppError::NotFound { request_id: rid },
            "group-full" => AppError::GroupFull {
                max_members: problem
                    .extra_usize("maxMembers")
                    .unwrap_or(DEFAULT_MAX_MEMBERS),
                request_id: rid,
            },
            "last-admin" => AppError::LastAdmin { request_id: rid },
            "sms-delivery-failed" => AppError::SmsDeliveryFailed { request_id: rid },
            "group-too-small" => AppError::GroupTooSmall {
                min_members: problem
                    .extra_usize("minMembers")
                    .or_else(|| problem.extra_usize("minMembersToPost"))
                    .map(|v| v as u32),
                request_id: rid,
            },
            "rate-limited" => AppError::RateLimited {
                retry_after_seconds: problem
                    .extra_usize("retryAfterSeconds")
                    .map(|v| v as u64)
                    .or(retry_after_header),
                request_id: rid,
            },
            "not-implemented" => AppError::NotImplemented { request_id: rid },
            "internal" => AppError::Internal { request_id: rid },
            _ => Self::from_status(
                status,
                rid,
                problem.detail.clone(),
                retry_after_header,
                problem.errors.clone(),
            ),
        }
    }

    /// Fallback mapping by HTTP status alone (non-problem bodies, or unknown `type`).
    pub fn from_status(
        status: u16,
        request_id: Option<String>,
        detail: Option<String>,
        retry_after_header: Option<u64>,
        errors: Option<Value>,
    ) -> Self {
        match status {
            400 => AppError::ValidationFailed { request_id, errors },
            401 => AppError::Unauthorized { request_id },
            403 => AppError::Forbidden { request_id },
            404 => AppError::NotFound { request_id },
            409 => AppError::Unknown {
                status: Some(409),
                request_id,
                detail,
            },
            429 => AppError::RateLimited {
                retry_after_seconds: retry_after_header,
                request_id,
            },
            501 => AppError::NotImplemented { request_id },
            500..=599 => AppError::Internal { request_id },
            s => AppError::Unknown {
                status: Some(s),
                request_id,
                detail,
            },
        }
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        let message = e.to_string();
        AppError::Network {
            message,
            cause: Some(Arc::new(e)),
        }
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Json(e.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Store(e.to_string())
    }
}

impl From<tungstenite::Error> for AppError {
    fn from(e: tungstenite::Error) -> Self {
        AppError::WebSocket(e.to_string())
    }
}

impl From<url::ParseError> for AppError {
    fn from(e: url::ParseError) -> Self {
        AppError::InvalidInput(format!("bad url: {e}"))
    }
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_by_type_suffix() {
        let p: ProblemDto = serde_json::from_str(
            r#"{"type":"urn:tzibbur:error:group-full","title":"x","status":422,"requestId":"r1","maxMembers":150}"#,
        )
        .unwrap();
        match AppError::from_problem(422, &p, None) {
            AppError::GroupFull {
                max_members,
                request_id,
            } => {
                assert_eq!(max_members, 150);
                assert_eq!(request_id.as_deref(), Some("r1"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_underscore_slugs() {
        let p: ProblemDto = serde_json::from_str(
            r#"{"type":"urn:tzibbur:error:validation_failed","title":"validation_failed","status":400,"detail":"Request validation failed","requestId":"r","errors":[{"path":"/id","message":"Invalid UUID"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            AppError::from_problem(400, &p, None),
            AppError::ValidationFailed { .. }
        ));
        let p: ProblemDto =
            serde_json::from_str(r#"{"type":"urn:tzibbur:error:not_found","status":404}"#).unwrap();
        assert!(matches!(
            AppError::from_problem(404, &p, None),
            AppError::NotFound { .. }
        ));
    }

    #[test]
    fn falls_back_to_status() {
        let p = ProblemDto::default();
        assert!(matches!(
            AppError::from_problem(401, &p, None),
            AppError::Unauthorized { .. }
        ));
        assert!(matches!(
            AppError::from_problem(503, &p, None),
            AppError::Internal { .. }
        ));
        assert!(matches!(
            AppError::from_problem(429, &p, Some(7)),
            AppError::RateLimited {
                retry_after_seconds: Some(7),
                ..
            }
        ));
    }
}

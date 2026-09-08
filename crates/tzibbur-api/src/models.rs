//! Wire DTOs and request bodies for the Tzibbur REST + WebSocket API.
//!
//! Field names follow the server's camelCase JSON. Where the reverse-engineered
//! reference did not pin down a shape (pagination envelopes, `contacts/check`
//! results, timestamps) the types here deserialize tolerantly.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deserialize an epoch-millisecond timestamp from either a JSON number or an
/// RFC 3339 / ISO-8601 string.
pub fn de_epoch_ms<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    epoch_ms_from_value(&v).ok_or_else(|| de::Error::custom(format!("invalid timestamp: {v}")))
}

/// Same as [`de_epoch_ms`] but for nullable fields.
pub fn de_epoch_ms_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    epoch_ms_from_value(&v)
        .map(Some)
        .ok_or_else(|| de::Error::custom(format!("invalid timestamp: {v}")))
}

pub(crate) fn epoch_ms_from_value(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                return Some(n);
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp_millis())
        }
        _ => None,
    }
}

/// Current wall-clock time in epoch milliseconds.
pub fn now_epoch_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Number of Unicode code points in `s` (the server counts code points, not bytes).
pub fn code_points(s: &str) -> usize {
    s.chars().count()
}

/// A page of results. Accepts either a bare JSON array or an object wrapping
/// the array under any key (`items`, `groups`, `members`, `messages`, …) with
/// an optional `nextCursor`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    /// `true` when the server explicitly signalled more results (`hasMore`), or a cursor was returned.
    pub has_more: bool,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
            has_more: false,
        }
    }
}

impl<'de, T: de::DeserializeOwned> Deserialize<'de> for Page<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        const LIST_KEYS: &[&str] = &[
            "items",
            "data",
            "results",
            "groups",
            "members",
            "messages",
            "devices",
            "registered",
            "events",
            "categories",
        ];
        const CURSOR_KEYS: &[&str] = &["nextCursor", "next_cursor", "cursor", "next"];
        let v = Value::deserialize(d)?;
        match v {
            Value::Array(_) => {
                let items: Vec<T> = serde_json::from_value(v).map_err(de::Error::custom)?;
                Ok(Page {
                    items,
                    next_cursor: None,
                    has_more: false,
                })
            }
            Value::Object(mut obj) => {
                let mut list = None;
                for k in LIST_KEYS {
                    if matches!(obj.get(*k), Some(Value::Array(_))) {
                        list = obj.remove(*k);
                        break;
                    }
                }
                if list.is_none() {
                    let key = obj
                        .iter()
                        .find(|(_, v)| v.is_array())
                        .map(|(k, _)| k.clone());
                    list = key.and_then(|k| obj.remove(&k));
                }
                let items: Vec<T> = match list {
                    Some(l) => serde_json::from_value(l).map_err(de::Error::custom)?,
                    None => Vec::new(),
                };
                let next_cursor = CURSOR_KEYS
                    .iter()
                    .find_map(|k| obj.get(*k))
                    .and_then(|v| v.as_str().map(str::to_owned));
                let has_more = obj
                    .get("hasMore")
                    .and_then(Value::as_bool)
                    .unwrap_or(next_cursor.is_some());
                Ok(Page {
                    items,
                    next_cursor,
                    has_more,
                })
            }
            Value::Null => Ok(Page::default()),
            other => Err(de::Error::custom(format!(
                "expected list or page object, got {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Group kind. `system` threads have no composer.
///
/// The live server uses lowercase (`"system"`); the decompiled app used
/// `SYSTEM`. Parsing is case-insensitive, serialization is lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GroupKind {
    Standard,
    System,
    #[default]
    Unknown,
}

impl GroupKind {
    /// Wire value (lowercase, as the server emits).
    pub fn as_str(self) -> &'static str {
        match self {
            GroupKind::Standard => "standard",
            GroupKind::System => "system",
            GroupKind::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "standard" => GroupKind::Standard,
            "system" => GroupKind::System,
            _ => GroupKind::Unknown,
        }
    }
}

impl Serialize for GroupKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for GroupKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(v.as_str().map(GroupKind::parse).unwrap_or_default())
    }
}

/// Member role within a group. Lowercase on the wire (`admin` / `member`);
/// parsing is case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Role {
    Admin,
    #[default]
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }
}

impl Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Role::parse(&raw).ok_or_else(|| de::Error::custom(format!("unknown role `{raw}`")))
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Account kind (`person` for users, `service` for system senders such as the
/// Tzibbur System thread's author).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    #[default]
    Person,
    Service,
    #[serde(untagged)]
    Other(String),
}

impl AccountKind {
    pub fn is_service(&self) -> bool {
        matches!(self, AccountKind::Service)
    }
}

/// Group permission setting (`whoCanPost`, `whoCanAddMembers`).
///
/// The concrete slugs were not present in the decompiled reference, so this is
/// an open string newtype with the two values the permission model implies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permission(pub String);

impl Permission {
    /// Every member may perform the action (observed on the live server).
    pub const EVERYONE: &'static str = "everyone";
    /// Only admins may perform the action.
    pub const ADMINS: &'static str = "admins";

    pub fn everyone() -> Self {
        Permission(Self::EVERYONE.into())
    }
    pub fn admins() -> Self {
        Permission(Self::ADMINS.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Whether a member holding `role` satisfies this permission.
    ///
    /// Admins always pass. Members pass unless the slug restricts to admins.
    pub fn allows(&self, role: Role) -> bool {
        if role == Role::Admin {
            return true;
        }
        let s = self.0.to_ascii_uppercase();
        !(s.contains("ADMIN") || s.contains("OWNER"))
    }
}

impl From<&str> for Permission {
    fn from(s: &str) -> Self {
        Permission(s.to_owned())
    }
}
impl From<String> for Permission {
    fn from(s: String) -> Self {
        Permission(s)
    }
}
impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Legal document keys accepted by `GET /v1/legal/{key}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegalDocKey {
    Privacy,
    Terms,
}

impl LegalDocKey {
    pub fn as_str(self) -> &'static str {
        match self {
            LegalDocKey::Privacy => "privacy",
            LegalDocKey::Terms => "terms",
        }
    }
}

/// UI theme override persisted in app prefs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ThemeOverride {
    #[default]
    System,
    Light,
    Dark,
}

// ---------------------------------------------------------------------------
// Auth / profile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default, alias = "phone")]
    pub phone_e164: Option<String>,
    /// `person` | `service`.
    #[serde(default)]
    pub kind: AccountKind,
    #[serde(
        default,
        deserialize_with = "de_epoch_ms_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default)]
    pub google_linked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// e.g. `"Pixel 7"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imei: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    /// Enrollment time (`registeredAt` on the wire).
    #[serde(
        default,
        alias = "registeredAt",
        deserialize_with = "de_epoch_ms_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<i64>,
    #[serde(
        default,
        deserialize_with = "de_epoch_ms_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_seen_at: Option<i64>,
    /// Any additional device fields the server returns.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Authenticated session as returned by `POST /v1/auth/verify`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub user: User,
    pub device: Device,
    pub token: String,
}

/// Response of `POST /v1/auth/start`. The live server does not send
/// `expiresAtEpochMs` as the decompiled app expected, so expiry is optional and
/// accepted under several names; unknown fields are kept in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentChallenge {
    #[serde(alias = "id", alias = "challenge")]
    pub challenge_id: String,
    #[serde(
        default,
        alias = "expiresAt",
        alias = "expires_at",
        alias = "expiresAtMs",
        alias = "expiry",
        deserialize_with = "de_epoch_ms_opt"
    )]
    pub expires_at_epoch_ms: Option<i64>,
    #[serde(default, alias = "expiresInSeconds", alias = "ttlSeconds")]
    pub expires_in_seconds: Option<u64>,
    #[serde(
        default,
        alias = "resendAfter",
        alias = "retryAfterSeconds",
        alias = "resendInSeconds"
    )]
    pub resend_after_seconds: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl EnrollmentChallenge {
    /// Best-effort expiry as epoch ms.
    pub fn expires_at(&self) -> Option<i64> {
        self.expires_at_epoch_ms.or_else(|| {
            self.expires_in_seconds
                .map(|s| now_epoch_ms() + (s as i64) * 1000)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartAuthRequest {
    /// E.164 international format.
    pub phone: String,
    /// Required on first registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// ISO 3166-1 alpha-2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// `kosher` | `android` | `ios` | `web` — required by the live server.
    /// Filled from the client's [`crate::DeviceInfo`] when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Device model string — required by the live server; filled from `DeviceInfo` when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VerifyAuthRequest {
    pub challenge_id: String,
    /// 6-digit OTP.
    pub code: String,
    pub phone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// See [`StartAuthRequest::platform`]; filled from `DeviceInfo` when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProfileRequest {
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactsCheckRequest {
    /// E.164 numbers, max 100.
    pub phones: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// One registered contact returned by `POST /v1/contacts/check`.
///
/// The live server returns `{"registered": ["+1555…", …]}` (plain E.164
/// strings); an object form with `phoneE164` / `userId` / `displayName` is
/// accepted too.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredContact {
    pub phone_e164: Option<String>,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
    pub extra: Map<String, Value>,
}

impl<'de> Deserialize<'de> for RegisteredContact {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        match v {
            Value::String(s) => Ok(RegisteredContact {
                phone_e164: Some(s),
                ..Default::default()
            }),
            Value::Object(mut o) => {
                let take_str = |o: &mut Map<String, Value>, keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| o.remove(*k))
                        .and_then(|v| v.as_str().map(str::to_owned))
                };
                Ok(RegisteredContact {
                    phone_e164: take_str(&mut o, &["phoneE164", "phone"]),
                    user_id: take_str(&mut o, &["userId", "id"]),
                    display_name: take_str(&mut o, &["displayName"]),
                    extra: o,
                })
            }
            other => Err(de::Error::custom(format!(
                "expected contact string or object, got {other}"
            ))),
        }
    }
}

impl RegisteredContact {
    pub fn phone(&self) -> Option<&str> {
        self.phone_e164.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Legal
// ---------------------------------------------------------------------------

/// Response of `GET /v1/legal/{key}`. The live server wraps the document as
/// `{"document": {"key", "text", "checksum"}}`; the wrapper is unwrapped here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "LegalDocumentWire")]
pub struct LegalDocument {
    #[serde(default)]
    pub key: Option<String>,
    /// Markdown body.
    #[serde(default, alias = "content", alias = "body", alias = "text")]
    pub markdown: String,
    #[serde(default, alias = "hash", alias = "version")]
    pub checksum: String,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    pub updated_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegalDocumentWire {
    #[serde(default)]
    document: Option<Box<LegalDocumentFlat>>,
    #[serde(flatten)]
    flat: LegalDocumentFlat,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LegalDocumentFlat {
    #[serde(default)]
    key: Option<String>,
    #[serde(default, alias = "content", alias = "body", alias = "text")]
    markdown: String,
    #[serde(default, alias = "hash", alias = "version")]
    checksum: String,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    updated_at: Option<i64>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl From<LegalDocumentWire> for LegalDocument {
    fn from(w: LegalDocumentWire) -> Self {
        let f = match w.document {
            Some(d) => *d,
            None => w.flat,
        };
        LegalDocument {
            key: f.key,
            markdown: f.markdown,
            checksum: f.checksum,
            updated_at: f.updated_at,
            extra: f.extra,
        }
    }
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// Server-imposed limits for a group (`limits` on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GroupLimits {
    #[serde(default)]
    pub member_cap: Option<u32>,
    /// Observed as 1000 on the live server (the app's compiled default is 2000).
    #[serde(default)]
    pub message_max_length: Option<u32>,
    #[serde(default)]
    pub min_members_to_post: Option<u32>,
}

/// `settings` object on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GroupSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub who_can_post: Option<Permission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub who_can_add_members: Option<Permission>,
}

/// A group as returned by the server. Live shape:
///
/// ```json
/// {"id","name","category","kind":"system","createdBy","createdAt":"…Z",
///  "role":"member","memberCount":2,"muted":false,"readSeq":0,"unreadCount":1,
///  "settings":{"whoCanPost":"everyone","whoCanAddMembers":"everyone"},
///  "limits":{"memberCap":100,"messageMaxLength":1000,"minMembersToPost":0}}
/// ```
///
/// Top-level `whoCanPost` / `myRole` / `lastReadSeq` (the app's DTO names) are
/// accepted as well and flattened into the same fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "GroupDtoWire")]
pub struct GroupDto {
    pub id: String,
    pub name: String,
    pub category: String,
    pub kind: GroupKind,
    pub who_can_post: Permission,
    pub who_can_add_members: Permission,
    pub created_by: Option<String>,
    pub created_at: Option<i64>,
    /// `role` on the wire.
    pub my_role: Role,
    pub member_count: i64,
    pub muted: bool,
    pub last_activity_at: Option<i64>,
    pub last_message_preview: Option<String>,
    /// `readSeq` on the wire: this device's read bookmark.
    pub last_read_seq: Option<i64>,
    /// Server-computed unread count, when returned.
    pub unread_count: Option<i64>,
    /// Server-side latest sequence number, when returned.
    pub latest_seq: Option<i64>,
    pub limits: Option<GroupLimits>,
    pub extra: Map<String, Value>,
}

impl GroupDto {
    /// Effective max message length: the group's `limits.messageMaxLength`,
    /// else the app default.
    pub fn message_max_length(&self) -> usize {
        self.limits
            .as_ref()
            .and_then(|l| l.message_max_length)
            .map(|v| v as usize)
            .unwrap_or(crate::constants::DEFAULT_MAX_MESSAGE)
    }
    pub fn member_cap(&self) -> usize {
        self.limits
            .as_ref()
            .and_then(|l| l.member_cap)
            .map(|v| v as usize)
            .unwrap_or(crate::constants::DEFAULT_MAX_MEMBERS)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupDtoWire {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    kind: GroupKind,
    #[serde(default)]
    settings: GroupSettings,
    #[serde(default)]
    who_can_post: Option<Permission>,
    #[serde(default)]
    who_can_add_members: Option<Permission>,
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    created_at: Option<i64>,
    #[serde(default, alias = "role")]
    my_role: Role,
    #[serde(default)]
    member_count: i64,
    #[serde(default)]
    muted: bool,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    last_activity_at: Option<i64>,
    #[serde(default)]
    last_message_preview: Option<String>,
    #[serde(default, alias = "readSeq")]
    last_read_seq: Option<i64>,
    #[serde(default)]
    unread_count: Option<i64>,
    #[serde(default, alias = "lastSeq", alias = "maxSeq")]
    latest_seq: Option<i64>,
    #[serde(default)]
    limits: Option<GroupLimits>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl From<GroupDtoWire> for GroupDto {
    fn from(w: GroupDtoWire) -> Self {
        GroupDto {
            id: w.id,
            name: w.name,
            category: w.category,
            kind: w.kind,
            who_can_post: w
                .settings
                .who_can_post
                .or(w.who_can_post)
                .unwrap_or_else(Permission::everyone),
            who_can_add_members: w
                .settings
                .who_can_add_members
                .or(w.who_can_add_members)
                .unwrap_or_else(Permission::admins),
            created_by: w.created_by,
            created_at: w.created_at,
            my_role: w.my_role,
            member_count: w.member_count,
            muted: w.muted,
            last_activity_at: w.last_activity_at,
            last_message_preview: w.last_message_preview,
            last_read_seq: w.last_read_seq,
            unread_count: w.unread_count,
            latest_seq: w.latest_seq,
            limits: w.limits,
            extra: w.extra,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupRequest {
    /// Max 100 code points.
    pub name: String,
    /// Must be a slug from `GET /v1/groups/categories` (`family`, `neighborhood`, `shul`, `school`, `other`).
    pub category: String,
    pub kind: GroupKind,
    pub who_can_post: Permission,
    pub who_can_add_members: Permission,
}

impl CreateGroupRequest {
    /// A standard group where everyone may post and only admins add members.
    pub fn standard(name: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            category: category.into(),
            kind: GroupKind::Standard,
            who_can_post: Permission::everyone(),
            who_can_add_members: Permission::admins(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGroupRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who_can_post: Option<Permission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who_can_add_members: Option<Permission>,
}

/// Cached in `AppPrefsStore` under `CATEGORIES_JSON`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CategoriesEnvelope {
    pub categories: Vec<String>,
    /// RFC 3339 timestamp of the fetch.
    pub fetched_at: String,
}

/// Cursor pagination parameters (`GET /v1/groups`, `GET /v1/groups/{id}/members`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl ListParams {
    pub fn limit(limit: u32) -> Self {
        Self {
            cursor: None,
            limit: Some(limit),
        }
    }
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberDto {
    #[serde(alias = "id")]
    pub user_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default, alias = "phone")]
    pub phone_e164: Option<String>,
    #[serde(default)]
    pub role: Role,
    /// `person` | `service`.
    #[serde(default)]
    pub kind: AccountKind,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    pub joined_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMembersRequest {
    pub phones: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// An entry in `AddMembersOutcome.added`: either a full member or just a phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AddedMember {
    Member(MemberDto),
    Phone(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AddMembersOutcome {
    #[serde(default)]
    pub added: Vec<AddedMember>,
    #[serde(default)]
    pub not_found: Vec<String>,
    #[serde(default)]
    pub already_member: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRoleRequest {
    pub role: Role,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: String,
    /// Present in REST responses; may be absent inside a WS `messages` frame (the frame carries it).
    #[serde(default)]
    pub group_id: Option<String>,
    pub seq: i64,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub body: String,
    /// Set when this device sent the message (echo detection).
    #[serde(default)]
    pub client_message_id: Option<String>,
    #[serde(default, deserialize_with = "de_epoch_ms_opt")]
    pub created_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Query for `GET /v1/groups/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessagesQuery {
    /// Exclusive lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<i64>,
    /// Exclusive upper bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MessagesQuery {
    pub fn after(seq: i64) -> Self {
        Self {
            after_seq: Some(seq),
            ..Default::default()
        }
    }
    pub fn before(seq: i64) -> Self {
        Self {
            before_seq: Some(seq),
            ..Default::default()
        }
    }
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Response of `GET /v1/groups/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPage {
    #[serde(default, alias = "messages")]
    pub items: Vec<MessageDto>,
    /// Pass as `afterSeq` to continue forwards; `null` when caught up.
    #[serde(default)]
    pub next_after_seq: Option<i64>,
    /// Pass as `beforeSeq` to continue backwards; `null` at the beginning.
    #[serde(default)]
    pub next_before_seq: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// UUID used for deduplication and echo detection.
    pub client_message_id: String,
    /// Max 2000 code points.
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckRequest {
    pub seq: i64,
}

// ---------------------------------------------------------------------------
// Pending catch-up
// ---------------------------------------------------------------------------

/// Messages for one group inside a `pending` response or a WS `messages` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMessages {
    pub group_id: String,
    #[serde(default)]
    pub messages: Vec<MessageDto>,
    /// Seq this device had been delivered up to before this batch (`pending` only).
    #[serde(default)]
    pub delivered_seq: Option<i64>,
    /// More messages exist beyond this batch for the group; page with `GET …/messages?afterSeq=`.
    #[serde(default)]
    pub has_more: bool,
}

/// A group lifecycle event (also the payload of the WS `group` frame).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupEventDto {
    pub event: String,
    #[serde(default)]
    pub payload: Map<String, Value>,
}

/// Response of `GET /v1/pending`. Live shape:
///
/// ```json
/// {"groups":[{"groupId":"…","deliveredSeq":0,"hasMore":false,"messages":[…]}]}
/// ```
///
/// Also tolerates `messages` as a list of buckets or a flat list of messages
/// carrying `groupId`, and `groups` as a list of full group objects.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PendingResponse {
    pub messages: Vec<GroupMessages>,
    pub groups: Vec<GroupDto>,
    pub events: Vec<GroupEventDto>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub extra: Map<String, Value>,
}

impl PendingResponse {
    /// Total number of messages across all groups.
    pub fn message_count(&self) -> usize {
        self.messages.iter().map(|g| g.messages.len()).sum()
    }
}

impl<'de> Deserialize<'de> for PendingResponse {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let mut obj = match v {
            Value::Object(o) => o,
            Value::Array(items) => {
                let mut o = Map::new();
                o.insert("messages".into(), Value::Array(items));
                o
            }
            Value::Null => return Ok(Self::default()),
            other => {
                return Err(de::Error::custom(format!(
                    "expected pending object, got {other}"
                )))
            }
        };
        let mut out = PendingResponse::default();
        if let Some(m) = obj.remove("messages").or_else(|| obj.remove("items")) {
            out.messages = parse_message_buckets(m).map_err(de::Error::custom)?;
        }
        if let Some(g) = obj.remove("groups") {
            // Server's /v1/pending wraps message buckets under "groups" (shape:
            // {groupId, deliveredSeq, messages, hasMore}). Detect by presence of
            // "groupId"; fall back to GroupDto list for other callers.
            let is_pending_buckets = matches!(&g,
                Value::Array(arr) if arr.first().and_then(|v| v.get("groupId")).is_some()
            );
            if is_pending_buckets {
                let msgs = parse_message_buckets(g).map_err(de::Error::custom)?;
                out.messages.extend(msgs);
            } else {
                out.groups = serde_json::from_value(g).map_err(de::Error::custom)?;
            }
        }
        if let Some(e) = obj.remove("events") {
            out.events = serde_json::from_value(e).map_err(de::Error::custom)?;
        }
        out.next_cursor = obj
            .remove("nextCursor")
            .or_else(|| obj.remove("cursor"))
            .and_then(|v| v.as_str().map(str::to_owned));
        out.has_more = obj
            .remove("hasMore")
            .and_then(|v| v.as_bool())
            .unwrap_or(out.next_cursor.is_some());
        out.extra = obj;
        Ok(out)
    }
}

fn parse_message_buckets(v: Value) -> Result<Vec<GroupMessages>, serde_json::Error> {
    let arr = match v {
        Value::Array(a) => a,
        Value::Null => return Ok(Vec::new()),
        other => return serde_json::from_value(other),
    };
    // Bucketed form: [{groupId, messages:[...]}, ...]
    let bucketed = arr
        .iter()
        .all(|x| x.get("messages").map(Value::is_array).unwrap_or(false));
    if bucketed {
        return serde_json::from_value(Value::Array(arr));
    }
    // Flat form: [{id, groupId, seq, ...}, ...] → bucket by groupId, preserving order.
    let flat: Vec<MessageDto> = serde_json::from_value(Value::Array(arr))?;
    let mut order: Vec<String> = Vec::new();
    let mut buckets: BTreeMap<String, Vec<MessageDto>> = BTreeMap::new();
    for m in flat {
        let gid = m.group_id.clone().unwrap_or_default();
        if !buckets.contains_key(&gid) {
            order.push(gid.clone());
        }
        buckets.entry(gid).or_default().push(m);
    }
    Ok(order
        .into_iter()
        .map(|gid| GroupMessages {
            messages: buckets.remove(&gid).unwrap_or_default(),
            group_id: gid,
            delivered_seq: None,
            has_more: false,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_accepts_array_and_object() {
        let a: Page<GroupDto> = serde_json::from_str(r#"[{"id":"g1","name":"A"}]"#).unwrap();
        assert_eq!(a.items.len(), 1);
        let b: Page<GroupDto> =
            serde_json::from_str(r#"{"groups":[{"id":"g1","name":"A"}],"nextCursor":"c2"}"#)
                .unwrap();
        assert_eq!(b.items[0].id, "g1");
        assert_eq!(b.next_cursor.as_deref(), Some("c2"));
        assert!(b.has_more);
    }

    #[test]
    fn epoch_accepts_number_and_iso() {
        let m: MessageDto = serde_json::from_str(
            r#"{"id":"m","seq":1,"senderId":"u","body":"x","createdAt":"2026-01-02T03:04:05Z"}"#,
        )
        .unwrap();
        assert_eq!(m.created_at, Some(1767323045000));
        let m2: MessageDto =
            serde_json::from_str(r#"{"id":"m","seq":1,"senderId":"u","body":"x","createdAt":5}"#)
                .unwrap();
        assert_eq!(m2.created_at, Some(5));
    }

    #[test]
    fn pending_flat_and_bucketed() {
        let p: PendingResponse = serde_json::from_str(
            r#"{"messages":[{"id":"1","groupId":"g","seq":1,"senderId":"u","body":"a"},{"id":"2","groupId":"h","seq":1,"senderId":"u","body":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(p.messages.len(), 2);
        let q: PendingResponse = serde_json::from_str(
            r#"{"messages":[{"groupId":"g","messages":[{"id":"1","seq":1,"senderId":"u","body":"a"}]}],"hasMore":true}"#,
        )
        .unwrap();
        assert_eq!(q.message_count(), 1);
        assert!(q.has_more);
    }

    #[test]
    fn live_group_shape() {
        let g: GroupDto = serde_json::from_str(
            r#"{"category":"other","createdAt":"2026-09-08T01:34:12.708Z","createdBy":"u","id":"g","kind":"system",
                "limits":{"memberCap":100,"messageMaxLength":1000,"minMembersToPost":0},"memberCount":2,"muted":false,
                "name":"Tzibbur System","readSeq":0,"role":"member","settings":{"whoCanAddMembers":"everyone","whoCanPost":"everyone"},"unreadCount":1}"#,
        )
        .unwrap();
        assert_eq!(g.kind, GroupKind::System);
        assert_eq!(g.my_role, Role::Member);
        assert_eq!(g.who_can_post, Permission::everyone());
        assert_eq!(g.last_read_seq, Some(0));
        assert_eq!(g.unread_count, Some(1));
        assert_eq!(g.message_max_length(), 1000);
        assert!(g.extra.is_empty(), "{:?}", g.extra);
        // App-style uppercase still parses.
        let m: MemberDto =
            serde_json::from_str(r#"{"userId":"u","role":"ADMIN","kind":"service"}"#).unwrap();
        assert_eq!(m.role, Role::Admin);
        assert!(m.kind.is_service());
        assert_eq!(serde_json::to_string(&Role::Admin).unwrap(), "\"admin\"");
    }

    #[test]
    fn live_pending_legal_contacts() {
        let p: PendingResponse = serde_json::from_str(
            r#"{"groups":[{"deliveredSeq":0,"groupId":"g","hasMore":true,"messages":[{"id":"m","groupId":"g","seq":1,"senderId":"s","body":"b","createdAt":"2026-09-08T01:34:12.758Z"}]}]}"#,
        )
        .unwrap();
        assert_eq!(p.messages.len(), 1);
        assert!(p.groups.is_empty());
        assert!(p.messages[0].has_more);
        assert_eq!(p.messages[0].delivered_seq, Some(0));
        let l: LegalDocument =
            serde_json::from_str(r##"{"document":{"checksum":"abc","key":"terms","text":"# T"}}"##)
                .unwrap();
        assert_eq!(
            (l.checksum.as_str(), l.markdown.as_str(), l.key.as_deref()),
            ("abc", "# T", Some("terms"))
        );
        let c: Page<RegisteredContact> =
            serde_json::from_str(r#"{"registered":["+1440"]}"#).unwrap();
        assert_eq!(c.items[0].phone_e164.as_deref(), Some("+1440"));
    }

    #[test]
    fn permission_allows() {
        assert!(Permission::everyone().allows(Role::Member));
        assert!(!Permission::admins().allows(Role::Member));
        assert!(Permission::admins().allows(Role::Admin));
        assert!(!Permission::from("admins_only").allows(Role::Member));
    }
}

//! REST client for `https://api.tzibbur.me`.
//!
//! Every authenticated call sends `Authorization: Bearer {token}`. Errors are
//! parsed from RFC 7807 bodies via [`AppError::from_problem`]. A 401 on any
//! request notifies the registered [`SessionInvalidationListener`] (the
//! mobile client wipes the session in response).

use crate::constants::{DEFAULT_BASE_URL, DEFAULT_MAX_PHONES};
use crate::error::{AppError, ProblemDto, Result};
use crate::models::*;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use url::Url;

/// Notified when the server rejects the bearer token (HTTP 401 or WS handshake 401).
pub trait SessionInvalidationListener: Send + Sync {
    fn on_session_invalidated(&self);
}

impl<F: Fn() + Send + Sync> SessionInvalidationListener for F {
    fn on_session_invalidated(&self) {
        self()
    }
}

/// How the client presents itself to the server (what shows up under
/// `GET /v1/me/devices` as `platform` / `deviceModel`). Defaults mimic the
/// Android app so the bridge registers as an Android device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `android` | `ios` | `web` | `kosher` …
    pub platform: String,
    /// e.g. `Pixel 7`
    pub model: String,
    /// App version the server sees, e.g. `0.1.0`
    pub app_version: String,
    /// OS version string, e.g. `14`
    pub os_version: String,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self::android()
    }
}

impl DeviceInfo {
    /// The reverse-engineered app: `com.tzibbur.app` 0.1.0 on a Pixel 7 / Android 14.
    pub fn android() -> Self {
        Self {
            platform: "android".into(),
            model: "Pixel 7".into(),
            app_version: "0.1.0".into(),
            os_version: "14".into(),
        }
    }

    /// `Tzibbur/0.1.0 (Android 14; Pixel 7) Ktor`
    pub fn user_agent(&self) -> String {
        let os = match self.platform.as_str() {
            "android" => "Android",
            "ios" => "iOS",
            other => other,
        };
        format!(
            "Tzibbur/{} ({} {}; {}) Ktor",
            self.app_version, os, self.os_version, self.model
        )
    }

    /// Extra headers sent with every request and the WS handshake.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("X-Platform", self.platform.clone()),
            ("X-Device-Platform", self.platform.clone()),
            ("X-Device-Model", self.model.clone()),
            ("X-App-Version", self.app_version.clone()),
            ("X-OS-Version", self.os_version.clone()),
        ]
    }
}

/// Builder for [`TzibburClient`].
pub struct ClientBuilder {
    base_url: String,
    token: Option<String>,
    user_agent: String,
    timeout: Duration,
    listener: Option<Arc<dyn SessionInvalidationListener>>,
    http: Option<reqwest::Client>,
    device: DeviceInfo,
    user_agent_override: bool,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        let device = DeviceInfo::default();
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            token: None,
            user_agent: device.user_agent(),
            timeout: Duration::from_secs(30),
            listener: None,
            http: None,
            device,
            user_agent_override: false,
        }
    }
}

impl ClientBuilder {
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
    /// Override the User-Agent (by default it is derived from [`DeviceInfo`]).
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self.user_agent_override = true;
        self
    }
    /// Present as this device (platform / model / versions). Defaults to Android.
    pub fn device(mut self, device: DeviceInfo) -> Self {
        if !self.user_agent_override {
            self.user_agent = device.user_agent();
        }
        self.device = device;
        self
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
    pub fn session_listener(mut self, l: Arc<dyn SessionInvalidationListener>) -> Self {
        self.listener = Some(l);
        self
    }
    /// Supply a pre-built reqwest client (proxy, custom TLS, …).
    pub fn http_client(mut self, c: reqwest::Client) -> Self {
        self.http = Some(c);
        self
    }
    pub fn build(self) -> Result<TzibburClient> {
        let mut base = Url::parse(&self.base_url)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = match self.http {
            Some(c) => c,
            None => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    ACCEPT,
                    HeaderValue::from_static("application/json, application/problem+json"),
                );
                reqwest::Client::builder()
                    .default_headers(headers)
                    .timeout(self.timeout)
                    .user_agent(self.user_agent.clone())
                    .build()?
            }
        };
        Ok(TzibburClient {
            inner: Arc::new(Inner {
                http,
                base,
                token: RwLock::new(self.token),
                listener: self.listener,
                user_agent: self.user_agent,
                device: self.device,
            }),
        })
    }
}

struct Inner {
    http: reqwest::Client,
    base: Url,
    token: RwLock<Option<String>>,
    listener: Option<Arc<dyn SessionInvalidationListener>>,
    user_agent: String,
    device: DeviceInfo,
}

/// Cheaply clonable REST client.
#[derive(Clone)]
pub struct TzibburClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TzibburClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TzibburClient")
            .field("base", &self.inner.base.as_str())
            .finish()
    }
}

impl TzibburClient {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Client against the production base URL with no token.
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    pub fn base_url(&self) -> &Url {
        &self.inner.base
    }

    pub fn user_agent(&self) -> &str {
        &self.inner.user_agent
    }

    /// The device identity presented to the server.
    pub fn device(&self) -> &DeviceInfo {
        &self.inner.device
    }

    /// Replace (or clear) the bearer token used for subsequent requests.
    pub async fn set_token(&self, token: Option<String>) {
        *self.inner.token.write().await = token;
    }

    pub async fn token(&self) -> Option<String> {
        self.inner.token.read().await.clone()
    }

    pub async fn is_signed_in(&self) -> bool {
        self.inner.token.read().await.is_some()
    }

    /// Fire the registered [`SessionInvalidationListener`], if any.
    pub fn notify_session_invalidated(&self) {
        if let Some(l) = &self.inner.listener {
            l.on_session_invalidated();
        }
    }

    /// WebSocket URL derived from the base URL (`wss://host/v1/ws`).
    pub fn ws_url(&self) -> Result<Url> {
        let mut u = self.inner.base.join("v1/ws")?;
        let scheme = match u.scheme() {
            "https" => "wss",
            "http" => "ws",
            s => s,
        }
        .to_owned();
        u.set_scheme(&scheme)
            .map_err(|_| AppError::InvalidInput("cannot derive ws scheme".into()))?;
        Ok(u)
    }

    // -----------------------------------------------------------------------
    // Core request plumbing
    // -----------------------------------------------------------------------

    fn url(&self, path: &str) -> Result<Url> {
        Ok(self.inner.base.join(path.trim_start_matches('/'))?)
    }

    async fn send<B: Serialize + ?Sized, Q: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: Option<&Q>,
        body: Option<&B>,
        auth: bool,
    ) -> Result<reqwest::Response> {
        let url = self.url(path)?;
        let mut req = self.inner.http.request(method.clone(), url.clone());
        if let Some(q) = query {
            req = req.query(q);
        }
        if let Some(b) = body {
            req = req.header(CONTENT_TYPE, "application/json").json(b);
        }
        if auth {
            let tok = self
                .inner
                .token
                .read()
                .await
                .clone()
                .ok_or(AppError::NotSignedIn)?;
            req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
        }
        req = req.header(USER_AGENT, &self.inner.user_agent);
        for (k, v) in self.inner.device.headers() {
            req = req.header(k, v);
        }
        tracing::debug!(%method, %url, "tzibbur request");
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let err = self.map_error(resp).await;
        if err.invalidates_session() {
            self.notify_session_invalidated();
        }
        Err(err)
    }

    async fn map_error(&self, resp: reqwest::Response) -> AppError {
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let request_id_hdr = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = resp.text().await.unwrap_or_default();
        let mut problem: ProblemDto = serde_json::from_str(&text).unwrap_or_default();
        if problem.request_id.is_none() {
            problem.request_id = request_id_hdr;
        }
        if problem.detail.is_none() && !text.is_empty() && problem.type_uri.is_none() {
            problem.detail = Some(text.chars().take(512).collect());
        }
        tracing::warn!(status = status.as_u16(), code = ?problem.code(), "tzibbur error response");
        AppError::from_problem(status.as_u16(), &problem, retry_after)
    }

    async fn json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        if resp.status() == StatusCode::NO_CONTENT {
            return serde_json::from_value(serde_json::Value::Null).map_err(Into::into);
        }
        let bytes = resp.bytes().await?;
        if bytes.is_empty() {
            return serde_json::from_value(serde_json::Value::Null).map_err(Into::into);
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            let keys = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                })
                .unwrap_or_else(|| format!("{} bytes, non-object", bytes.len()));
            tracing::warn!(error = %e, keys, "tzibbur: response did not match the expected shape");
            AppError::Json(e.to_string())
        })
    }

    async fn get<T: DeserializeOwned, Q: Serialize + ?Sized>(
        &self,
        path: &str,
        query: Option<&Q>,
    ) -> Result<T> {
        let r = self
            .send::<(), Q>(Method::GET, path, query, None, true)
            .await?;
        Self::json(r).await
    }

    async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
        auth: bool,
    ) -> Result<T> {
        let r = self
            .send::<B, ()>(Method::POST, path, None, Some(body), auth)
            .await?;
        Self::json(r).await
    }

    async fn patch<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let r = self
            .send::<B, ()>(Method::PATCH, path, None, Some(body), true)
            .await?;
        Self::json(r).await
    }

    async fn delete_(&self, path: &str) -> Result<()> {
        self.send::<(), ()>(Method::DELETE, path, None, None, true)
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Auth
    // -----------------------------------------------------------------------

    /// `POST /v1/auth/start` — triggers SMS OTP delivery. Unauthenticated.
    pub async fn start_auth(&self, req: &StartAuthRequest) -> Result<EnrollmentChallenge> {
        let mut req = req.clone();
        req.platform
            .get_or_insert_with(|| self.inner.device.platform.clone());
        req.device_model
            .get_or_insert_with(|| self.inner.device.model.clone());
        self.post("v1/auth/start", &req, false).await
    }

    /// `POST /v1/auth/verify` — exchanges the OTP for a session. Unauthenticated.
    ///
    /// On success the returned token is installed on this client.
    pub async fn verify_auth(&self, req: &VerifyAuthRequest) -> Result<Session> {
        let mut req = req.clone();
        req.platform
            .get_or_insert_with(|| self.inner.device.platform.clone());
        req.device_model
            .get_or_insert_with(|| self.inner.device.model.clone());
        let s: Session = self.post("v1/auth/verify", &req, false).await?;
        self.set_token(Some(s.token.clone())).await;
        Ok(s)
    }

    // -----------------------------------------------------------------------
    // Profile
    // -----------------------------------------------------------------------

    /// `GET /v1/me`
    pub async fn me(&self) -> Result<User> {
        self.get::<User, ()>("v1/me", None).await
    }

    /// `PATCH /v1/me` — update display name (max 64 code points).
    pub async fn update_display_name(&self, display_name: &str) -> Result<User> {
        let body = UpdateProfileRequest {
            display_name: display_name.to_owned(),
        };
        // Server may answer 204; fall back to a fresh GET in that case.
        match self.patch::<Option<User>, _>("v1/me", &body).await? {
            Some(u) => Ok(u),
            None => self.me().await,
        }
    }

    /// `GET /v1/me/devices`
    pub async fn devices(&self) -> Result<Vec<Device>> {
        Ok(self
            .get::<Page<Device>, ()>("v1/me/devices", None)
            .await?
            .items)
    }

    // -----------------------------------------------------------------------
    // Contacts
    // -----------------------------------------------------------------------

    /// `POST /v1/contacts/check` — which of `phones` (max 100) are registered.
    pub async fn check_contacts(
        &self,
        phones: &[String],
        region: Option<&str>,
    ) -> Result<Vec<RegisteredContact>> {
        if phones.len() > DEFAULT_MAX_PHONES {
            return Err(AppError::ContactsBatchTooLarge { request_id: None });
        }
        let body = ContactsCheckRequest {
            phones: phones.to_vec(),
            region: region.map(str::to_owned),
        };
        Ok(self
            .post::<Page<RegisteredContact>, _>("v1/contacts/check", &body, true)
            .await?
            .items)
    }

    /// Convenience: splits into ≤100 batches and concatenates.
    pub async fn check_contacts_batched(
        &self,
        phones: &[String],
        region: Option<&str>,
    ) -> Result<Vec<RegisteredContact>> {
        let mut out = Vec::new();
        for chunk in phones.chunks(DEFAULT_MAX_PHONES) {
            out.extend(self.check_contacts(chunk, region).await?);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Legal
    // -----------------------------------------------------------------------

    /// `GET /v1/legal/{key}`
    pub async fn legal(&self, key: LegalDocKey) -> Result<LegalDocument> {
        let raw: serde_json::Value = self
            .get::<_, ()>(&format!("v1/legal/{}", key.as_str()), None)
            .await?;
        // Server wraps the document in {"document": {...}}; unwrap if present.
        let inner = if let Some(doc) = raw.get("document").cloned() {
            doc
        } else {
            raw
        };
        let mut doc: LegalDocument = serde_json::from_value(inner)?;
        doc.key.get_or_insert_with(|| key.as_str().to_owned());
        Ok(doc)
    }

    // -----------------------------------------------------------------------
    // Groups
    // -----------------------------------------------------------------------

    /// `POST /v1/groups`
    pub async fn create_group(&self, req: &CreateGroupRequest) -> Result<GroupDto> {
        self.post("v1/groups", req, true).await
    }

    /// `GET /v1/groups` — one page.
    pub async fn list_groups(&self, params: &ListParams) -> Result<Page<GroupDto>> {
        self.get("v1/groups", Some(params)).await
    }

    /// Follows cursors until exhausted.
    pub async fn list_all_groups(&self) -> Result<Vec<GroupDto>> {
        let mut out = Vec::new();
        let mut params = ListParams::default();
        loop {
            let page = self.list_groups(&params).await?;
            out.extend(page.items);
            match page.next_cursor {
                Some(c) if page.has_more || !c.is_empty() => params.cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }

    /// `GET /v1/groups/categories`
    pub async fn group_categories(&self) -> Result<CategoriesEnvelope> {
        let page: Page<String> = self.get::<_, ()>("v1/groups/categories", None).await?;
        Ok(CategoriesEnvelope {
            categories: page.items,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// `GET /v1/groups/{id}`. A 404 maps to [`AppError::NotFound`]; callers
    /// (see `SyncEngine`) mark the group deleted locally in that case.
    pub async fn get_group(&self, id: &str) -> Result<GroupDto> {
        self.get::<_, ()>(&format!("v1/groups/{id}"), None).await
    }

    /// `PATCH /v1/groups/{id}`
    pub async fn update_group(
        &self,
        id: &str,
        req: &UpdateGroupRequest,
    ) -> Result<Option<GroupDto>> {
        self.patch(&format!("v1/groups/{id}"), req).await
    }

    /// `DELETE /v1/groups/{id}` (admin only).
    pub async fn delete_group(&self, id: &str) -> Result<()> {
        self.delete_(&format!("v1/groups/{id}")).await
    }

    // -----------------------------------------------------------------------
    // Members
    // -----------------------------------------------------------------------

    /// `GET /v1/groups/{id}/members` — one page.
    pub async fn list_members(
        &self,
        group_id: &str,
        params: &ListParams,
    ) -> Result<Page<MemberDto>> {
        self.get(&format!("v1/groups/{group_id}/members"), Some(params))
            .await
    }

    /// Follows cursors until exhausted.
    pub async fn list_all_members(&self, group_id: &str) -> Result<Vec<MemberDto>> {
        let mut out = Vec::new();
        let mut params = ListParams::default();
        loop {
            let page = self.list_members(group_id, &params).await?;
            out.extend(page.items);
            match page.next_cursor {
                Some(c) if !c.is_empty() => params.cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }

    /// `POST /v1/groups/{id}/members` — add by phone number, max 100 per batch.
    pub async fn add_members(
        &self,
        group_id: &str,
        phones: &[String],
        region: Option<&str>,
    ) -> Result<AddMembersOutcome> {
        if phones.len() > DEFAULT_MAX_PHONES {
            return Err(AppError::ContactsBatchTooLarge { request_id: None });
        }
        let body = AddMembersRequest {
            phones: phones.to_vec(),
            region: region.map(str::to_owned),
        };
        self.post(&format!("v1/groups/{group_id}/members"), &body, true)
            .await
    }

    /// `DELETE /v1/groups/{id}/members/{userId}`
    pub async fn remove_member(&self, group_id: &str, user_id: &str) -> Result<()> {
        self.delete_(&format!("v1/groups/{group_id}/members/{user_id}"))
            .await
    }

    /// `PATCH /v1/groups/{id}/members/{userId}` — set role. Demoting the last
    /// admin yields [`AppError::LastAdmin`].
    pub async fn set_member_role(&self, group_id: &str, user_id: &str, role: Role) -> Result<()> {
        let _: Option<serde_json::Value> = self
            .patch(
                &format!("v1/groups/{group_id}/members/{user_id}"),
                &SetRoleRequest { role },
            )
            .await?;
        Ok(())
    }

    /// `POST /v1/groups/{id}/leave`
    pub async fn leave_group(&self, group_id: &str) -> Result<()> {
        let _: Option<serde_json::Value> = self
            .post(
                &format!("v1/groups/{group_id}/leave"),
                &serde_json::json!({}),
                true,
            )
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// `GET /v1/groups/{id}/messages` — paginated by sequence number, with
    /// the server's `nextAfterSeq` / `nextBeforeSeq` cursors.
    pub async fn get_messages_page(
        &self,
        group_id: &str,
        query: &MessagesQuery,
    ) -> Result<MessagesPage> {
        let mut page: MessagesPage = self
            .get(&format!("v1/groups/{group_id}/messages"), Some(query))
            .await?;
        for m in &mut page.items {
            m.group_id.get_or_insert_with(|| group_id.to_owned());
        }
        Ok(page)
    }

    /// `GET /v1/groups/{id}/messages` — just the items.
    pub async fn get_messages(
        &self,
        group_id: &str,
        query: &MessagesQuery,
    ) -> Result<Vec<MessageDto>> {
        Ok(self.get_messages_page(group_id, query).await?.items)
    }

    /// `POST /v1/groups/{id}/messages`. In the mobile app this is only reached
    /// through the outbox dispatcher; use [`crate::outbox::OutboxDispatcher`]
    /// for retry/backoff semantics.
    pub async fn send_message(
        &self,
        group_id: &str,
        client_message_id: &str,
        body: &str,
    ) -> Result<MessageDto> {
        let req = SendMessageRequest {
            client_message_id: client_message_id.to_owned(),
            body: body.to_owned(),
        };
        // The live reply is not always a bare message object: accept `{message: {...}}`,
        // `{data: {...}}`, an `{id, seq}` stub, or a `{clientMessageId, seq}` ack.
        let raw: serde_json::Value = self
            .post(&format!("v1/groups/{group_id}/messages"), &req, true)
            .await?;
        let mut m = decode_sent_message(raw, group_id, client_message_id, body)?;
        m.group_id.get_or_insert_with(|| group_id.to_owned());
        m.client_message_id
            .get_or_insert_with(|| client_message_id.to_owned());
        Ok(m)
    }

    /// `POST /v1/groups/{id}/ack` — mark read up to `seq`.
    pub async fn ack(&self, group_id: &str, seq: i64) -> Result<()> {
        let _: Option<serde_json::Value> = self
            .post(
                &format!("v1/groups/{group_id}/ack"),
                &AckRequest { seq },
                true,
            )
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Pending catch-up
    // -----------------------------------------------------------------------

    /// `GET /v1/pending` — events missed while disconnected.
    pub async fn pending(&self, limit: Option<u32>) -> Result<PendingResponse> {
        #[derive(Serialize)]
        struct Q {
            #[serde(skip_serializing_if = "Option::is_none")]
            limit: Option<u32>,
        }
        self.get("v1/pending", Some(&Q { limit })).await
    }
}

/// Turn whatever `POST /messages` answered into a `MessageDto`.
fn decode_sent_message(
    raw: serde_json::Value,
    group_id: &str,
    client_message_id: &str,
    body: &str,
) -> Result<MessageDto> {
    let mut v = raw;
    for key in ["message", "data", "item", "result"] {
        if v.get(key).map(|x| x.is_object()).unwrap_or(false) {
            v = v[key].take();
        }
    }
    if let Ok(m) = serde_json::from_value::<MessageDto>(v.clone()) {
        return Ok(m);
    }
    // Minimal ack: need at least a seq or an id to be useful.
    let obj = v.as_object().cloned().unwrap_or_default();
    let seq = obj.get("seq").and_then(|x| x.as_i64());
    let id = obj
        .get("id")
        .or_else(|| obj.get("messageId"))
        .and_then(|x| x.as_str())
        .map(str::to_owned);
    match (id, seq) {
        (Some(id), Some(seq)) => Ok(MessageDto {
            id,
            group_id: Some(group_id.to_owned()),
            seq,
            sender_id: obj
                .get("senderId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            body: obj
                .get("body")
                .and_then(|x| x.as_str())
                .unwrap_or(body)
                .to_owned(),
            client_message_id: Some(client_message_id.to_owned()),
            created_at: obj
                .get("createdAt")
                .and_then(crate::models::epoch_ms_from_value),
            extra: obj,
        }),
        _ => Err(AppError::Json(format!(
            "send reply has no usable message (keys: {})",
            v.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_else(|| v.to_string())
        ))),
    }
}

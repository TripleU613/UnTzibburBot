//! Session persistence and lifecycle: `SessionStore`, `SessionState`,
//! `KeystoreSecretCipher` (AES-GCM wire format), `SessionScopeManager`
//! (logout / wipe), `AppPrefsStore` and `LegalStore`.

use crate::error::{AppError, Result};
use crate::http::{SessionInvalidationListener, TzibburClient};
use crate::models::{
    CategoriesEnvelope, Device, EnrollmentChallenge, LegalDocKey, LegalDocument, Session,
    StartAuthRequest, ThemeOverride, User, VerifyAuthRequest,
};
use crate::store::LocalStore;
use crate::sync::SyncEngine;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Cipher
// ---------------------------------------------------------------------------

/// Encrypts the bearer token at rest. Wire format (matches the Android
/// `KeystoreSecretCipher`): `[ivLength: 1 byte][iv][ciphertext+tag]`.
pub trait SecretCipher: Send + Sync {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn decrypt(&self, wire: &[u8]) -> Result<Vec<u8>>;
}

/// No encryption (token stored as-is). Use only when the file itself is protected.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainCipher;

impl SecretCipher for PlainCipher {
    fn encrypt(&self, p: &[u8]) -> Result<Vec<u8>> {
        Ok(p.to_vec())
    }
    fn decrypt(&self, w: &[u8]) -> Result<Vec<u8>> {
        Ok(w.to_vec())
    }
}

/// AES-256-GCM, 128-bit tag, 96-bit random nonce, Android-compatible framing.
pub struct AesGcmCipher {
    key: Key<Aes256Gcm>,
}

impl AesGcmCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key: key.into() }
    }
    /// Derive a key from any secret bytes (SHA-256 via the `aes-gcm` dependency tree is
    /// not available, so this uses a simple wide-pipe fold; prefer passing 32 random bytes).
    pub fn from_secret(secret: &[u8]) -> Self {
        let mut key = [0u8; 32];
        for (i, b) in secret.iter().enumerate() {
            key[i % 32] ^= b.rotate_left((i % 7) as u32);
            key[(i * 7 + 3) % 32] = key[(i * 7 + 3) % 32].wrapping_add(*b);
        }
        Self::new(key)
    }
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut key);
        Self::new(key)
    }
}

impl SecretCipher for AesGcmCipher {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(&self.key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| AppError::Store(format!("encrypt: {e}")))?;
        let mut out = Vec::with_capacity(1 + nonce.len() + ct.len());
        out.push(nonce.len() as u8);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }
    fn decrypt(&self, wire: &[u8]) -> Result<Vec<u8>> {
        let iv_len = *wire
            .first()
            .ok_or_else(|| AppError::Store("empty ciphertext".into()))?
            as usize;
        if wire.len() < 1 + iv_len {
            return Err(AppError::Store("truncated ciphertext".into()));
        }
        let nonce = Nonce::from_slice(&wire[1..1 + iv_len]);
        let cipher = Aes256Gcm::new(&self.key);
        cipher
            .decrypt(nonce, &wire[1 + iv_len..])
            .map_err(|e| AppError::Store(format!("decrypt: {e}")))
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

/// What `SessionStore` persists (`user` + `device` + encrypted token).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSession {
    pub user: User,
    pub device: Device,
    pub token: String,
}

impl From<Session> for StoredSession {
    fn from(s: Session) -> Self {
        StoredSession {
            user: s.user,
            device: s.device,
            token: s.token,
        }
    }
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self) -> Result<Option<StoredSession>>;
    async fn save(&self, session: &StoredSession) -> Result<()>;
    async fn update_display_name(&self, name: &str) -> Result<()>;
    async fn clear(&self) -> Result<()>;
}

/// In-memory store (tests).
#[derive(Default)]
pub struct MemorySessionStore(Mutex<Option<StoredSession>>);

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self) -> Result<Option<StoredSession>> {
        Ok(self.0.lock().clone())
    }
    async fn save(&self, s: &StoredSession) -> Result<()> {
        *self.0.lock() = Some(s.clone());
        Ok(())
    }
    async fn update_display_name(&self, name: &str) -> Result<()> {
        if let Some(s) = self.0.lock().as_mut() {
            s.user.display_name = name.to_owned();
        }
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        *self.0.lock() = None;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SessionFile {
    user: Option<User>,
    device: Option<Device>,
    /// Base64 of the cipher wire format (`TOKEN_CIPHERTEXT`).
    token_ciphertext: Option<String>,
}

/// JSON file store with the token encrypted by a [`SecretCipher`].
pub struct FileSessionStore {
    path: PathBuf,
    cipher: Arc<dyn SecretCipher>,
    lock: tokio::sync::Mutex<()>,
}

impl FileSessionStore {
    pub fn new(path: impl Into<PathBuf>, cipher: Arc<dyn SecretCipher>) -> Self {
        Self {
            path: path.into(),
            cipher,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn read(&self) -> Result<SessionFile> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) if !bytes.is_empty() => Ok(serde_json::from_slice(&bytes)?),
            Ok(_) => Ok(SessionFile::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SessionFile::default()),
            Err(e) => Err(AppError::Store(e.to_string())),
        }
    }

    async fn write(&self, f: &SessionFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AppError::Store(e.to_string()))?;
        }
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(f)?)
            .await
            .map_err(|e| AppError::Store(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| AppError::Store(e.to_string()))
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self) -> Result<Option<StoredSession>> {
        let _g = self.lock.lock().await;
        let f = self.read().await?;
        let (Some(user), Some(device), Some(ct)) = (f.user, f.device, f.token_ciphertext) else {
            return Ok(None);
        };
        let wire = base64::engine::general_purpose::STANDARD
            .decode(ct)
            .map_err(|e| AppError::Store(format!("token base64: {e}")))?;
        let token = String::from_utf8(self.cipher.decrypt(&wire)?)
            .map_err(|e| AppError::Store(format!("token utf8: {e}")))?;
        Ok(Some(StoredSession {
            user,
            device,
            token,
        }))
    }

    async fn save(&self, s: &StoredSession) -> Result<()> {
        let _g = self.lock.lock().await;
        let wire = self.cipher.encrypt(s.token.as_bytes())?;
        let f = SessionFile {
            user: Some(s.user.clone()),
            device: Some(s.device.clone()),
            token_ciphertext: Some(base64::engine::general_purpose::STANDARD.encode(wire)),
        };
        self.write(&f).await
    }

    async fn update_display_name(&self, name: &str) -> Result<()> {
        let _g = self.lock.lock().await;
        let mut f = self.read().await?;
        if let Some(u) = f.user.as_mut() {
            u.display_name = name.to_owned();
        }
        self.write(&f).await
    }

    async fn clear(&self) -> Result<()> {
        let _g = self.lock.lock().await;
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::Store(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionState + manager
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub enum SessionState {
    /// Store read in progress.
    #[default]
    Loading,
    /// No token.
    SignedOut,
    SignedIn {
        user: User,
        token: String,
    },
}

impl SessionState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, SessionState::SignedIn { .. })
    }
    pub fn user(&self) -> Option<&User> {
        match self {
            SessionState::SignedIn { user, .. } => Some(user),
            _ => None,
        }
    }
}

/// `SessionRepository` + `SessionScopeManager`: owns the session store, keeps
/// the client's token in sync, and wipes everything on logout / 401.
pub struct SessionManager {
    client: TzibburClient,
    store: Arc<dyn SessionStore>,
    local: Arc<dyn LocalStore>,
    sync: Mutex<Option<Arc<SyncEngine>>>,
    state_tx: watch::Sender<SessionState>,
    wipe_in_flight: AtomicBool,
}

impl SessionManager {
    pub fn new(
        client: TzibburClient,
        store: Arc<dyn SessionStore>,
        local: Arc<dyn LocalStore>,
    ) -> Arc<Self> {
        let (state_tx, _) = watch::channel(SessionState::Loading);
        Arc::new(Self {
            client,
            store,
            local,
            sync: Mutex::new(None),
            state_tx,
            wipe_in_flight: AtomicBool::new(false),
        })
    }

    /// Attach the sync engine so it is stopped on wipe and started on sign-in.
    pub fn attach_sync(&self, sync: Arc<SyncEngine>) {
        *self.sync.lock() = Some(sync);
    }

    pub fn state(&self) -> SessionState {
        self.state_tx.borrow().clone()
    }
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state_tx.subscribe()
    }
    pub fn client(&self) -> &TzibburClient {
        &self.client
    }

    /// Read the persisted session and install the token. Call once at startup.
    pub async fn load(&self) -> Result<SessionState> {
        let st = match self.store.load().await? {
            Some(s) => {
                self.client.set_token(Some(s.token.clone())).await;
                if let Some(sync) = self.sync.lock().clone() {
                    sync.set_self_user_id(Some(s.user.id.clone()));
                }
                SessionState::SignedIn {
                    user: s.user,
                    token: s.token,
                }
            }
            None => SessionState::SignedOut,
        };
        self.state_tx.send_replace(st.clone());
        Ok(st)
    }

    /// `POST /v1/auth/start`.
    pub async fn start_auth(
        &self,
        phone: &str,
        display_name: Option<&str>,
        region: Option<&str>,
    ) -> Result<EnrollmentChallenge> {
        self.client
            .start_auth(&StartAuthRequest {
                phone: phone.to_owned(),
                display_name: display_name.map(str::to_owned),
                region: region.map(str::to_owned),
            })
            .await
    }

    /// `POST /v1/auth/verify`, then persist and move to `SignedIn`.
    pub async fn verify_auth(&self, req: &VerifyAuthRequest) -> Result<Session> {
        let session = self.client.verify_auth(req).await?;
        self.store
            .save(&StoredSession::from(session.clone()))
            .await?;
        if let Some(sync) = self.sync.lock().clone() {
            sync.set_self_user_id(Some(session.user.id.clone()));
        }
        self.state_tx.send_replace(SessionState::SignedIn {
            user: session.user.clone(),
            token: session.token.clone(),
        });
        Ok(session)
    }

    /// `PATCH /v1/me` + local update.
    pub async fn update_display_name(&self, name: &str) -> Result<User> {
        let user = self.client.update_display_name(name).await?;
        self.store.update_display_name(&user.display_name).await?;
        let _ = self.state_tx.send_if_modified(|s| {
            if let SessionState::SignedIn { user: u, .. } = s {
                u.display_name = user.display_name.clone();
                true
            } else {
                false
            }
        });
        Ok(user)
    }

    /// Explicit sign-out: same as a wipe.
    pub async fn sign_out(&self) -> Result<()> {
        self.wipe_session().await
    }

    /// `SessionScopeManager.wipeSession()`: stop sync, clear local DB, clear session.
    /// Idempotent while a wipe is in flight.
    pub async fn wipe_session(&self) -> Result<()> {
        if self.wipe_in_flight.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = async {
            let sync = self.sync.lock().clone();
            if let Some(sync) = sync {
                sync.stop().await;
                sync.set_self_user_id(None);
            }
            self.local.clear_all()?;
            self.store.clear().await?;
            self.client.set_token(None).await;
            self.state_tx.send_replace(SessionState::SignedOut);
            Ok(())
        }
        .await;
        self.wipe_in_flight.store(false, Ordering::SeqCst);
        result
    }
}

/// Hook the manager into a client: `TzibburClient::builder().session_listener(manager.invalidation_listener())`.
impl SessionManager {
    pub fn invalidation_listener(self: &Arc<Self>) -> Arc<dyn SessionInvalidationListener> {
        Arc::new(WipeOn401(Arc::downgrade(self)))
    }
}

struct WipeOn401(std::sync::Weak<SessionManager>);

/// A [`SessionInvalidationListener`] that can be handed to
/// [`TzibburClient::builder`] *before* the [`SessionManager`] exists, then bound
/// to it. Breaks the client ↔ manager construction cycle.
#[derive(Default)]
pub struct LateBoundListener(std::sync::OnceLock<std::sync::Weak<SessionManager>>);

impl LateBoundListener {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn bind(&self, manager: &Arc<SessionManager>) {
        let _ = self.0.set(Arc::downgrade(manager));
    }
}

impl SessionInvalidationListener for LateBoundListener {
    fn on_session_invalidated(&self) {
        if let Some(m) = self.0.get().and_then(|w| w.upgrade()) {
            tokio::spawn(async move {
                if let Err(e) = m.wipe_session().await {
                    tracing::error!(error = %e, "session wipe failed");
                }
            });
        }
    }
}

impl SessionInvalidationListener for WipeOn401 {
    fn on_session_invalidated(&self) {
        if let Some(m) = self.0.upgrade() {
            tokio::spawn(async move {
                if let Err(e) = m.wipe_session().await {
                    tracing::error!(error = %e, "session wipe failed");
                }
            });
        }
    }
}

/// `SyncLifecycle`: start sync when signed in (and "foregrounded"), stop otherwise.
pub fn spawn_sync_lifecycle(
    manager: Arc<SessionManager>,
    sync: Arc<SyncEngine>,
    mut foreground: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let mut session = manager.watch_state();
    tokio::spawn(async move {
        loop {
            let should_run = *foreground.borrow() && session.borrow().is_signed_in();
            if should_run && !sync.is_running() {
                sync.start();
            } else if !should_run && sync.is_running() {
                sync.stop().await;
            }
            tokio::select! {
                r = foreground.changed() => if r.is_err() { break },
                r = session.changed() => if r.is_err() { break },
            }
        }
    })
}

// ---------------------------------------------------------------------------
// AppPrefsStore & LegalStore
// ---------------------------------------------------------------------------

/// Non-sensitive preferences (`CATEGORIES_JSON`, `THEME_OVERRIDE`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppPrefs {
    pub categories: Option<CategoriesEnvelope>,
    pub theme_override: ThemeOverride,
}

/// JSON-file backed [`AppPrefs`].
pub struct AppPrefsStore {
    path: PathBuf,
    cache: Mutex<AppPrefs>,
}

impl AppPrefsStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let cache = read_json_or_default(&path)?;
        Ok(Self {
            path,
            cache: Mutex::new(cache),
        })
    }
    pub fn get(&self) -> AppPrefs {
        self.cache.lock().clone()
    }
    pub fn set_categories(&self, env: CategoriesEnvelope) -> Result<()> {
        let snapshot = {
            let mut c = self.cache.lock();
            c.categories = Some(env);
            c.clone()
        };
        write_json(&self.path, &snapshot)
    }
    pub fn set_theme_override(&self, t: ThemeOverride) -> Result<()> {
        let snapshot = {
            let mut c = self.cache.lock();
            c.theme_override = t;
            c.clone()
        };
        write_json(&self.path, &snapshot)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LegalEntry {
    pub document: Option<LegalDocument>,
    /// Checksum the user last acknowledged.
    pub last_seen_checksum: Option<String>,
}

/// Cached legal documents plus acknowledgement tracking.
pub struct LegalStore {
    path: PathBuf,
    cache: Mutex<BTreeMap<String, LegalEntry>>,
}

impl LegalStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let cache = read_json_or_default(&path)?;
        Ok(Self {
            path,
            cache: Mutex::new(cache),
        })
    }
    pub fn get(&self, key: LegalDocKey) -> LegalEntry {
        self.cache
            .lock()
            .get(key.as_str())
            .cloned()
            .unwrap_or_default()
    }
    pub fn put_document(&self, key: LegalDocKey, doc: LegalDocument) -> Result<()> {
        let snap = {
            let mut c = self.cache.lock();
            c.entry(key.as_str().to_owned()).or_default().document = Some(doc);
            c.clone()
        };
        write_json(&self.path, &snap)
    }
    /// `LegalRepository.markSeen(key)`.
    pub fn mark_seen(&self, key: LegalDocKey) -> Result<()> {
        let snap = {
            let mut c = self.cache.lock();
            let e = c.entry(key.as_str().to_owned()).or_default();
            e.last_seen_checksum = e.document.as_ref().map(|d| d.checksum.clone());
            c.clone()
        };
        write_json(&self.path, &snap)
    }
    /// Whether the cached document differs from what the user acknowledged.
    pub fn needs_acceptance(&self, key: LegalDocKey) -> bool {
        let e = self.get(key);
        match (&e.document, &e.last_seen_checksum) {
            (Some(d), Some(seen)) => &d.checksum != seen,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
    /// Fetch from the server (`GET /v1/legal/{key}`) and cache.
    pub async fn refresh(&self, client: &TzibburClient, key: LegalDocKey) -> Result<LegalDocument> {
        let doc = client.legal(key).await?;
        self.put_document(key, doc.clone())?;
        Ok(doc)
    }
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) if !b.is_empty() => Ok(serde_json::from_slice(&b)?),
        Ok(_) => Ok(T::default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(AppError::Store(e.to_string())),
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| AppError::Store(e.to_string()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)
        .map_err(|e| AppError::Store(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| AppError::Store(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_gcm_roundtrip_and_framing() {
        let c = AesGcmCipher::random();
        let wire = c.encrypt(b"tok").unwrap();
        assert_eq!(wire[0], 12);
        assert_eq!(c.decrypt(&wire).unwrap(), b"tok");
        assert!(AesGcmCipher::random().decrypt(&wire).is_err());
    }

    #[tokio::test]
    async fn file_session_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(
            dir.path().join("session.json"),
            Arc::new(AesGcmCipher::random()),
        );
        assert!(store.load().await.unwrap().is_none());
        let s = StoredSession {
            user: User {
                id: "u".into(),
                display_name: "N".into(),
                phone_e164: Some("+1".into()),
                ..Default::default()
            },
            device: Device {
                id: "d".into(),
                ..Default::default()
            },
            token: "secret".into(),
        };
        store.save(&s).await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("session.json")).unwrap();
        assert!(!raw.contains("secret"));
        assert_eq!(store.load().await.unwrap().unwrap(), s);
        store.update_display_name("M").await.unwrap();
        assert_eq!(store.load().await.unwrap().unwrap().user.display_name, "M");
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
}

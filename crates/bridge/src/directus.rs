//! Minimal Directus REST client (items + schema bootstrap).
//!
//! Directus is the bridge's durable database: users, connected accounts,
//! conversation ↔ topic mappings and message-id mappings live in collections
//! created on startup by [`Directus::ensure_schema`]. Only ids, mappings and
//! the encrypted session are stored — never message bodies.

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Directus {
    http: reqwest::Client,
    base: url::Url,
    token: String,
}

impl std::fmt::Debug for Directus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Directus")
            .field("base", &self.base.as_str())
            .finish()
    }
}

/// One `filter[field][_op]=value` clause.
#[derive(Debug, Clone)]
pub struct Filter {
    pub field: String,
    pub op: &'static str,
    pub value: String,
}

impl Filter {
    pub fn eq(field: &str, value: impl ToString) -> Self {
        Filter {
            field: field.into(),
            op: "_eq",
            value: value.to_string(),
        }
    }
}

impl Directus {
    pub fn new(base: url::Url, token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("tzibbur-telegram-bridge")
            .build()?;
        let mut base = base;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self { http, base, token })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<T> {
        let url = self.base.join(path.trim_start_matches('/'))?;
        let mut req = self
            .http
            .request(method.clone(), url.clone())
            .bearer_auth(&self.token)
            .query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("directus {method} {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["errors"][0]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(DirectusError {
                status,
                message: msg,
                path: path.to_owned(),
            }
            .into());
        }
        if text.trim().is_empty() {
            return serde_json::from_value(Value::Null).context("directus empty body");
        }
        let v: Value = serde_json::from_str(&text).context("directus json")?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        serde_json::from_value(data).with_context(|| format!("directus decode {path}"))
    }

    // ---- items ----------------------------------------------------------------

    pub async fn list<T: DeserializeOwned>(
        &self,
        collection: &str,
        filters: &[Filter],
        limit: Option<u32>,
        sort: Option<&str>,
    ) -> Result<Vec<T>> {
        let mut q: Vec<(String, String)> = filters
            .iter()
            .map(|f| (format!("filter[{}][{}]", f.field, f.op), f.value.clone()))
            .collect();
        q.push((
            "limit".into(),
            limit.map(|l| l.to_string()).unwrap_or_else(|| "-1".into()),
        ));
        if let Some(s) = sort {
            q.push(("sort".into(), s.into()));
        }
        let v: Option<Vec<T>> = self
            .call(Method::GET, &format!("items/{collection}"), &q, None)
            .await?;
        Ok(v.unwrap_or_default())
    }

    pub async fn first<T: DeserializeOwned>(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<Option<T>> {
        Ok(self
            .list::<T>(collection, filters, Some(1), None)
            .await?
            .into_iter()
            .next())
    }

    pub async fn get<T: DeserializeOwned>(&self, collection: &str, id: i64) -> Result<Option<T>> {
        match self
            .call::<Option<T>>(Method::GET, &format!("items/{collection}/{id}"), &[], None)
            .await
        {
            Ok(v) => Ok(v),
            Err(e)
                if is_status(&e, StatusCode::FORBIDDEN) || is_status(&e, StatusCode::NOT_FOUND) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn create<T: DeserializeOwned>(
        &self,
        collection: &str,
        item: &impl Serialize,
    ) -> Result<T> {
        let body = serde_json::to_value(item)?;
        self.call(
            Method::POST,
            &format!("items/{collection}"),
            &[],
            Some(&body),
        )
        .await
    }

    pub async fn update<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: i64,
        patch: &impl Serialize,
    ) -> Result<T> {
        let body = serde_json::to_value(patch)?;
        self.call(
            Method::PATCH,
            &format!("items/{collection}/{id}"),
            &[],
            Some(&body),
        )
        .await
    }

    pub async fn delete(&self, collection: &str, id: i64) -> Result<()> {
        let _: Option<Value> = self
            .call(
                Method::DELETE,
                &format!("items/{collection}/{id}"),
                &[],
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn delete_where(&self, collection: &str, filters: &[Filter]) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct Row {
            id: i64,
        }
        let rows: Vec<Row> = self.list(collection, filters, None, None).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();
        let _: Option<Value> = self
            .call(
                Method::DELETE,
                &format!("items/{collection}"),
                &[],
                Some(&json!(ids)),
            )
            .await?;
        Ok(())
    }

    // ---- schema -----------------------------------------------------------------

    pub async fn ping(&self) -> Result<()> {
        let _: Option<Value> = self
            .call(Method::GET, "server/ping", &[], None)
            .await
            .or_else(|e| {
                // /server/ping returns "pong" as plain text which our decoder treats as empty/null.
                if is_status(&e, StatusCode::UNAUTHORIZED) {
                    Err(e)
                } else {
                    Ok(None)
                }
            })?;
        Ok(())
    }

    async fn collection_exists(&self, name: &str) -> Result<bool> {
        match self
            .call::<Option<Value>>(Method::GET, &format!("collections/{name}"), &[], None)
            .await
        {
            Ok(Some(v)) => Ok(!v.is_null()),
            Ok(None) => Ok(false),
            Err(e)
                if is_status(&e, StatusCode::NOT_FOUND) || is_status(&e, StatusCode::FORBIDDEN) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Create every bridge collection that does not exist yet. Idempotent.
    pub async fn ensure_schema(&self, prefix: &str) -> Result<()> {
        for spec in schema(prefix) {
            if self.collection_exists(&spec.name).await? {
                tracing::debug!(collection = %spec.name, "directus: collection exists");
                continue;
            }
            tracing::info!(collection = %spec.name, "directus: creating collection");
            let body = spec.to_create_payload();
            let _: Option<Value> = self
                .call(Method::POST, "collections", &[], Some(&body))
                .await
                .with_context(|| format!("create collection {}", spec.name))?;
            for rel in &spec.relations {
                let rel_body = json!({
                    "collection": spec.name,
                    "field": rel.field,
                    "related_collection": rel.related,
                    "schema": {"on_delete": "CASCADE"},
                    "meta": {"one_field": null}
                });
                if let Err(e) = self
                    .call::<Option<Value>>(Method::POST, "relations", &[], Some(&rel_body))
                    .await
                {
                    tracing::warn!(error = %e, collection = %spec.name, field = rel.field, "directus: relation not created (non-fatal)");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("directus {status} on {path}: {message}")]
pub struct DirectusError {
    pub status: StatusCode,
    pub message: String,
    pub path: String,
}

pub fn is_status(e: &anyhow::Error, s: StatusCode) -> bool {
    e.downcast_ref::<DirectusError>()
        .map(|d| d.status == s)
        .unwrap_or(false)
}

// ---- schema definition ------------------------------------------------------------

struct FieldSpec {
    name: &'static str,
    ty: &'static str,
    interface: &'static str,
    /// Extra `schema` properties.
    schema: Value,
    /// Extra `meta` properties.
    meta: Value,
}

struct RelationSpec {
    field: &'static str,
    related: String,
}

struct CollectionSpec {
    name: String,
    note: &'static str,
    fields: Vec<FieldSpec>,
    relations: Vec<RelationSpec>,
}

impl CollectionSpec {
    fn to_create_payload(&self) -> Value {
        let mut fields = vec![json!({
            "field": "id", "type": "integer", "meta": {"hidden": true, "interface": "input", "readonly": true},
            "schema": {"is_primary_key": true, "has_auto_increment": true}
        })];
        for f in &self.fields {
            let mut meta = json!({"interface": f.interface});
            merge(&mut meta, &f.meta);
            let mut schema = json!({});
            merge(&mut schema, &f.schema);
            fields.push(json!({"field": f.name, "type": f.ty, "meta": meta, "schema": schema}));
        }
        json!({
            "collection": self.name,
            "meta": {"note": self.note, "hidden": false, "singleton": false, "icon": "hub"},
            "schema": {},
            "fields": fields
        })
    }
}

fn merge(into: &mut Value, from: &Value) {
    if let (Some(a), Some(b)) = (into.as_object_mut(), from.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
}

fn s(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "string",
        interface: "input",
        schema: json!({}),
        meta: json!({}),
    }
}
fn text(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "text",
        interface: "input-multiline",
        schema: json!({}),
        meta: json!({}),
    }
}
fn bigint(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "bigInteger",
        interface: "input",
        schema: json!({}),
        meta: json!({}),
    }
}
fn boolean(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "boolean",
        interface: "boolean",
        schema: json!({"default_value": false}),
        meta: json!({}),
    }
}
fn ts(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "timestamp",
        interface: "datetime",
        schema: json!({}),
        meta: json!({}),
    }
}
fn jsonf(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "json",
        interface: "input-code",
        schema: json!({}),
        meta: json!({"options": {"language": "json"}}),
    }
}
fn fk(name: &'static str) -> FieldSpec {
    FieldSpec {
        name,
        ty: "integer",
        interface: "select-dropdown-m2o",
        schema: json!({}),
        meta: json!({"special": ["m2o"]}),
    }
}
fn unique(mut f: FieldSpec) -> FieldSpec {
    merge(&mut f.schema, &json!({"is_unique": true}));
    f
}

pub fn collection_names(prefix: &str) -> Names {
    Names {
        users: format!("{prefix}users"),
        accounts: format!("{prefix}accounts"),
        conversations: format!("{prefix}conversations"),
        messages: format!("{prefix}messages"),
    }
}

#[derive(Clone, Debug)]
pub struct Names {
    pub users: String,
    pub accounts: String,
    pub conversations: String,
    pub messages: String,
}

fn schema(prefix: &str) -> Vec<CollectionSpec> {
    let n = collection_names(prefix);
    vec![
        CollectionSpec {
            name: n.users.clone(),
            note: "Telegram users who have talked to the bridge bot",
            fields: vec![
                unique(s("telegram_user_id")),
                s("telegram_username"),
                s("telegram_first_name"),
                jsonf("settings"),
                ts("created_at"),
                ts("updated_at"),
            ],
            relations: vec![],
        },
        CollectionSpec {
            name: n.accounts.clone(),
            note: "Connected Tzibbur accounts (session token encrypted with BRIDGE_MASTER_KEY)",
            fields: vec![
                fk("user"),
                s("tzibbur_user_id"),
                s("phone_e164"),
                s("display_name"),
                s("device_id"),
                text("encrypted_session"),
                s("status"),
                jsonf("settings"),
                ts("connected_at"),
                ts("updated_at"),
            ],
            relations: vec![RelationSpec {
                field: "user",
                related: n.users.clone(),
            }],
        },
        CollectionSpec {
            name: n.conversations.clone(),
            note: "Tzibbur group Telegram topic mapping (one per account × group)",
            fields: vec![
                fk("account"),
                s("group_id"),
                s("telegram_chat_id"),
                s("telegram_topic_id"),
                s("name"),
                s("kind"),
                bigint("last_forwarded_seq"),
                boolean("closed"),
                ts("created_at"),
                ts("updated_at"),
            ],
            relations: vec![RelationSpec {
                field: "account",
                related: n.accounts.clone(),
            }],
        },
        CollectionSpec {
            name: n.messages.clone(),
            note: "Message-id mapping for replies and idempotency (no bodies stored)",
            fields: vec![
                fk("conversation"),
                s("tzibbur_message_id"),
                s("client_message_id"),
                bigint("seq"),
                s("telegram_message_id"),
                s("direction"),
                ts("created_at"),
            ],
            relations: vec![RelationSpec {
                field: "conversation",
                related: n.conversations.clone(),
            }],
        },
    ]
}

//! HTTP server: health endpoint, the Telegram Mini App login page, and its API.
//!
//! The Mini App is the "secure login page" from the architecture: the user
//! enters phone + SMS code in a Telegram-hosted web view instead of the chat
//! stream. Requests carry Telegram `initData`, which is verified with
//! HMAC-SHA256 against the bot token before anything happens.

use crate::app::App;
use crate::phone::parse_phone;
use crate::telegram::handlers::finish_connect;
use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use teloxide::types::{ChatId, User as TgUser};
use tzibbur_api::models::{StartAuthRequest, VerifyAuthRequest};
use tzibbur_api::prelude::*;
use tzibbur_api::validation::is_valid_otp;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/app", get(page))
        .route("/app/api/start", post(api_start))
        .route("/app/api/verify", post(api_verify))
        .with_state(app)
}

async fn health(State(app): State<Arc<App>>) -> impl IntoResponse {
    Json(serde_json::json!({"ok": true, "accounts_running": app.registry.len()}))
}

/// Verify Telegram WebApp `initData` and return the embedded user.
pub fn verify_init_data(init_data: &str, bot_token: &str, max_age_secs: i64) -> Result<TgUser> {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(init_data.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let hash = pairs
        .iter()
        .find(|(k, _)| k == "hash")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow!("initData without hash"))?;
    pairs.retain(|(k, _)| k != "hash");
    pairs.sort();
    let check: String = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut secret = Hmac::<Sha256>::new_from_slice(b"WebAppData")?;
    secret.update(bot_token.as_bytes());
    let secret = secret.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret)?;
    mac.update(check.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    if expected != hash.to_ascii_lowercase() {
        return Err(anyhow!("initData signature mismatch"));
    }
    let auth_date: i64 = pairs
        .iter()
        .find(|(k, _)| k == "auth_date")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if chrono::Utc::now().timestamp() - auth_date > max_age_secs {
        return Err(anyhow!("initData expired"));
    }
    let user_json = pairs
        .iter()
        .find(|(k, _)| k == "user")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow!("initData without user"))?;
    Ok(serde_json::from_str(&user_json)?)
}

#[derive(Deserialize)]
struct StartReq {
    init_data: String,
    phone: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Serialize)]
struct StartResp {
    challenge_id: String,
    phone: String,
    resend_after_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct VerifyReq {
    init_data: String,
    challenge_id: String,
    phone: String,
    code: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Serialize)]
struct ErrResp {
    error: String,
}

fn err(status: StatusCode, msg: impl ToString) -> (StatusCode, Json<ErrResp>) {
    (
        status,
        Json(ErrResp {
            error: msg.to_string(),
        }),
    )
}

async fn api_start(
    State(app): State<Arc<App>>,
    Json(req): Json<StartReq>,
) -> Result<Json<StartResp>, (StatusCode, Json<ErrResp>)> {
    let _user = verify_init_data(&req.init_data, &app.shared.cfg.telegram_token, 3600)
        .map_err(|e| err(StatusCode::UNAUTHORIZED, e))?;
    let phone = parse_phone(&req.phone, &app.shared.cfg.default_region)
        .map_err(|why| err(StatusCode::BAD_REQUEST, format!("phone: {why}")))?;
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .device(app.shared.cfg.device.clone())
        .build()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let display_name = req
        .display_name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let ch = client
        .start_auth(&StartAuthRequest {
            phone: phone.clone(),
            display_name,
            region: None,
            ..Default::default()
        })
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(StartResp {
        challenge_id: ch.challenge_id,
        phone,
        resend_after_seconds: ch.resend_after_seconds,
    }))
}

async fn api_verify(
    State(app): State<Arc<App>>,
    Json(req): Json<VerifyReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrResp>)> {
    let user = verify_init_data(&req.init_data, &app.shared.cfg.telegram_token, 3600)
        .map_err(|e| err(StatusCode::UNAUTHORIZED, e))?;
    let code: String = req.code.chars().filter(|c| c.is_ascii_digit()).collect();
    if !is_valid_otp(&code) {
        return Err(err(StatusCode::BAD_REQUEST, "code must be 6 digits"));
    }
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .device(app.shared.cfg.device.clone())
        .build()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let display_name = req
        .display_name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let session = client
        .verify_auth(&VerifyAuthRequest {
            challenge_id: req.challenge_id,
            code,
            phone: parse_phone(&req.phone, &app.shared.cfg.default_region).unwrap_or(req.phone),
            display_name,
            region: None,
            ..Default::default()
        })
        .await
        .map_err(|e| match e {
            AppError::InvalidCode { .. } => err(StatusCode::UNAUTHORIZED, "wrong code"),
            other => err(StatusCode::BAD_GATEWAY, other),
        })?;
    let name = session.user.display_name.clone();
    finish_connect(
        &app.shared.bot,
        ChatId(user.id.0 as i64),
        &user,
        &app,
        session,
        false,
    )
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({"ok": true, "display_name": name})))
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = r##"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Connect Tzibbur</title><script src="https://telegram.org/js/telegram-web-app.js"></script>
<style>
:root{color-scheme:light dark}
body{font-family:-apple-system,system-ui,Segoe UI,Roboto,sans-serif;margin:0;padding:24px;background:var(--tg-theme-bg-color,#fff);color:var(--tg-theme-text-color,#111)}
h1{font-size:1.3rem;margin:0 0 6px}p{color:var(--tg-theme-hint-color,#666);margin:0 0 18px;font-size:.95rem}
label{display:block;font-size:.85rem;margin:14px 0 6px;color:var(--tg-theme-hint-color,#666)}
input{width:100%;box-sizing:border-box;font-size:1.1rem;padding:12px;border-radius:10px;border:1px solid var(--tg-theme-hint-color,#ccc);background:var(--tg-theme-secondary-bg-color,#f4f4f4);color:inherit}
button{width:100%;margin-top:20px;padding:14px;font-size:1rem;border:0;border-radius:10px;background:var(--tg-theme-button-color,#2ea6ff);color:var(--tg-theme-button-text-color,#fff)}
button:disabled{opacity:.5}.err{color:#d33;margin-top:12px;min-height:1.2em}.ok{color:#2a2}#step2{display:none}
</style></head><body>
<h1>Connect your Tzibbur account</h1>
<p>Tzibbur will text you a 6-digit code. The code is used once and never stored.</p>
<div id="step1">
<label>Phone number</label><input id="phone" type="tel" placeholder="+1 415 555 0123" autocomplete="tel">
<label>Display name (leave empty if you already have an account)</label><input id="name" type="text" maxlength="64" placeholder="Your name">
<button id="send">Send code</button></div>
<div id="step2"><label>6-digit code</label><input id="code" inputmode="numeric" pattern="[0-9]*" maxlength="6" autocomplete="one-time-code">
<button id="verify">Connect</button></div>
<div class="err" id="err"></div>
<script>
const tg=window.Telegram&&window.Telegram.WebApp;if(tg){tg.ready();tg.expand();}
const $=id=>document.getElementById(id);let challenge=null,phone=null,name=null;
async function post(path,body){const r=await fetch(path,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});const j=await r.json().catch(()=>({}));if(!r.ok)throw new Error(j.error||('HTTP '+r.status));return j;}
$('send').onclick=async()=>{$('err').textContent='';$('send').disabled=true;try{
 phone=$('phone').value;name=$('name').value||null;
 const base=location.pathname.replace(/\/app\/?$/,'');const j=await post(base+'/app/api/start',{init_data:tg?tg.initData:'',phone,display_name:name});challenge=j.challenge_id;phone=j.phone;
 $('step1').style.display='none';$('step2').style.display='block';$('code').focus();
}catch(e){$('err').textContent=e.message}finally{$('send').disabled=false}};
$('verify').onclick=async()=>{$('err').textContent='';$('verify').disabled=true;try{
 const base=location.pathname.replace(/\/app\/?$/,'');const j=await post(base+'/app/api/verify',{init_data:tg?tg.initData:'',challenge_id:challenge,phone,code:$('code').value,display_name:name});
 $('err').className='ok';$('err').textContent='Connected as '+j.display_name+'. You can close this window.';if(tg)setTimeout(()=>tg.close(),1500);
}catch(e){$('err').textContent=e.message}finally{$('verify').disabled=false}};
</script></body></html>"##;

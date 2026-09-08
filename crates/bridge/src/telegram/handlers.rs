use super::{Command, Dialog, State, Storage};
use crate::app::App;
use crate::bridge::format::escape_html;
use crate::bridge::{BridgeBot, MAX_OUTBOUND_CHARS};
use crate::phone::{parse_phone, parse_phone_list};
use crate::store::{AccountSettings, AccountStatus, Conversation};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use teloxide::dispatching::dialogue::GetChatId;
use teloxide::prelude::*;
use teloxide::types::{
    ButtonRequest, CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup,
    KeyboardButton, KeyboardMarkup, KeyboardRemove, LabeledPrice, MessageId, ParseMode,
    PreCheckoutQuery, ReplyParameters, ThreadId, User as TgUser,
};
use teloxide::utils::command::BotCommands;
use tzibbur_api::models::{CreateGroupRequest, LegalDocKey, VerifyAuthRequest};
use tzibbur_api::prelude::*;
use tzibbur_api::validation::{
    is_valid_otp, looks_like_e164, normalize_phone, validate_display_name, validate_group_name,
};

const HTML: ParseMode = ParseMode::Html;

fn from(msg: &Message) -> Result<&TgUser> {
    msg.from
        .as_ref()
        .ok_or_else(|| anyhow!("message without sender"))
}

async fn say(bot: &BridgeBot, msg: &Message, text: impl Into<String>) -> Result<()> {
    let mut r = bot.send_message(msg.chat.id, text).parse_mode(HTML);
    if let Some(t) = msg.thread_id {
        r = r.message_thread_id(t);
    }
    r.await?;
    Ok(())
}

async fn say_kb(
    bot: &BridgeBot,
    chat: ChatId,
    text: impl Into<String>,
    kb: InlineKeyboardMarkup,
) -> Result<()> {
    bot.send_message(chat, text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .await?;
    Ok(())
}

fn kb(rows: Vec<Vec<(&str, String)>>) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(
        rows.into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|(t, d)| InlineKeyboardButton::callback(t, d))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
    )
}

fn friendly(e: &anyhow::Error) -> String {
    if let Some(app) = e.downcast_ref::<AppError>() {
        return match app {
            AppError::RateLimited {
                retry_after_seconds,
                ..
            } => {
                format!(
                    "Too many attempts. Try again in {} seconds.",
                    retry_after_seconds.unwrap_or(60)
                )
            }
            AppError::InvalidCode { .. } => "That code is not right.".into(),
            AppError::SmsDeliveryFailed { .. } => {
                "Tzibbur could not deliver the SMS to that number.".into()
            }
            AppError::InvalidDisplayName { max_length, .. } => {
                format!("Display name is invalid (max {max_length} characters).")
            }
            AppError::ReservedDisplayName { .. } => "That display name is reserved.".into(),
            AppError::ValidationFailed { errors, .. } => {
                format!(
                    "Tzibbur rejected the request: {}",
                    errors.as_ref().map(|v| v.to_string()).unwrap_or_default()
                )
            }
            AppError::Network { .. } => {
                "Tzibbur is unreachable right now. Try again shortly.".into()
            }
            other => other.to_string(),
        };
    }
    e.to_string()
}

fn ae(e: AppError) -> anyhow::Error {
    anyhow::Error::new(e)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn on_command(
    bot: BridgeBot,
    msg: Message,
    cmd: Command,
    app: Arc<App>,
    dialogue: Dialog,
) -> Result<()> {
    let tg = from(&msg)?.clone();
    if !msg.chat.is_private() {
        say(
            &bot,
            &msg,
            "I only work in a private chat. Open @{} directly.".replace("{}", &app.bot_username),
        )
        .await?;
        return Ok(());
    }
    match cmd {
        Command::Start => {
            app.bridge_user(&tg).await?;
            let account = app.account_for(tg.id.0 as i64).await?;
            let text = match account {
                Some(a) if a.status == AccountStatus::Connected => format!(
                    "👋 Welcome back, <b>{}</b>. Your Tzibbur groups are mirrored as topics in this chat. Reply inside a topic to post to the group.\n\n/chats · /newgroup · /settings · /help",
                    escape_html(a.display_name.as_deref().unwrap_or("friend"))
                ),
                Some(a) if a.status == AccountStatus::ReauthRequired => {
                    "⚠️ Your Tzibbur session expired. Use /reconnect to sign in again; your topics will be reused.".to_owned()
                }
                _ => format!(
                    "👋 <b>Tzibbur ↔ Telegram</b>\n\nI turn each of your Tzibbur groups into a topic in this chat, so you can read and reply from Telegram.\n\n• Tzibbur stays the source of truth; I keep only ids and an encrypted session.\n• Messages are plain text (that's all Tzibbur supports).\n\nTap <b>/connect</b> to sign in with your phone number{}.",
                    if app.shared.cfg.public_url.is_some() { ", or use the ≡ menu button for the secure login page" } else { "" }
                ),
            };
            let mut r = bot.send_message(msg.chat.id, text).parse_mode(HTML);
            if app.account_for(tg.id.0 as i64).await?.is_none() {
                r = r.reply_markup(kb(vec![vec![("🔗 Connect Tzibbur", "connect".into())]]));
            }
            r.await?;
        }
        Command::Help => say(&bot, &msg, escape_html(&Command::descriptions().to_string())).await?,
        Command::Cancel => {
            dialogue.exit().await?;
            say(&bot, &msg, "Cancelled.").await?;
        }
        Command::Connect | Command::Reconnect => begin_connect(&bot, &msg, &app, &dialogue).await?,
        Command::Disconnect => {
            match app.account_for(tg.id.0 as i64).await? {
                Some(a) if a.status != AccountStatus::Disconnected => {
                    say_kb(
                        &bot,
                        msg.chat.id,
                        "Disconnect your Tzibbur account from Telegram?\n\n<b>Disconnect</b> stops forwarding and deletes the stored session but keeps the topic mappings for an easy reconnect.\n<b>Disconnect &amp; erase</b> also removes all mappings.",
                        kb(vec![
                            vec![("🔌 Disconnect", "disc:keep".into()), ("🗑 Disconnect & erase", "disc:purge".into())],
                            vec![("Cancel", "noop".into())],
                        ]),
                    )
                    .await?
                }
                _ => say(&bot, &msg, "No Tzibbur account is connected.").await?,
            }
        }
        Command::Status => {
            let text = match app.account_for(tg.id.0 as i64).await? {
                None => "Not connected. Use /connect.".to_owned(),
                Some(a) => {
                    let rt = app.registry.get(a.id);
                    let state = rt.as_ref().map(|r| format!("{:?}", r.sync_state())).unwrap_or_else(|| "stopped".into());
                    let groups = rt.as_ref().and_then(|r| r.local().groups().ok()).map(|g| g.len()).unwrap_or(0);
                    format!(
                        "Account: <b>{}</b> ({})\nStatus: {}\nSync: {}\nGroups cached: {}\nTopics: {}",
                        escape_html(a.display_name.as_deref().unwrap_or("?")),
                        escape_html(a.phone_e164.as_deref().unwrap_or("")),
                        a.status.as_str(),
                        state,
                        groups,
                        if app.shared.bot_topics_enabled.load(std::sync::atomic::Ordering::Relaxed) { "enabled" } else { "disabled" }
                    )
                }
            };
            say(&bot, &msg, text).await?;
        }
        Command::Sync => {
            let rt = connected_runtime(&app, &tg).await?;
            match rt.sync_refresh().await {
                Ok(()) => say(&bot, &msg, "🔄 Synced.").await?,
                Err(e) => say(&bot, &msg, format!("Sync failed: {}", escape_html(&friendly(&e)))).await?,
            }
        }
        Command::Chats => {
            let rt = connected_runtime(&app, &tg).await?;
            let rows = rt.local().groups_with_unread(&rt.tzibbur_user_id)?;
            if rows.is_empty() {
                say(&bot, &msg, "No groups yet. Use /newgroup to create one.").await?;
            } else {
                let mut out = String::from("<b>Your Tzibbur groups</b>\n");
                for r in rows {
                    let unread = if r.unread_count > 0 { format!(" · {} unread", r.unread_count) } else { String::new() };
                    let muted = if r.group.muted { " 🔕" } else { "" };
                    out.push_str(&format!(
                        "• {}{} — {} member(s){}\n",
                        escape_html(&r.group.name),
                        muted,
                        r.group.member_count,
                        unread
                    ));
                }
                say(&bot, &msg, out).await?;
            }
        }
        Command::NewGroup => {
            connected_runtime(&app, &tg).await?;
            dialogue.update(State::AwaitGroupName).await?;
            say(&bot, &msg, "What should the group be called? (max 100 characters, /cancel to abort)").await?;
        }
        Command::Add(arg) => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let (phones, bad) = parse_phones(&app, &arg);
            if phones.is_empty() {
                say(&bot, &msg, "Usage inside a group topic: <code>/add 212-736-5000, +972 50 123 4567</code>").await?;
                return Ok(());
            }
            if !bad.is_empty() {
                say(&bot, &msg, format!("Skipping (not valid numbers): {}", escape_html(&bad.join(", ")))).await?;
            }
            match rt.client().add_members(&conv.group_id, &phones, None).await {
                Ok(out) => {
                    say(&bot, &msg, format_add_outcome(&out)).await?;
                    rt.sync_refresh_members(&conv.group_id).await.ok();
                }
                Err(e) => say(&bot, &msg, format!("Could not add members: {}", escape_html(&friendly(&ae(e))))).await?,
            }
        }
        Command::Members => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            rt.sync_refresh_members(&conv.group_id).await.ok();
            let members = rt.local().members(&conv.group_id)?;
            let mut out = format!("<b>{}</b> — {} member(s)\n", escape_html(conv.name.as_deref().unwrap_or("Group")), members.len());
            for m in members {
                let role = if m.role == tzibbur_api::models::Role::Admin { " (admin)" } else { "" };
                let you = if m.user_id == rt.tzibbur_user_id { " — you" } else { "" };
                out.push_str(&format!("• {}{}{}\n", escape_html(&m.display_name), role, you));
            }
            say(&bot, &msg, out).await?;
        }
        Command::Leave => {
            let (_rt, conv) = topic_context(&app, &tg, &msg).await?;
            say_kb(
                &bot,
                msg.chat.id,
                format!("Leave <b>{}</b>?", escape_html(conv.name.as_deref().unwrap_or("this group"))),
                kb(vec![vec![("🚪 Leave", format!("leave:{}", conv.id)), ("Cancel", "noop".into())]]),
            )
            .await?;
        }
        Command::Mute => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let g = rt.local().get_group(&conv.group_id)?.ok_or_else(|| anyhow!("group not cached"))?;
            rt.local().set_muted(&g.id, !g.muted)?;
            say(&bot, &msg, if g.muted { "🔔 Unmuted (bridge-local flag)." } else { "🔕 Muted (bridge-local flag)." }).await?;
        }
        Command::Name(name) => {
            let rt = connected_runtime(&app, &tg).await?;
            match validate_display_name(&name) {
                TextValidation::Valid { text } => match rt.client().update_display_name(&text).await {
                    Ok(u) => {
                        let acc = app.account_for(tg.id.0 as i64).await?.ok_or_else(|| anyhow!("no account"))?;
                        app.shared.store.update_account_display_name(acc.id, &u.display_name).await?;
                        say(&bot, &msg, format!("Display name is now <b>{}</b>.", escape_html(&u.display_name))).await?;
                    }
                    Err(e) => say(&bot, &msg, friendly(&ae(e))).await?,
                },
                TextValidation::Empty => say(&bot, &msg, "Usage: <code>/name Your Name</code>").await?,
                TextValidation::TooLong { count, max } => say(&bot, &msg, format!("Too long: {count} characters, max {max}.")).await?,
            }
        }
        Command::Settings => {
            let acc = app.account_for(tg.id.0 as i64).await?.ok_or_else(|| anyhow!("Connect first with /connect."))?;
            let s = acc.settings();
            say_kb(&bot, msg.chat.id, "<b>Settings</b>", settings_kb(&s)).await?;
        }
        Command::Donate => {
            let prices = vec![LabeledPrice { label: "Support the bridge".into(), amount: 50 }];
            bot.send_invoice(
                msg.chat.id,
                "Support the Tzibbur bridge",
                "Keeps the servers running. Thank you! ⭐",
                "donation:50",
                "XTR",
                prices,
            )
            .await?;
        }
        Command::Legal => {
            let client = TzibburClient::builder().base_url(app.shared.cfg.tzibbur_base_url.clone()).build()?;
            for key in [LegalDocKey::Terms, LegalDocKey::Privacy] {
                match client.legal(key).await {
                    Ok(doc) => {
                        let text: String = doc.markdown.chars().take(3800).collect();
                        say(&bot, &msg, format!("<b>{}</b>\n<pre>{}</pre>", key.as_str(), escape_html(&text))).await?;
                    }
                    Err(e) => say(&bot, &msg, format!("Could not fetch {}: {}", key.as_str(), escape_html(&e.to_string()))).await?,
                }
            }
        }
    }
    Ok(())
}

fn settings_kb(s: &AccountSettings) -> InlineKeyboardMarkup {
    let on = |b: bool| if b { "✅" } else { "⬜" };
    kb(vec![
        vec![(
            &format!("{} Auto-create topics for new groups", on(s.auto_topics)),
            "set:auto_topics".into(),
        )],
        vec![(
            &format!(
                "{} Mark read on Tzibbur after forwarding",
                on(s.auto_mark_read)
            ),
            "set:auto_mark_read".into(),
        )],
        vec![(
            &format!(
                "{} Show phone numbers for unnamed senders",
                on(s.show_phone_numbers)
            ),
            "set:show_phone_numbers".into(),
        )],
        vec![("Done", "noop".into())],
    ])
}

fn parse_phones(app: &App, arg: &str) -> (Vec<String>, Vec<String>) {
    parse_phone_list(arg, &app.shared.cfg.default_region)
}

fn format_add_outcome(out: &tzibbur_api::models::AddMembersOutcome) -> String {
    use tzibbur_api::models::AddedMember;
    let added: Vec<String> = out
        .added
        .iter()
        .map(|a| match a {
            AddedMember::Member(m) => escape_html(&m.display_name),
            AddedMember::Phone(p) => escape_html(p),
        })
        .collect();
    let mut s = String::new();
    if !added.is_empty() {
        s.push_str(&format!("✅ Added: {}\n", added.join(", ")));
    }
    if !out.already_member.is_empty() {
        s.push_str(&format!(
            "ℹ️ Already members: {}\n",
            escape_html(&out.already_member.join(", "))
        ));
    }
    if !out.not_found.is_empty() {
        s.push_str(&format!(
            "❌ Not on Tzibbur: {}\n",
            escape_html(&out.not_found.join(", "))
        ));
    }
    if s.is_empty() {
        s = "Nothing changed.".into();
    }
    s
}

async fn connected_runtime(app: &App, tg: &TgUser) -> Result<Arc<crate::bridge::AccountRuntime>> {
    match app.account_for(tg.id.0 as i64).await? {
        Some(a) if a.status == AccountStatus::Connected => app.runtime(a.id),
        Some(a) if a.status == AccountStatus::ReauthRequired => {
            Err(anyhow!("Your session expired. Use /reconnect."))
        }
        _ => Err(anyhow!("Connect first with /connect.")),
    }
}

/// Resolve the group behind the topic (or replied-to message) a command was sent in.
async fn topic_context(
    app: &App,
    tg: &TgUser,
    msg: &Message,
) -> Result<(Arc<crate::bridge::AccountRuntime>, Conversation)> {
    let rt = connected_runtime(app, tg).await?;
    let conv = resolve_conversation(app, msg)
        .await?
        .ok_or_else(|| anyhow!("Run this command inside a group topic."))?;
    if conv.account != rt.account_id {
        return Err(anyhow!("This topic belongs to another account."));
    }
    Ok((rt, conv))
}

async fn resolve_conversation(app: &App, msg: &Message) -> Result<Option<Conversation>> {
    if let Some(t) = msg.thread_id {
        if let Some(c) = app
            .shared
            .store
            .conversation_by_topic(msg.chat.id.0, t.0 .0)
            .await?
        {
            return Ok(Some(c));
        }
    }
    if let Some(reply) = msg.reply_to_message() {
        return app
            .shared
            .store
            .conversation_by_telegram_message(msg.chat.id.0, reply.id.0)
            .await;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// /connect dialogue
// ---------------------------------------------------------------------------

async fn begin_connect(bot: &BridgeBot, msg: &Message, app: &App, dialogue: &Dialog) -> Result<()> {
    let tg = from(msg)?;
    app.bridge_user(tg).await?;
    if let Some(a) = app.account_for(tg.id.0 as i64).await? {
        if a.status == AccountStatus::Connected && app.registry.get(a.id).is_some() {
            say(
                bot,
                msg,
                "You're already connected. Use /disconnect first if you want to switch accounts.",
            )
            .await?;
            return Ok(());
        }
    }
    dialogue.update(State::AwaitPhone).await?;
    send_phone_prompt(bot, msg.chat.id, app).await
}

/// Ask for the phone with a one-tap "share my number" button.
pub async fn send_phone_prompt(bot: &BridgeBot, chat: ChatId, app: &App) -> Result<()> {
    let mut text = format!(
        "📱 Tap the button to use your Telegram number, or type it any way you like — <code>212-736-5000</code>, <code>+972 50 123 4567</code>… (numbers without a country code are treated as {}).\n\nTzibbur will text you a 6-digit code. I never store the code, only the resulting session (encrypted).",
        app.shared.cfg.default_region
    );
    if app.shared.cfg.public_url.is_some() {
        text.push_str(
            "\n\nPrefer a form? Use the ≡ menu button to sign in on a secure page instead.",
        );
    }
    let kb = KeyboardMarkup::new(vec![vec![
        KeyboardButton::new("📱 Use my Telegram number").request(ButtonRequest::Contact)
    ]])
    .resize_keyboard()
    .one_time_keyboard();
    bot.send_message(chat, text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .await?;
    Ok(())
}

pub async fn on_phone(bot: BridgeBot, msg: Message, dialogue: Dialog, app: Arc<App>) -> Result<()> {
    let tg = from(&msg)?;
    // Shared contact (the button), or typed text.
    let raw = match msg.contact() {
        Some(c) => {
            if c.user_id.map(|u| u != tg.id).unwrap_or(false) {
                say(
                    &bot,
                    &msg,
                    "That's someone else's contact — share <b>your own</b> number, or type it.",
                )
                .await?;
                return Ok(());
            }
            c.phone_number.clone()
        }
        None => msg.text().unwrap_or_default().to_owned(),
    };
    let phone = match parse_phone(&raw, &app.shared.cfg.default_region) {
        Ok(p) => p,
        Err(why) => {
            say(
                &bot,
                &msg,
                format!(
                    "Hmm, I couldn't read that as a phone number ({}). Try <code>212-736-5000</code> or <code>+1 555 010 0123</code>, tap the button, or /cancel.",
                    escape_html(&why)
                ),
            )
            .await?;
            return Ok(());
        }
    };
    dialogue
        .update(State::AwaitName {
            phone: phone.clone(),
        })
        .await?;
    bot.send_message(
        msg.chat.id,
        format!(
            "Using <b>{}</b>.\n\nWhat display name should other Tzibbur members see? Reply <b>skip</b> if you already have a Tzibbur account.",
            escape_html(&phone)
        ),
    )
    .parse_mode(HTML)
    .reply_markup(KeyboardRemove::new())
    .await?;
    Ok(())
}

pub async fn on_name(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
    phone: String,
) -> Result<()> {
    let raw = msg.text().unwrap_or_default().trim();
    let display_name = if raw.eq_ignore_ascii_case("skip") || raw == "-" {
        None
    } else {
        match validate_display_name(raw) {
            TextValidation::Valid { text } => Some(text),
            TextValidation::Empty => {
                say(&bot, &msg, "Send a name, or <b>skip</b>.").await?;
                return Ok(());
            }
            TextValidation::TooLong { count, max } => {
                say(
                    &bot,
                    &msg,
                    format!("Too long ({count} characters, max {max}). Try a shorter name."),
                )
                .await?;
                return Ok(());
            }
        }
    };
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .build()?;
    match client
        .start_auth(&StartAuthRequest {
            phone: phone.clone(),
            display_name: display_name.clone(),
            region: None,
        })
        .await
    {
        Ok(ch) => {
            dialogue
                .update(State::AwaitCode {
                    challenge_id: ch.challenge_id,
                    phone,
                    display_name,
                    failures: 0,
                })
                .await?;
            say(
                &bot,
                &msg,
                format!(
                    "📨 Code sent. Reply with the 6-digit code from the SMS.{}",
                    ch.resend_after_seconds
                        .map(|s| format!(" You can request a new one with /connect after {s}s."))
                        .unwrap_or_default()
                ),
            )
            .await
        }
        Err(e) => {
            dialogue.exit().await?;
            say(
                &bot,
                &msg,
                format!(
                    "Couldn't start sign-in: {} Use /connect to try again.",
                    escape_html(&friendly(&ae(e)))
                ),
            )
            .await
        }
    }
}

pub async fn on_code(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
    (challenge_id, phone, display_name, failures): (String, String, Option<String>, u32),
) -> Result<()> {
    let tg = from(&msg)?.clone();
    let code: String = msg
        .text()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    // Never keep the OTP around: delete the user's message right away (best effort).
    bot.delete_message(msg.chat.id, msg.id).await.ok();
    if !is_valid_otp(&code) {
        say(
            &bot,
            &msg,
            "Please send the 6-digit code (digits only), or /cancel.",
        )
        .await?;
        return Ok(());
    }
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .build()?;
    let req = VerifyAuthRequest {
        challenge_id: challenge_id.clone(),
        code,
        phone: phone.clone(),
        display_name: display_name.clone(),
        region: None,
    };
    match client.verify_auth(&req).await {
        Ok(session) => {
            dialogue.exit().await?;
            finish_connect(&bot, msg.chat.id, &tg, &app, session).await
        }
        Err(AppError::InvalidCode { .. }) => {
            let failures = failures + 1;
            if failures >= tzibbur_api::constants::SUGGEST_RESEND_AFTER_FAILURES {
                dialogue.exit().await?;
                say(
                    &bot,
                    &msg,
                    "❌ Wrong code three times. Use /connect to request a new one.",
                )
                .await
            } else {
                dialogue
                    .update(State::AwaitCode {
                        challenge_id,
                        phone,
                        display_name,
                        failures,
                    })
                    .await?;
                say(
                    &bot,
                    &msg,
                    "❌ That code is not right. Try again, or /cancel.",
                )
                .await
            }
        }
        Err(e) => {
            dialogue.exit().await?;
            say(
                &bot,
                &msg,
                format!(
                    "Sign-in failed: {} Use /connect to start over.",
                    escape_html(&friendly(&ae(e)))
                ),
            )
            .await
        }
    }
}

/// Shared by the chat flow and the Mini App: persist + start + welcome.
pub async fn finish_connect(
    bot: &BridgeBot,
    chat: ChatId,
    tg: &TgUser,
    app: &App,
    session: Session,
) -> Result<()> {
    let name = session.user.display_name.clone();
    match app.connect(tg, session).await {
        Ok((_account, rt)) => {
            bot.send_message(
                chat,
                format!(
                    "✅ Connected as <b>{}</b>.\n\nImporting your groups now — each one becomes a topic in this chat. Reply inside a topic to post to that group.",
                    escape_html(&name)
                ),
            )
            .parse_mode(HTML)
            .await?;
            // Kick an initial sync so topics appear quickly even before the socket settles.
            let rt2 = rt.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = rt2.sync_refresh().await;
            });
            Ok(())
        }
        Err(e) => {
            bot.send_message(
                chat,
                format!(
                    "Signed in, but I couldn't finish setup: {}",
                    escape_html(&e.to_string())
                ),
            )
            .parse_mode(HTML)
            .await?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// /newgroup dialogue
// ---------------------------------------------------------------------------

pub async fn on_group_name(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
) -> Result<()> {
    let tg = from(&msg)?.clone();
    let rt = connected_runtime(&app, &tg).await?;
    match validate_group_name(msg.text().unwrap_or_default()) {
        TextValidation::Valid { text } => {
            let cats = rt
                .client()
                .group_categories()
                .await
                .map(|c| c.categories)
                .unwrap_or_else(|_| {
                    vec![
                        "family".into(),
                        "neighborhood".into(),
                        "shul".into(),
                        "school".into(),
                        "other".into(),
                    ]
                });
            dialogue
                .update(State::AwaitGroupCategory { name: text.clone() })
                .await?;
            let rows: Vec<Vec<(&str, String)>> = cats
                .chunks(3)
                .map(|ch| {
                    ch.iter()
                        .map(|c| (c.as_str(), format!("cat:{c}")))
                        .collect()
                })
                .collect();
            say_kb(
                &bot,
                msg.chat.id,
                format!("Category for <b>{}</b>?", escape_html(&text)),
                kb(rows),
            )
            .await
        }
        TextValidation::Empty => say(&bot, &msg, "Send a name for the group, or /cancel.").await,
        TextValidation::TooLong { count, max } => {
            say(
                &bot,
                &msg,
                format!("Too long ({count} characters, max {max})."),
            )
            .await
        }
    }
}

pub async fn on_group_phones(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
    (name, category): (String, String),
) -> Result<()> {
    let tg = from(&msg)?.clone();
    let rt = connected_runtime(&app, &tg).await?;
    let raw = msg.text().unwrap_or_default();
    let skip = raw.trim().eq_ignore_ascii_case("skip");
    let (phones, bad) = if skip {
        (vec![], vec![])
    } else {
        parse_phones(&app, raw)
    };
    if !skip && phones.is_empty() {
        say(
            &bot,
            &msg,
            "Send phone numbers like <code>212-736-5000, +972 50 123 4567</code>, or <b>skip</b>.",
        )
        .await?;
        return Ok(());
    }
    if !bad.is_empty() {
        say(
            &bot,
            &msg,
            format!(
                "Skipping (not valid numbers): {}",
                escape_html(&bad.join(", "))
            ),
        )
        .await?;
    }
    dialogue.exit().await?;
    let req = CreateGroupRequest::standard(&name, &category);
    let group = match rt.client().create_group(&req).await {
        Ok(g) => g,
        Err(e) => {
            return say(
                &bot,
                &msg,
                format!(
                    "Could not create the group: {}",
                    escape_html(&friendly(&ae(e)))
                ),
            )
            .await
        }
    };
    let mut report = format!("✅ Created <b>{}</b>.\n", escape_html(&group.name));
    if !phones.is_empty() {
        match rt.client().add_members(&group.id, &phones, None).await {
            Ok(out) => report.push_str(&format_add_outcome(&out)),
            Err(e) => report.push_str(&format!(
                "Could not add members: {}\n",
                escape_html(&friendly(&ae(e)))
            )),
        }
    }
    rt.sync_refresh().await.ok();
    say(&bot, &msg, report).await
}

// ---------------------------------------------------------------------------
// Plain messages: topic replies → Tzibbur
// ---------------------------------------------------------------------------

pub async fn on_message(bot: BridgeBot, msg: Message, app: Arc<App>) -> Result<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let tg = from(&msg)?.clone();
    let Some(conv) = resolve_conversation(&app, &msg).await? else {
        if msg.thread_id.is_none() {
            say(&bot, &msg, "Reply inside one of your group topics to send a message there. /help for commands.").await?;
        }
        return Ok(());
    };
    let rt = match connected_runtime(&app, &tg).await {
        Ok(rt) if rt.account_id == conv.account => rt,
        Ok(_) => return Ok(()),
        Err(e) => return say(&bot, &msg, escape_html(&e.to_string())).await,
    };
    let Some(text) = msg.text().map(str::to_owned) else {
        return say(
            &bot,
            &msg,
            "ℹ️ Tzibbur is text-only; photos, files and voice notes can't be delivered.",
        )
        .await;
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    if text.chars().count() > MAX_OUTBOUND_CHARS {
        return say(
            &bot,
            &msg,
            format!("Too long: Tzibbur allows {MAX_OUTBOUND_CHARS} characters per message."),
        )
        .await;
    }
    if let Err(e) = rt.send_text(&conv, &text, msg.id.0).await {
        let mut r = bot
            .send_message(
                msg.chat.id,
                format!("❌ Not sent: {}.", escape_html(&e.to_string())),
            )
            .parse_mode(HTML)
            .reply_parameters(ReplyParameters::new(msg.id));
        if let Some(t) = conv.topic_id() {
            r = r.message_thread_id(ThreadId(MessageId(t)));
        }
        r.await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

pub async fn on_callback(
    bot: BridgeBot,
    q: CallbackQuery,
    app: Arc<App>,
    storage: Arc<Storage>,
) -> Result<()> {
    let data = q.data.clone().unwrap_or_default();
    let chat_id = match q.chat_id() {
        Some(c) => c,
        None => {
            bot.answer_callback_query(q.id.clone()).await?;
            return Ok(());
        }
    };
    let tg = q.from.clone();
    let dialogue: Dialog = Dialogue::new(storage, chat_id);
    let mut ack = String::new();
    match data.as_str() {
        "noop" => {
            if let Some(m) = q.regular_message() {
                bot.delete_message(chat_id, m.id).await.ok();
            }
        }
        "connect" | "reconnect" => {
            dialogue.update(State::AwaitPhone).await?;
            send_phone_prompt(&bot, chat_id, &app).await?;
        }
        "disc:keep" | "disc:purge" => {
            if let Some(a) = app.account_for(tg.id.0 as i64).await? {
                app.disconnect(&a, data == "disc:purge").await?;
                ack = "Disconnected".into();
                bot.send_message(
                    chat_id,
                    if data == "disc:purge" {
                        "🗑 Disconnected and erased. Your Tzibbur account itself is untouched (Tzibbur has no remote sign-out; the session simply stops being used)."
                    } else {
                        "🔌 Disconnected. Your topics stay; /reconnect brings them back to life."
                    },
                )
                .await?;
            }
        }
        d if d.starts_with("cat:") => {
            let category = d[4..].to_owned();
            if let Some(State::AwaitGroupCategory { name }) = dialogue.get().await? {
                dialogue
                    .update(State::AwaitGroupPhones {
                        name,
                        category: category.clone(),
                    })
                    .await?;
                bot.send_message(
                    chat_id,
                    format!("Category <b>{}</b>. Now send the members' phone numbers (comma-separated), or <b>skip</b>.", escape_html(&category)),
                )
                .parse_mode(HTML)
                .await?;
            }
        }
        d if d.starts_with("leave:") => {
            let conv_id: i64 = d[6..].parse().unwrap_or(0);
            if let (Some(conv), Some(a)) = (
                app.shared.store.conversation(conv_id).await?,
                app.account_for(tg.id.0 as i64).await?,
            ) {
                if conv.account == a.id {
                    let rt = app.runtime(a.id)?;
                    match rt.leave_group(&conv).await {
                        Ok(()) => ack = "Left the group".into(),
                        Err(e) => {
                            bot.send_message(
                                chat_id,
                                format!("Could not leave: {}", escape_html(&friendly(&e))),
                            )
                            .parse_mode(HTML)
                            .await?;
                        }
                    }
                }
            }
        }
        d if d.starts_with("set:") => {
            if let Some(a) = app.account_for(tg.id.0 as i64).await? {
                let mut s = a.settings();
                match &d[4..] {
                    "auto_topics" => s.auto_topics = !s.auto_topics,
                    "auto_mark_read" => s.auto_mark_read = !s.auto_mark_read,
                    "show_phone_numbers" => s.show_phone_numbers = !s.show_phone_numbers,
                    _ => {}
                }
                app.shared.store.update_account_settings(a.id, &s).await?;
                if let Some(rt) = app.registry.get(a.id) {
                    rt.set_settings(s.clone());
                    if s.auto_topics {
                        rt.reconcile_topics().await.ok();
                    }
                }
                if let Some(m) = q.regular_message() {
                    bot.edit_message_reply_markup(chat_id, m.id)
                        .reply_markup(settings_kb(&s))
                        .await
                        .ok();
                }
            }
        }
        _ => {}
    }
    let mut a = bot.answer_callback_query(q.id);
    if !ack.is_empty() {
        a = a.text(ack);
    }
    a.await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Telegram Stars
// ---------------------------------------------------------------------------

pub async fn on_pre_checkout(bot: BridgeBot, q: PreCheckoutQuery) -> Result<()> {
    bot.answer_pre_checkout_query(q.id, true).await?;
    Ok(())
}

pub async fn on_successful_payment(bot: BridgeBot, msg: Message) -> Result<()> {
    if let Some(p) = msg.successful_payment() {
        tracing::info!(chat = msg.chat.id.0, amount = p.total_amount, currency = %p.currency, "donation received");
        say(&bot, &msg, "⭐ Thank you for supporting the bridge!").await?;
    }
    Ok(())
}

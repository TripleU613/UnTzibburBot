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
use tzibbur_api::validation::{is_valid_otp, validate_display_name, validate_group_name};

const HTML: ParseMode = ParseMode::Html;

fn from(msg: &Message) -> Result<&TgUser> {
    msg.from
        .as_ref()
        .ok_or_else(|| anyhow!("message without sender"))
}

/// Which thread a reply to `msg` belongs in: a mapped group topic if the message
/// came from one, otherwise the main thread (the command center).
async fn thread_for(app: &App, msg: &Message) -> Option<ThreadId> {
    let t = msg.thread_id?;
    match app
        .shared
        .store
        .conversation_by_topic(msg.chat.id.0, t.0 .0)
        .await
    {
        Ok(Some(_)) => Some(t),
        _ => None,
    }
}

/// If `msg` arrived in a thread that is not one of the group topics (a thread
/// Telegram opened when the user typed from the thread list), delete that thread:
/// the conversation with the bot lives in the main thread.
async fn tidy_stray_thread(bot: &BridgeBot, app: &App, msg: &Message) {
    let Some(t) = msg.thread_id else { return };
    if thread_for(app, msg).await.is_some() {
        return;
    }
    if let Err(e) = bot.delete_forum_topic(msg.chat.id, t).await {
        tracing::debug!(error = %e, "could not delete stray thread");
    }
}

/// A send_message builder targeted at `thread` (if any).
fn send_in(
    bot: &BridgeBot,
    chat: ChatId,
    thread: Option<ThreadId>,
    text: impl Into<String>,
) -> <BridgeBot as teloxide::requests::Requester>::SendMessage {
    let mut r = bot.send_message(chat, text).parse_mode(HTML);
    if let Some(t) = thread {
        r = r.message_thread_id(t);
    }
    r
}

async fn say(bot: &BridgeBot, msg: &Message, app: &App, text: impl Into<String>) -> Result<()> {
    let thread = thread_for(app, msg).await;
    send_in(bot, msg.chat.id, thread, text).await?;
    Ok(())
}

async fn say_kb(
    bot: &BridgeBot,
    msg: &Message,
    app: &App,
    text: impl Into<String>,
    kb: InlineKeyboardMarkup,
) -> Result<()> {
    let thread = thread_for(app, msg).await;
    send_in(bot, msg.chat.id, thread, text)
        .reply_markup(kb)
        .await?;
    Ok(())
}

/// Send to the user's main thread when we only have the user (callbacks, Mini App).
async fn say_home(
    app: &App,
    tg: &TgUser,
    text: impl Into<String>,
    kb: Option<InlineKeyboardMarkup>,
) -> Result<()> {
    let user = app.bridge_user(tg).await?;
    crate::bridge::send_home(&app.shared, &user, &text.into(), kb).await?;
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
            &app,
            "I only work in a private chat. Open @{} directly.".replace("{}", &app.bot_username),
        )
        .await?;
        return Ok(());
    }
    tidy_stray_thread(&bot, &app, &msg).await;
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
                    "👋 <b>Tzibbur ↔ Telegram</b>\n\nI turn each of your Tzibbur groups into a topic in this chat, so you can read and reply from Telegram. This main thread is the command center.\n\n• Tzibbur stays the source of truth; I keep only ids and an encrypted session.\n• Messages are plain text (that's all Tzibbur supports).\n\nTap <b>/connect</b> to sign in with your phone number{}. See /privacy for what is (and isn't) stored.",
                    if app.shared.cfg.public_url.is_some() { ", or use the ≡ menu button for the secure login page" } else { "" }
                ),
            };
            let mut r = send_in(&bot, msg.chat.id, thread_for(&app, &msg).await, text);
            if app.account_for(tg.id.0 as i64).await?.is_none() {
                r = r.reply_markup(kb(vec![vec![("🔗 Connect Tzibbur", "connect".into())]]));
            }
            r.await?;
        }
        Command::Help => say(&bot, &msg, &app, escape_html(&Command::descriptions().to_string())).await?,
        Command::Cancel => {
            dialogue.exit().await?;
            say(&bot, &msg, &app, "Cancelled.").await?;
        }
        Command::Connect | Command::Reconnect => begin_connect(&bot, &msg, &app, &dialogue).await?,
        Command::Disconnect => {
            match app.account_for(tg.id.0 as i64).await? {
                Some(a) if a.status != AccountStatus::Disconnected => {
                    say_kb(&bot, &msg, &app,
                        "Disconnect your Tzibbur account from Telegram?\n\n<b>Disconnect</b> stops forwarding and deletes the stored session but keeps the topic mappings for an easy reconnect.\n<b>Disconnect &amp; erase</b> also removes all mappings.",
                        kb(vec![
                            vec![("🔌 Disconnect", "disc:keep".into()), ("🗑 Disconnect & erase", "disc:purge".into())],
                            vec![("Cancel", "noop".into())],
                        ]),
                    )
                    .await?
                }
                _ => say(&bot, &msg, &app, "No Tzibbur account is connected.").await?,
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
            say(&bot, &msg, &app, text).await?;
        }
        Command::Sync => {
            let rt = connected_runtime(&app, &tg).await?;
            match rt.sync_refresh().await {
                Ok(()) => say(&bot, &msg, &app, "🔄 Synced.").await?,
                Err(e) => say(&bot, &msg, &app, format!("Sync failed: {}", escape_html(&friendly(&e)))).await?,
            }
        }
        Command::Chats => {
            let rt = connected_runtime(&app, &tg).await?;
            let rows = rt.local().groups_with_unread(&rt.tzibbur_user_id)?;
            if rows.is_empty() {
                say(&bot, &msg, &app, "No groups yet. Use /newgroup to create one.").await?;
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
                say(&bot, &msg, &app, out).await?;
            }
        }
        Command::NewGroup => {
            connected_runtime(&app, &tg).await?;
            dialogue.update(State::AwaitGroupName).await?;
            say(&bot, &msg, &app, "What should the group be called? (max 100 characters, /cancel to abort)").await?;
        }
        Command::Add(arg) => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let (phones, bad) = parse_phones(&app, &arg);
            if phones.is_empty() {
                say(&bot, &msg, &app, "Usage inside a group topic: <code>/add 212-736-5000, +972 50 123 4567</code>").await?;
                return Ok(());
            }
            if !bad.is_empty() {
                say(&bot, &msg, &app, format!("Skipping (not valid numbers): {}", escape_html(&bad.join(", ")))).await?;
            }
            match rt.client().add_members(&conv.group_id, &phones, None).await {
                Ok(out) => {
                    say(&bot, &msg, &app, format_add_outcome(&out)).await?;
                    rt.sync_refresh_members(&conv.group_id).await.ok();
                }
                Err(e) => say(&bot, &msg, &app, format!("Could not add members: {}", escape_html(&friendly(&ae(e))))).await?,
            }
        }
        Command::Group => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let (text, kb) = super::groups::card(&rt, &conv).await?;
            say_kb(&bot, &msg, &app, text, kb).await?;
        }
        Command::Rename(arg) => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            match validate_group_name(&arg) {
                TextValidation::Valid { text } => match rt.rename_group(&conv, &text).await {
                    Ok(()) => say(&bot, &msg, &app, format!("✏️ Renamed to <b>{}</b>.", escape_html(&text))).await?,
                    Err(e) => say(&bot, &msg, &app, format!("Could not rename: {}", escape_html(&e.to_string()))).await?,
                },
                TextValidation::Empty => {
                    dialogue.update(State::AwaitRename { conv_id: conv.id }).await?;
                    say(&bot, &msg, &app, "Send the new group name (max 100 characters), or /cancel.").await?;
                }
                TextValidation::TooLong { count, max } => say(&bot, &msg, &app, format!("Too long ({count} characters, max {max}).")).await?,
            }
        }
        Command::Manage => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let (text, kb) = super::groups::members_view(&rt, &conv).await?;
            say_kb(&bot, &msg, &app, text, kb).await?;
        }
        Command::DeleteGroup => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let (g, _) = rt.group_details(&conv).await?;
            if g.my_role != tzibbur_api::models::Role::Admin {
                say(&bot, &msg, &app, "Only admins can delete a group.").await?;
            } else {
                say_kb(
                    &bot,
                    &msg,
                    &app,
                    format!("Delete <b>{}</b> for all members? This cannot be undone.", escape_html(&g.name)),
                    kb(vec![vec![("🗑 Delete for everyone", format!("g:{}:del2", conv.id)), ("Cancel", "noop".into())]]),
                )
                .await?;
            }
        }
        Command::Read => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let seq = rt.mark_read(&conv).await?;
            say(&bot, &msg, &app, if seq > 0 { "✅ Marked read on Tzibbur." } else { "Nothing to mark." }).await?;
        }
        Command::Contacts(arg) => {
            let rt = connected_runtime(&app, &tg).await?;
            let (phones, bad) = parse_phones(&app, &arg);
            if phones.is_empty() {
                say(&bot, &msg, &app, "Usage: <code>/contacts 212-736-5000, +972 50 123 4567</code> — tells you who is on Tzibbur.").await?;
                return Ok(());
            }
            match rt.client().check_contacts_batched(&phones, None).await {
                Ok(reg) => {
                    let on: Vec<String> = reg.iter().filter_map(|c| c.phone_e164.clone()).collect();
                    let off: Vec<String> = phones.iter().filter(|p| !on.contains(p)).cloned().collect();
                    let mut out = String::new();
                    if !on.is_empty() {
                        out.push_str(&format!("✅ On Tzibbur: {}\n", escape_html(&on.join(", "))));
                    }
                    if !off.is_empty() {
                        out.push_str(&format!("❌ Not on Tzibbur: {}\n", escape_html(&off.join(", "))));
                    }
                    if !bad.is_empty() {
                        out.push_str(&format!("Skipped (not valid numbers): {}", escape_html(&bad.join(", "))));
                    }
                    say(&bot, &msg, &app, out).await?;
                }
                Err(e) => say(&bot, &msg, &app, format!("Lookup failed: {}", escape_html(&friendly(&ae(e))))).await?,
            }
        }
        Command::Devices => {
            let rt = connected_runtime(&app, &tg).await?;
            match rt.client().devices().await {
                Ok(devs) => {
                    let mut out = format!("<b>Your Tzibbur devices</b> ({})\n", devs.len());
                    for d in devs {
                        let seen = d.last_seen_at.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms)).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "never".into());
                        out.push_str(&format!(
                            "• {} {} — last seen {}\n",
                            escape_html(d.platform.as_deref().unwrap_or("?")),
                            escape_html(d.device_model.as_deref().unwrap_or("")),
                            seen
                        ));
                    }
                    out.push_str("\nThe bridge itself shows up as one of these (android · Pixel 7 by default).");
                    say(&bot, &msg, &app, out).await?;
                }
                Err(e) => say(&bot, &msg, &app, format!("Could not list devices: {}", escape_html(&friendly(&ae(e))))).await?,
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
            say(&bot, &msg, &app, out).await?;
        }
        Command::Leave => {
            let (_rt, conv) = topic_context(&app, &tg, &msg).await?;
            say_kb(&bot, &msg, &app,
                format!("Leave <b>{}</b>?", escape_html(conv.name.as_deref().unwrap_or("this group"))),
                kb(vec![vec![("🚪 Leave", format!("leave:{}", conv.id)), ("Cancel", "noop".into())]]),
            )
            .await?;
        }
        Command::Mute => {
            let (rt, conv) = topic_context(&app, &tg, &msg).await?;
            let g = rt.local().get_group(&conv.group_id)?.ok_or_else(|| anyhow!("group not cached"))?;
            rt.local().set_muted(&g.id, !g.muted)?;
            say(&bot, &msg, &app, if g.muted { "🔔 Unmuted (bridge-local flag)." } else { "🔕 Muted (bridge-local flag)." }).await?;
        }
        Command::Name(name) => {
            let rt = connected_runtime(&app, &tg).await?;
            match validate_display_name(&name) {
                TextValidation::Valid { text } => match rt.client().update_display_name(&text).await {
                    Ok(u) => {
                        let acc = app.account_for(tg.id.0 as i64).await?.ok_or_else(|| anyhow!("no account"))?;
                        app.shared.store.update_account_display_name(acc.id, &u.display_name).await?;
                        say(&bot, &msg, &app, format!("Display name is now <b>{}</b>.", escape_html(&u.display_name))).await?;
                    }
                    Err(e) => say(&bot, &msg, &app, friendly(&ae(e))).await?,
                },
                TextValidation::Empty => say(&bot, &msg, &app, "Usage: <code>/name Your Name</code>").await?,
                TextValidation::TooLong { count, max } => say(&bot, &msg, &app, format!("Too long: {count} characters, max {max}.")).await?,
            }
        }
        Command::Settings => {
            let acc = app.account_for(tg.id.0 as i64).await?.ok_or_else(|| anyhow!("Connect first with /connect."))?;
            let s = acc.settings();
            say_kb(&bot, &msg, &app, "<b>Settings</b>", settings_kb(&s)).await?;
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
        Command::Privacy => say(&bot, &msg, &app, PRIVACY_TEXT).await?,
        Command::Legal => {
            let client = TzibburClient::builder().base_url(app.shared.cfg.tzibbur_base_url.clone()).device(app.shared.cfg.device.clone()).build()?;
            for key in [LegalDocKey::Terms, LegalDocKey::Privacy] {
                match client.legal(key).await {
                    Ok(doc) => {
                        let text: String = doc.markdown.chars().take(3800).collect();
                        say(&bot, &msg, &app, format!("<b>{}</b>\n<pre>{}</pre>", key.as_str(), escape_html(&text))).await?;
                    }
                    Err(e) => say(&bot, &msg, &app, format!("Could not fetch {}: {}", key.as_str(), escape_html(&e.to_string()))).await?,
                }
            }
        }
    }
    Ok(())
}

pub const PRIVACY_TEXT: &str = "<b>What this bridge keeps, and who can see it</b>\n\n\
<b>Stored on the bridge server</b>\n\
• Your Telegram id, your Tzibbur user id and phone, the list of your groups, and which Telegram topic each maps to.\n\
• Message <i>ids</i> and sequence numbers (to avoid duplicates). Message <b>text is erased from the server the moment it is delivered</b> to Telegram or confirmed by Tzibbur.\n\
• Your Tzibbur session token, encrypted. The bridge needs it to stay connected for you 24/7, so the server operator technically holds it — the same trust you place in any always-on relay.\n\n\
<b>Not stored</b>\n\
• Message text at rest, photos (Tzibbur has none), SMS codes, your Telegram messages.\n\n\
<b>What the operator cannot do</b>\n\
• Read your Telegram chat history — Telegram never gives bots that.\n\
• Read past Tzibbur messages from the bridge — there is no text on disk.\n\n\
<b>Your controls</b>\n\
• /disconnect erases the session; “Disconnect &amp; erase” also removes every mapping.\n\
• Nothing about you is logged beyond ids and error codes.";

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

pub fn format_add_outcome(out: &tzibbur_api::models::AddMembersOutcome) -> String {
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
                app,
                "You're already connected. Use /disconnect first if you want to switch accounts.",
            )
            .await?;
            return Ok(());
        }
    }
    dialogue.update(State::AwaitPhone).await?;
    let thread = thread_for(app, msg).await;
    send_phone_prompt(bot, msg.chat.id, thread, app).await
}

/// Ask for the phone with a one-tap "share my number" button.
pub async fn send_phone_prompt(
    bot: &BridgeBot,
    chat: ChatId,
    thread: Option<ThreadId>,
    app: &App,
) -> Result<()> {
    let mut text = format!(
        "📱 Type your phone number any way you like — <code>212-736-5000</code>, <code>+972 50 123 4567</code>… (numbers without a country code are treated as {}). If Telegram shows a “📱 Use my Telegram number” button, you can tap that instead.\n\nTzibbur will text you a 6-digit code. I never store the code, only the resulting session (encrypted).",
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
    send_in(bot, chat, thread, text).reply_markup(kb).await?;
    Ok(())
}

pub async fn on_phone(bot: BridgeBot, msg: Message, dialogue: Dialog, app: Arc<App>) -> Result<()> {
    tidy_stray_thread(&bot, &app, &msg).await;
    let tg = from(&msg)?;
    // Shared contact (the button), or typed text.
    let raw = match msg.contact() {
        Some(c) => {
            if c.user_id.map(|u| u != tg.id).unwrap_or(false) {
                say(
                    &bot,
                    &msg,
                    &app,
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
            say(&bot, &msg, &app, format!(
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
    let thread = thread_for(&app, &msg).await;
    send_in(&bot, msg.chat.id, thread, format!(
            "Using <b>{}</b>.\n\nWhat display name should other Tzibbur members see? Reply <b>skip</b> if you already have a Tzibbur account.",
            escape_html(&phone)
        ))
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
    tidy_stray_thread(&bot, &app, &msg).await;
    let raw = msg.text().unwrap_or_default().trim();
    let display_name = if raw.eq_ignore_ascii_case("skip") || raw == "-" {
        None
    } else {
        match validate_display_name(raw) {
            TextValidation::Valid { text } => Some(text),
            TextValidation::Empty => {
                say(&bot, &msg, &app, "Send a name, or <b>skip</b>.").await?;
                return Ok(());
            }
            TextValidation::TooLong { count, max } => {
                say(
                    &bot,
                    &msg,
                    &app,
                    format!("Too long ({count} characters, max {max}). Try a shorter name."),
                )
                .await?;
                return Ok(());
            }
        }
    };
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .device(app.shared.cfg.device.clone())
        .build()?;
    match client
        .start_auth(&StartAuthRequest {
            phone: phone.clone(),
            display_name: display_name.clone(),
            region: None,
            ..Default::default()
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
                &app,
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
                &app,
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
    tidy_stray_thread(&bot, &app, &msg).await;
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
            &app,
            "Please send the 6-digit code (digits only), or /cancel.",
        )
        .await?;
        return Ok(());
    }
    let client = TzibburClient::builder()
        .base_url(app.shared.cfg.tzibbur_base_url.clone())
        .device(app.shared.cfg.device.clone())
        .build()?;
    let req = VerifyAuthRequest {
        challenge_id: challenge_id.clone(),
        code,
        phone: phone.clone(),
        display_name: display_name.clone(),
        region: None,
        ..Default::default()
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
                    &app,
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
                    &app,
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
                &app,
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
    _bot: &BridgeBot,
    _chat: ChatId,
    tg: &TgUser,
    app: &App,
    session: Session,
) -> Result<()> {
    let name = session.user.display_name.clone();
    match app.connect(tg, session).await {
        Ok((_account, rt)) => {
            say_home(app, tg, format!(
                    "✅ Connected as <b>{}</b>.\n\nImporting your groups now — each one becomes a topic in this chat. Reply inside a topic to post to that group.",
                    escape_html(&name)
                ), None).await?;
            // Kick an initial sync so topics appear quickly even before the socket settles.
            let rt2 = rt.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = rt2.sync_refresh().await;
            });
            Ok(())
        }
        Err(e) => {
            say_home(
                app,
                tg,
                format!(
                    "Signed in, but I couldn't finish setup: {}",
                    escape_html(&e.to_string())
                ),
                None,
            )
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
    tidy_stray_thread(&bot, &app, &msg).await;
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
                &msg,
                &app,
                format!("Category for <b>{}</b>?", escape_html(&text)),
                kb(rows),
            )
            .await
        }
        TextValidation::Empty => {
            say(&bot, &msg, &app, "Send a name for the group, or /cancel.").await
        }
        TextValidation::TooLong { count, max } => {
            say(
                &bot,
                &msg,
                &app,
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
    tidy_stray_thread(&bot, &app, &msg).await;
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
            &app,
            "Send phone numbers like <code>212-736-5000, +972 50 123 4567</code>, or <b>skip</b>.",
        )
        .await?;
        return Ok(());
    }
    if !bad.is_empty() {
        say(
            &bot,
            &msg,
            &app,
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
                &app,
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
    say(&bot, &msg, &app, report).await
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
        tidy_stray_thread(&bot, &app, &msg).await;
        say(
            &bot,
            &msg,
            &app,
            "This is the command center — to write to a group, open its topic. /help for commands.",
        )
        .await?;
        return Ok(());
    };
    let rt = match connected_runtime(&app, &tg).await {
        Ok(rt) if rt.account_id == conv.account => rt,
        Ok(_) => return Ok(()),
        Err(e) => return say(&bot, &msg, &app, escape_html(&e.to_string())).await,
    };
    let Some(text) = msg.text().map(str::to_owned) else {
        return say(
            &bot,
            &msg,
            &app,
            "ℹ️ Tzibbur is text-only; photos, files and voice notes can't be delivered.",
        )
        .await;
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    // Replying to a forwarded message: quote it, since Tzibbur has no reply threading.
    let text = match msg.reply_to_message() {
        Some(r) if r.text().is_some() => quote_prefix(r, &rt.tzibbur_user_id) + &text,
        _ => text,
    };
    // Tzibbur caps a message at MAX_OUTBOUND_CHARS: split long texts on whitespace.
    let parts = split_for_tzibbur(&text, MAX_OUTBOUND_CHARS);
    if parts.len() > 1 {
        say(
            &bot,
            &msg,
            &app,
            format!("ℹ️ Long message — sending it as {} parts.", parts.len()),
        )
        .await?;
    }
    for part in parts {
        if let Err(e) = rt.send_text(&conv, &part, msg.id.0).await {
            let e = anyhow::anyhow!(e);
            return send_failed(&bot, &msg, &conv, &e).await;
        }
    }
    Ok(())
}

async fn send_failed(
    bot: &BridgeBot,
    msg: &Message,
    conv: &Conversation,
    e: &anyhow::Error,
) -> Result<()> {
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
    Ok(())
}

/// `↩ Name: “snippet…”` built from the Telegram message being replied to.
fn quote_prefix(reply: &Message, _self_id: &str) -> String {
    let raw = reply.text().unwrap_or_default();
    let from_bot = reply.from.as_ref().map(|u| u.is_bot).unwrap_or(false);
    let (label, body) = if from_bot {
        match raw.split_once('\n') {
            Some((l, b)) => (l.trim().to_owned(), b.trim()),
            None => ("".to_owned(), raw.trim()),
        }
    } else {
        ("You".to_owned(), raw.trim())
    };
    if body.is_empty() || body.starts_with("🕘") || body.starts_with("🚫") {
        return String::new();
    }
    let snippet: String = body.chars().take(80).collect();
    let ell = if body.chars().count() > 80 { "…" } else { "" };
    if label.is_empty() {
        format!("↩ “{snippet}{ell}”\n")
    } else {
        format!("↩ {label}: “{snippet}{ell}”\n")
    }
}

/// Split on whitespace into chunks of at most `max` characters.
pub fn split_for_tzibbur(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        if cur.chars().count() + word.chars().count() > max && !cur.is_empty() {
            out.push(cur.trim_end().to_owned());
            cur = String::new();
        }
        if word.chars().count() > max {
            // A single huge token: hard-split.
            let mut w = String::new();
            for c in word.chars() {
                if w.chars().count() >= max {
                    out.push(std::mem::take(&mut w));
                }
                w.push(c);
            }
            cur.push_str(&w);
        } else {
            cur.push_str(word);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim_end().to_owned());
    }
    out
}

#[allow(dead_code)]
async fn _unused_send_path(
    bot: &BridgeBot,
    msg: &Message,
    conv: &Conversation,
    rt: &crate::bridge::AccountRuntime,
    text: String,
) -> Result<()> {
    if let Err(e) = rt.send_text(conv, &text, msg.id.0).await {
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
    // Reply in the group topic the button lives in, else in the main thread.
    let thread = match q.regular_message() {
        Some(m) => thread_for(&app, m).await,
        None => None,
    };
    let mut ack = String::new();
    match data.as_str() {
        "noop" => {
            if let Some(m) = q.regular_message() {
                bot.delete_message(chat_id, m.id).await.ok();
            }
        }
        "connect" | "reconnect" => {
            dialogue.update(State::AwaitPhone).await?;
            send_phone_prompt(&bot, chat_id, thread, &app).await?;
        }
        "disc:keep" | "disc:purge" => {
            if let Some(a) = app.account_for(tg.id.0 as i64).await? {
                app.disconnect(&a, data == "disc:purge").await?;
                ack = "Disconnected".into();
                send_in(&bot, chat_id, thread,
                    if data == "disc:purge" {
                        "🗑 Disconnected and erased. Your Tzibbur account itself is untouched (Tzibbur has no remote sign-out; the session simply stops being used)."
                    } else {
                        "🔌 Disconnected. Your topics stay; /reconnect brings them back to life."
                    },
                )
                .await?;
            }
        }
        d if d.starts_with("g:") || d.starts_with("m:") || d.starts_with("ma:") => {
            match super::groups::on_callback(&bot, &app, &q, &dialogue, d).await {
                Ok(t) => ack = t,
                Err(e) => {
                    ack = format!("⚠️ {}", e.to_string().chars().take(180).collect::<String>());
                }
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
                send_in(&bot, chat_id, thread,
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
                            send_in(
                                &bot,
                                chat_id,
                                thread,
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

pub async fn on_successful_payment(bot: BridgeBot, msg: Message, app: Arc<App>) -> Result<()> {
    if let Some(p) = msg.successful_payment() {
        tracing::info!(chat = msg.chat.id.0, amount = p.total_amount, currency = %p.currency, "donation received");
        say(&bot, &msg, &app, "⭐ Thank you for supporting the bridge!").await?;
    }
    Ok(())
}

/// Tzibbur messages cannot be edited: tell the user once per edit inside a group topic.
pub async fn on_edited(bot: BridgeBot, msg: Message, app: Arc<App>) -> Result<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    if resolve_conversation(&app, &msg).await?.is_some() {
        let mut r = bot
            .send_message(msg.chat.id, "ℹ️ Tzibbur messages can't be edited after sending. Send the correction as a new message.")
            .reply_parameters(ReplyParameters::new(msg.id));
        if let Some(t) = msg.thread_id {
            r = r.message_thread_id(t);
        }
        r.await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_for_tzibbur;
    #[test]
    fn splits_on_whitespace() {
        let words: Vec<String> = (0..300).map(|i| format!("word{i}")).collect();
        let text = words.join(" ");
        let parts = split_for_tzibbur(&text, 1000);
        assert!(parts.len() >= 2);
        assert!(parts.iter().all(|p| p.chars().count() <= 1000));
        assert_eq!(parts.join(" ").split_whitespace().count(), 300);
        assert_eq!(split_for_tzibbur("short", 1000), vec!["short"]);
    }
}

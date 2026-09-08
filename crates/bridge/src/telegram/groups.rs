//! Group management UI: the group card with inline buttons (members, add,
//! rename, permissions, mark read, leave, delete), member pickers with
//! promote / demote / remove, and the two dialogue steps they need.

use super::{Dialog, State};
use crate::app::App;
use crate::bridge::format::escape_html;
use crate::bridge::{AccountRuntime, BridgeBot};
use crate::phone::parse_phone_list;
use crate::store::Conversation;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use teloxide::dispatching::dialogue::GetChatId;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, ThreadId,
};
use tzibbur_api::models::{GroupKind, Permission, Role};
use tzibbur_api::validation::{validate_group_name, TextValidation};

const HTML: ParseMode = ParseMode::Html;

fn btn(text: impl Into<String>, data: impl Into<String>) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text, data)
}

fn perm_label(p: &Permission) -> &'static str {
    if p.as_str().eq_ignore_ascii_case("admins") {
        "admins"
    } else {
        "everyone"
    }
}

fn flip(p: &Permission) -> Permission {
    if perm_label(p) == "admins" {
        Permission::everyone()
    } else {
        Permission::admins()
    }
}

/// Build the group card (text + keyboard) for a conversation.
pub async fn card(
    rt: &AccountRuntime,
    conv: &Conversation,
) -> Result<(String, InlineKeyboardMarkup)> {
    let (g, live) = rt.group_details(conv).await?;
    let is_admin = g.my_role == Role::Admin;
    let min_members = live
        .as_ref()
        .and_then(|l| l.limits.as_ref())
        .and_then(|l| l.min_members_to_post)
        .unwrap_or(0) as i64;
    let max_len = live
        .as_ref()
        .map(|l| l.message_max_length())
        .unwrap_or(crate::bridge::MAX_OUTBOUND_CHARS);
    let unread = live.as_ref().and_then(|l| l.unread_count).unwrap_or(0);

    let mut text = format!(
        "<b>{}</b>\n{} · {} member(s) · you are {}{}\n",
        escape_html(&g.name),
        escape_html(&g.category),
        g.member_count,
        if is_admin { "an admin" } else { "a member" },
        if g.muted { " · muted" } else { "" }
    );
    if g.kind == GroupKind::System {
        text.push_str("System announcements thread — read-only.\n");
    } else {
        text.push_str(&format!(
            "Who can post: <b>{}</b> · Who can add members: <b>{}</b>\nMax message: {} characters",
            perm_label(&g.who_can_post),
            perm_label(&g.who_can_add_members),
            max_len
        ));
        if unread > 0 {
            text.push_str(&format!(" · {unread} unread on Tzibbur"));
        }
        text.push('\n');
        if g.member_count < min_members {
            text.push_str(&format!(
                "\nTzibbur requires <b>{min_members}</b> members before anyone can post here. Add {} more with <b>Add members</b>.\n",
                min_members - g.member_count
            ));
        }
    }

    let c = conv.id;
    let mut rows: Vec<Vec<InlineKeyboardButton>> = vec![];
    if g.kind != GroupKind::System {
        let mut row = vec![btn("Members", format!("g:{c}:members"))];
        if g.can_add_members() {
            row.push(btn("Add members", format!("g:{c}:add")));
        }
        rows.push(row);
        if is_admin {
            rows.push(vec![btn("Rename", format!("g:{c}:rename"))]);
            rows.push(vec![
                btn(
                    format!("Post: {} ", perm_label(&g.who_can_post)),
                    format!("g:{c}:post"),
                ),
                btn(
                    format!("Add: {} ", perm_label(&g.who_can_add_members)),
                    format!("g:{c}:addm"),
                ),
            ]);
        }
    }
    let mut row = vec![
        btn("Mark read", format!("g:{c}:read")),
        btn(
            if g.muted { "Unmute" } else { "Mute" },
            format!("g:{c}:mute"),
        ),
    ];
    if g.kind != GroupKind::System {
        row.push(btn("Leave", format!("g:{c}:leave")));
    }
    rows.push(row);
    if is_admin && g.kind != GroupKind::System {
        rows.push(vec![btn("Delete group…", format!("g:{c}:del1"))]);
    }
    rows.push(vec![btn("Close", "noop")]);
    Ok((text, InlineKeyboardMarkup::new(rows)))
}

/// Members list with a button per member (admins can act on them).
pub async fn members_view(
    rt: &AccountRuntime,
    conv: &Conversation,
) -> Result<(String, InlineKeyboardMarkup)> {
    rt.sync_refresh_members(&conv.group_id).await.ok();
    let g = rt
        .local()
        .get_group(&conv.group_id)?
        .ok_or_else(|| anyhow!("group not cached"))?;
    let members = rt.local().members(&conv.group_id)?;
    let is_admin = g.my_role == Role::Admin;
    let mut text = format!(
        "<b>{}</b> — {} member(s)\n",
        escape_html(&g.name),
        members.len()
    );
    let mut rows = vec![];
    for m in &members {
        let role = if m.role == Role::Admin { " " } else { "" };
        let you = if m.user_id == rt.tzibbur_user_id {
            " (you)"
        } else {
            ""
        };
        let phone = m
            .phone_e164
            .as_deref()
            .map(|p| format!(" · {p}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "• {}{}{}{}\n",
            escape_html(&m.display_name),
            role,
            you,
            escape_html(&phone)
        ));
        if is_admin
            && m.user_id != rt.tzibbur_user_id
            && !m.user_id.starts_with("00000000-0000-7000-8000-")
        {
            rows.push(vec![btn(
                format!("{}{}", m.display_name, role),
                format!("m:{}:{}", conv.id, m.user_id),
            )]);
        }
    }
    if is_admin && rows.is_empty() {
        text.push_str("\nNo other members to manage yet.");
    } else if is_admin {
        text.push_str("\nTap a member to promote, demote or remove.");
    }
    rows.push(vec![
        btn("Back", format!("g:{}:card", conv.id)),
        btn("Close", "noop"),
    ]);
    Ok((text, InlineKeyboardMarkup::new(rows)))
}

fn member_menu(
    conv: &Conversation,
    name: &str,
    user_id: &str,
    role: Role,
) -> (String, InlineKeyboardMarkup) {
    let c = conv.id;
    let mut rows = vec![];
    match role {
        Role::Admin => rows.push(vec![btn("Make member", format!("ma:{c}:{user_id}:m"))]),
        Role::Member => rows.push(vec![btn("Make admin", format!("ma:{c}:{user_id}:a"))]),
    }
    rows.push(vec![btn(
        "Remove from group",
        format!("ma:{c}:{user_id}:r"),
    )]);
    rows.push(vec![
        btn("Back", format!("g:{c}:members")),
        btn("Close", "noop"),
    ]);
    (
        format!(
            "<b>{}</b> — {}",
            escape_html(name),
            if role == Role::Admin {
                "admin"
            } else {
                "member"
            }
        ),
        InlineKeyboardMarkup::new(rows),
    )
}

async fn edit_or_send(
    bot: &BridgeBot,
    q: &CallbackQuery,
    chat: ChatId,
    thread: Option<ThreadId>,
    text: String,
    kb: InlineKeyboardMarkup,
) -> Result<()> {
    if let Some(m) = q.regular_message() {
        if bot
            .edit_message_text(chat, m.id, text.clone())
            .parse_mode(HTML)
            .reply_markup(kb.clone())
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    let mut r = bot
        .send_message(chat, text)
        .parse_mode(HTML)
        .reply_markup(kb);
    if let Some(t) = thread {
        r = r.message_thread_id(t);
    }
    r.await?;
    Ok(())
}

/// Resolve the conversation for a callback and make sure it belongs to this user's account.
async fn conv_for(
    app: &App,
    tg_user_id: i64,
    conv_id: i64,
) -> Result<(Arc<AccountRuntime>, Conversation)> {
    let account = app
        .account_for(tg_user_id)
        .await?
        .ok_or_else(|| anyhow!("not connected"))?;
    let conv = app
        .shared
        .store
        .conversation(conv_id)
        .await?
        .ok_or_else(|| anyhow!("unknown group"))?;
    if conv.account != account.id {
        return Err(anyhow!("not your group"));
    }
    Ok((app.runtime(account.id)?, conv))
}

/// Handle `g:`, `m:` and `ma:` callbacks. Returns the toast text.
pub async fn on_callback(
    bot: &BridgeBot,
    app: &App,
    q: &CallbackQuery,
    dialogue: &Dialog,
    data: &str,
) -> Result<String> {
    let chat = q.chat_id().ok_or_else(|| anyhow!("no chat"))?;
    let thread = q.regular_message().and_then(|m| m.thread_id);
    let parts: Vec<&str> = data.split(':').collect();
    let tg_id = q.from.id.0 as i64;
    match parts.as_slice() {
        ["g", c, action] => {
            let conv_id: i64 = c.parse()?;
            let (rt, conv) = conv_for(app, tg_id, conv_id).await?;
            let mut toast = String::new();
            match *action {
                "card" => {}
                "members" => {
                    let (t, kb) = members_view(&rt, &conv).await?;
                    edit_or_send(bot, q, chat, thread, t, kb).await?;
                    return Ok(toast);
                }
                "add" => {
                    dialogue.update(State::AwaitAddPhones { conv_id }).await?;
                    let mut r = bot
                        .send_message(
                            chat,
                            "Send the phone numbers to add (comma-separated), or /cancel.",
                        )
                        .parse_mode(HTML);
                    if let Some(t) = thread {
                        r = r.message_thread_id(t);
                    }
                    r.await?;
                    return Ok(toast);
                }
                "rename" => {
                    dialogue.update(State::AwaitRename { conv_id }).await?;
                    let mut r = bot
                        .send_message(
                            chat,
                            "Send the new group name (max 100 characters), or /cancel.",
                        )
                        .parse_mode(HTML);
                    if let Some(t) = thread {
                        r = r.message_thread_id(t);
                    }
                    r.await?;
                    return Ok(toast);
                }
                "post" | "addm" => {
                    let (g, _) = rt.group_details(&conv).await?;
                    if *action == "post" {
                        rt.set_permission(&conv, Some(flip(&g.who_can_post)), None)
                            .await?;
                    } else {
                        rt.set_permission(&conv, None, Some(flip(&g.who_can_add_members)))
                            .await?;
                    }
                    toast = "Updated".into();
                }
                "read" => {
                    let seq = rt.mark_read(&conv).await?;
                    toast = if seq > 0 {
                        "Marked read".into()
                    } else {
                        "Nothing to mark".into()
                    };
                }
                "mute" => {
                    let g = rt
                        .local()
                        .get_group(&conv.group_id)?
                        .ok_or_else(|| anyhow!("group not cached"))?;
                    rt.local().set_muted(&g.id, !g.muted)?;
                    toast = if g.muted {
                        "Unmuted".into()
                    } else {
                        "Muted".into()
                    };
                }
                "leave" => {
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        btn("Yes, leave", format!("leave:{conv_id}")),
                        btn("Cancel", format!("g:{conv_id}:card")),
                    ]]);
                    edit_or_send(
                        bot,
                        q,
                        chat,
                        thread,
                        format!(
                            "Leave <b>{}</b>?",
                            escape_html(conv.name.as_deref().unwrap_or("this group"))
                        ),
                        kb,
                    )
                    .await?;
                    return Ok(toast);
                }
                "del1" => {
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        btn("Delete for everyone", format!("g:{conv_id}:del2")),
                        btn("Cancel", format!("g:{conv_id}:card")),
                    ]]);
                    edit_or_send(
                        bot,
                        q,
                        chat,
                        thread,
                        format!(
                            "Delete <b>{}</b> for all members? This cannot be undone.",
                            escape_html(conv.name.as_deref().unwrap_or("this group"))
                        ),
                        kb,
                    )
                    .await?;
                    return Ok(toast);
                }
                "del2" => {
                    rt.delete_group(&conv).await?;
                    if let Some(m) = q.regular_message() {
                        bot.delete_message(chat, m.id).await.ok();
                    }
                    return Ok("Group deleted".into());
                }
                _ => {}
            }
            let (t, kb) = card(&rt, &conv).await?;
            edit_or_send(bot, q, chat, thread, t, kb).await?;
            Ok(toast)
        }
        ["m", c, uid] => {
            let (rt, conv) = conv_for(app, tg_id, c.parse()?).await?;
            let members = rt.local().members(&conv.group_id)?;
            let m = members
                .iter()
                .find(|m| m.user_id == *uid)
                .ok_or_else(|| anyhow!("member not found"))?;
            let (t, kb) = member_menu(&conv, &m.display_name, &m.user_id, m.role);
            edit_or_send(bot, q, chat, thread, t, kb).await?;
            Ok(String::new())
        }
        ["ma", c, uid, what] => {
            let (rt, conv) = conv_for(app, tg_id, c.parse()?).await?;
            let toast = match *what {
                "a" => {
                    rt.set_member_role(&conv, uid, Role::Admin).await?;
                    "Promoted to admin"
                }
                "m" => {
                    rt.set_member_role(&conv, uid, Role::Member).await?;
                    "Now a member"
                }
                "r" => {
                    rt.remove_member(&conv, uid).await?;
                    "Removed"
                }
                _ => "",
            };
            let (t, kb) = members_view(&rt, &conv).await?;
            edit_or_send(bot, q, chat, thread, t, kb).await?;
            Ok(toast.into())
        }
        _ => Ok(String::new()),
    }
}

async fn reply(bot: &BridgeBot, msg: &Message, text: impl Into<String>) -> Result<()> {
    let mut r = bot.send_message(msg.chat.id, text).parse_mode(HTML);
    if let Some(t) = msg.thread_id {
        r = r.message_thread_id(t);
    }
    r.await?;
    Ok(())
}

pub async fn on_rename(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
    conv_id: i64,
) -> Result<()> {
    let tg = msg.from.as_ref().ok_or_else(|| anyhow!("no sender"))?;
    let (rt, conv) = conv_for(&app, tg.id.0 as i64, conv_id).await?;
    match validate_group_name(msg.text().unwrap_or_default()) {
        TextValidation::Valid { text } => {
            dialogue.exit().await?;
            match rt.rename_group(&conv, &text).await {
                Ok(()) => {
                    reply(
                        &bot,
                        &msg,
                        format!("Renamed to <b>{}</b>.", escape_html(&text)),
                    )
                    .await
                }
                Err(e) => {
                    reply(
                        &bot,
                        &msg,
                        format!("Could not rename: {}", escape_html(&e.to_string())),
                    )
                    .await
                }
            }
        }
        TextValidation::Empty => reply(&bot, &msg, "Send the new name, or /cancel.").await,
        TextValidation::TooLong { count, max } => {
            reply(
                &bot,
                &msg,
                format!("Too long ({count} characters, max {max})."),
            )
            .await
        }
    }
}

pub async fn on_add_phones(
    bot: BridgeBot,
    msg: Message,
    dialogue: Dialog,
    app: Arc<App>,
    conv_id: i64,
) -> Result<()> {
    let tg = msg.from.as_ref().ok_or_else(|| anyhow!("no sender"))?;
    let (rt, conv) = conv_for(&app, tg.id.0 as i64, conv_id).await?;
    let (phones, bad) = parse_phone_list(
        msg.text().unwrap_or_default(),
        &app.shared.cfg.default_region,
    );
    if phones.is_empty() {
        return reply(
            &bot,
            &msg,
            "Send phone numbers like <code>212-736-5000, +972 50 123 4567</code>, or /cancel.",
        )
        .await;
    }
    dialogue.exit().await?;
    let mut report = String::new();
    if !bad.is_empty() {
        report.push_str(&format!(
            "Skipping (not valid numbers): {}\n",
            escape_html(&bad.join(", "))
        ));
    }
    let (out, failed) = super::handlers::add_members_carefully(
        &rt,
        &conv.group_id,
        &phones,
        &app.shared.cfg.default_region,
    )
    .await;
    report.push_str(&super::handlers::format_add_outcome(&out));
    report.push_str(&super::handlers::format_add_failures(&failed));
    rt.sync_refresh_members(&conv.group_id).await.ok();
    reply(&bot, &msg, report).await
}

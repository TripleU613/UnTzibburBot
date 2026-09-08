//! Telegram side: commands, the OTP login dialogue, topic-message routing,
//! inline-keyboard callbacks, Telegram Stars donations, bot profile setup.

pub mod groups;
pub mod handlers;

use crate::app::App;
use crate::bridge::BridgeBot;
use anyhow::Result;
use std::sync::Arc;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::{HandlerExt, UpdateHandler};
use teloxide::prelude::*;
use teloxide::types::{BotCommand, MenuButton, WebAppInfo};
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Tzibbur ↔ Telegram bridge")]
pub enum Command {
    #[command(description = "welcome & status")]
    Start,
    #[command(description = "connect your Tzibbur account (phone + SMS code)")]
    Connect,
    #[command(description = "reconnect after your session expired")]
    Reconnect,
    #[command(description = "list your groups and unread counts")]
    Chats,
    #[command(description = "create a new Tzibbur group")]
    NewGroup,
    #[command(description = "group card: info, members, permissions, rename, leave, delete")]
    Group,
    #[command(description = "add members to the group of the current topic (phone numbers)")]
    Add(String),
    #[command(description = "rename the group of the current topic")]
    Rename(String),
    #[command(description = "remove a member / change roles (pick from a list)")]
    Manage,
    #[command(description = "delete the group of the current topic (admins, two-step)")]
    DeleteGroup,
    #[command(description = "mark the current group read on Tzibbur")]
    Read,
    #[command(description = "check which phone numbers are on Tzibbur")]
    Contacts(String),
    #[command(description = "list your Tzibbur devices")]
    Devices,
    #[command(description = "show members of the group of the current topic")]
    Members,
    #[command(description = "leave the group of the current topic")]
    Leave,
    #[command(description = "mute/unmute the group of the current topic")]
    Mute,
    #[command(description = "change your Tzibbur display name")]
    Name(String),
    #[command(description = "bridge settings")]
    Settings,
    #[command(description = "connection status")]
    Status,
    #[command(description = "force a sync now")]
    Sync,
    #[command(description = "disconnect and remove your session")]
    Disconnect,
    #[command(description = "support the bridge with Telegram Stars")]
    Donate,
    #[command(description = "Tzibbur terms & privacy policy")]
    Legal,
    #[command(description = "what this bridge stores and who can see what")]
    Privacy,
    #[command(description = "cancel the current action")]
    Cancel,
    #[command(description = "this help")]
    Help,
}

/// Dialogue state for multi-step flows (kept in memory; a restart resets it).
#[derive(Clone, Default, Debug)]
pub enum State {
    #[default]
    Idle,
    /// /connect: waiting for the phone number.
    AwaitPhone,
    /// Waiting for the display name (or "skip").
    AwaitName { phone: String },
    /// Waiting for the 6-digit SMS code.
    AwaitCode {
        challenge_id: String,
        phone: String,
        display_name: Option<String>,
        failures: u32,
    },
    /// /newgroup: waiting for the name.
    AwaitGroupName,
    /// /newgroup: category chosen via inline keyboard.
    AwaitGroupCategory { name: String },
    /// /newgroup: waiting for member phone numbers.
    AwaitGroupPhones { name: String, category: String },
    /// Group card → Rename: waiting for the new name.
    AwaitRename { conv_id: i64 },
    /// Group card → Add members: waiting for phone numbers.
    AwaitAddPhones { conv_id: i64 },
}

pub type Storage = InMemStorage<State>;
pub type Dialog = Dialogue<State, Storage>;

/// The dptree handler tree.
pub fn schema() -> UpdateHandler<anyhow::Error> {
    use dptree::case;
    let messages = Update::filter_message()
        .enter_dialogue::<Message, Storage, State>()
        // Commands always win, even mid-dialogue.
        .branch(
            dptree::entry()
                .filter_command::<Command>()
                .endpoint(handlers::on_command),
        )
        .branch(
            dptree::filter(|m: Message| m.successful_payment().is_some())
                .endpoint(handlers::on_successful_payment),
        )
        .branch(case![State::AwaitPhone].endpoint(handlers::on_phone))
        .branch(case![State::AwaitName { phone }].endpoint(handlers::on_name))
        .branch(
            case![State::AwaitCode {
                challenge_id,
                phone,
                display_name,
                failures
            }]
            .endpoint(handlers::on_code),
        )
        .branch(case![State::AwaitGroupName].endpoint(handlers::on_group_name))
        .branch(
            case![State::AwaitGroupPhones { name, category }].endpoint(handlers::on_group_phones),
        )
        .branch(case![State::AwaitRename { conv_id }].endpoint(groups::on_rename))
        .branch(case![State::AwaitAddPhones { conv_id }].endpoint(groups::on_add_phones))
        .branch(dptree::endpoint(handlers::on_message));
    let edited = Update::filter_edited_message().endpoint(handlers::on_edited);
    let callbacks = Update::filter_callback_query().endpoint(handlers::on_callback);
    let pre_checkout = Update::filter_pre_checkout_query().endpoint(handlers::on_pre_checkout);
    dptree::entry()
        .branch(messages)
        .branch(callbacks)
        .branch(pre_checkout)
}

/// Register commands, description, and the menu button.
pub async fn setup_bot_profile(bot: &BridgeBot, app: &App) -> Result<()> {
    let cmds: Vec<BotCommand> = Command::bot_commands();
    bot.set_my_commands(cmds).await?;
    bot.set_my_short_description()
        .short_description("Your Tzibbur groups as Telegram topics. Read and reply from Telegram.")
        .await
        .ok();
    bot.set_my_description()
        .description(
            "This bot mirrors your Tzibbur groups into topics in this chat, so you can read and reply from Telegram.\n\nTap Start, then /connect with your phone number. Tzibbur stays the source of truth; the bridge stores only ids and an encrypted session.",
        )
        .await
        .ok();
    let menu = match &app.shared.cfg.public_url {
        Some(u) => {
            let mut url = u.clone();
            url.set_path(&format!("{}/app", u.path().trim_end_matches('/')));
            MenuButton::WebApp {
                text: "Connect".into(),
                web_app: WebAppInfo { url },
            }
        }
        None => MenuButton::Commands,
    };
    bot.set_chat_menu_button().menu_button(menu).await.ok();
    Ok(())
}

pub fn deps(app: Arc<App>) -> dptree::di::DependencyMap {
    dptree::deps![app, Storage::new()]
}

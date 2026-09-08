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
#[command(rename_rule = "lowercase", description = "Commands")]
pub enum Command {
    #[command(description = "Start")]
    Start,
    #[command(description = "Connect your Tzibbur account")]
    Connect,
    #[command(description = "Sign in again after your session expired")]
    Reconnect,
    #[command(description = "List your groups")]
    Chats,
    #[command(description = "Create a Tzibbur group")]
    NewGroup,
    #[command(description = "Manage the group of this topic")]
    Group,
    #[command(description = "Add members to this group by phone number")]
    Add(String),
    #[command(description = "Rename this group")]
    Rename(String),
    #[command(description = "Members: promote, demote, remove")]
    Manage,
    #[command(description = "Delete this group (admins)")]
    DeleteGroup,
    #[command(description = "Mark this group read")]
    Read,
    #[command(description = "Check which phone numbers are on Tzibbur")]
    Contacts(String),
    #[command(description = "List your Tzibbur devices")]
    Devices,
    #[command(description = "Show members of this group")]
    Members,
    #[command(description = "Leave this group")]
    Leave,
    #[command(description = "Mute or unmute this group")]
    Mute,
    #[command(description = "Change your Tzibbur display name")]
    Name(String),
    #[command(description = "Settings")]
    Settings,
    #[command(description = "Connection status")]
    Status,
    #[command(description = "Sync now")]
    Sync,
    #[command(description = "Disconnect and remove your session")]
    Disconnect,
    #[command(description = "Support the project with Telegram Stars")]
    Donate,
    #[command(description = "Tzibbur terms and privacy policy")]
    Legal,
    #[command(description = "What this bot stores")]
    Privacy,
    #[command(description = "Cancel the current action")]
    Cancel,
    #[command(description = "List commands")]
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
        .branch(edited)
        .branch(callbacks)
        .branch(pre_checkout)
}

/// Register commands, description, and the menu button.
pub async fn setup_bot_profile(bot: &BridgeBot, app: &App) -> Result<()> {
    let cmds: Vec<BotCommand> = Command::bot_commands();
    bot.set_my_commands(cmds).await?;
    bot.set_my_short_description()
        .short_description("A Telegram client for Tzibbur. Your groups as topics.")
        .await
        .ok();
    bot.set_my_description()
        .description(
            "A Telegram client for Tzibbur.\n\nEach group you belong to becomes a topic in this chat. Read and reply here.\n\nSend /connect to sign in with your phone number. Open source; message text is never stored.",
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

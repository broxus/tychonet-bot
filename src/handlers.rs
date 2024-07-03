use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::RequestError;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tracing::info;
use crate::commands::Command;
use crate::state::State;

pub async fn handle_command(bot: Bot, msg: Message, cmd: Command, state: Arc<State>) -> ResponseResult<()> {
    if state.authentication_enabled {
        let allowed_groups = &state.allowed_groups;
        if !allowed_groups.contains(&msg.chat.id.0) {
            bot.send_message(msg.chat.id, "Access denied.").await?;
            return Ok(());
        }
    }

    let response = match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?;
            return Ok(());
        }
        Command::GetChatId => {
            let chat_id = msg.chat.id;
            bot.send_message(chat_id, format!("Chat ID: {}", chat_id)).await?;
            return Ok(());
        }
        Command::Status => state.get_status().await,
        Command::Reset(commit) => {
            let chat_id = msg.chat.id;
            let commit = if commit.trim().is_empty() { "master".to_string() } else { commit };
            let progress_message = bot.send_message(chat_id, "Starting network reset...").await?;
            if let Err(err) = state.reset_network(bot.clone(), chat_id, progress_message.id, commit.clone()).await {
                bot.send_message(chat_id, "☠️ Network reset failed.").await?;
                tracing::error!("Network reset failed: {err}");
            } else {
                // Explicit error handling
                if let Err(err) = state.save_commit(commit).await.map_err(|e| {
                    tracing::error!("Failed to save commit: {e}");
                }) {
                    bot.send_message(chat_id, "Failed to save commit.").await?;
                }
            }
            return Ok(());
        }
        Command::GetCommit => {
            match state.get_saved_commit().await {
                Ok(commit) => {
                    bot.send_message(msg.chat.id, format!("Current deployed commit: {}", commit)).await?;
                }
                Err(err) => {
                    tracing::error!("Failed to get saved commit: {err}");
                    bot.send_message(msg.chat.id, "Failed to retrieve the saved commit.").await?;
                }
            }
            return Ok(());
        }
        Command::Give { address, amount } => {
            // TODO
            info!("{}{}", address, amount);
            return Ok(());
        }
        Command::Account { address } => state.get_account(&address).await,
        Command::GetParam { param } => state.get_param(param).await,
    };

    let reply_text = match response {
        Ok(reply) => reply.to_string(),
        Err(err) => {
            tracing::error!("request failed: {err:?}");
            format!("Failed to handle command:\n```\n{err}\n```")
        }
    };

    bot.send_message(msg.chat.id, reply_text)
        .reply_to_message_id(msg.id)
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

    Ok(())
}

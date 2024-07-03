use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::state::State;
use crate::util::SendMessageExt;

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<State>,
) -> ResponseResult<()> {
    if state.authentication_enabled {
        let allowed_groups = &state.allowed_groups;
        if !allowed_groups.contains(&msg.chat.id.0) {
            bot.send_message(msg.chat.id, "Access denied.")
                .reply_to(&msg)
                .await?;
            return Ok(());
        }
    }

    let response = match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .reply_to(&msg)
                .await?;
            return Ok(());
        }
        Command::GetChatId => {
            let chat_id = msg.chat.id;
            bot.send_message(chat_id, format!("Chat ID: {}", chat_id))
                .reply_to(&msg)
                .await?;
            return Ok(());
        }
        Command::Status => state.get_status().await,
        Command::Reset(commit) => {
            let commit = commit.trim();
            let commit = if commit.is_empty() { "master" } else { commit };

            match state.reset_network(bot.clone(), &msg, commit).await {
                Ok(()) => return Ok(()),
                Err(e) => Err(e),
            }
        }
        Command::GetCommit => Ok(state.get_saved_commit()),
        Command::Give { address, amount } => {
            // TODO
            tracing::info!("{}{}", address, amount);
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
        .reply_to(&msg)
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

    Ok(())
}

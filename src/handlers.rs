use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::state::State;
use crate::util::{SendMessageExt, WithLinkPreview};

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<State>,
) -> ResponseResult<()> {
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
            tokio::spawn(async move {
                let commit = commit.trim();
                let commit = if commit.is_empty() { "master" } else { commit };

                if let Err(e) = state.reset_network(bot.clone(), &msg, commit).await {
                    tracing::error!("request failed: {e:?}");

                    let reply = format!("Failed to handle reset:\n```\n{e}\n```");
                    _ = bot
                        .send_message(msg.chat.id, reply)
                        .reply_to(&msg)
                        .markdown()
                        .await;
                }
            });
            return Ok(());
        }
        Command::GetCommit => state.get_saved_commit(),
        Command::SetNodeConfig(expr) => state.set_node_config(&msg, &expr),
        Command::GetNodeConfig(expr) => state.get_node_config(&expr),
        Command::SetLoggerConfig(expr) => state.set_logger_config(&msg, &expr),
        Command::GetLoggerConfig(expr) => state.get_logger_config(&expr),
        Command::SetZeroState(expr) => state.set_zerostate(&msg, &expr),
        Command::GetZeroState(expr) => state.get_zerostate(&expr),
        Command::Give { address, amount } => {
            // TODO
            tracing::info!("{}{}", address, amount);
            return Ok(());
        }
        Command::Account { address } => state.get_account(&address).await,
        Command::GetParam { param } => state.get_param(param).await,
    };

    let mut link_preview_options = None;
    let reply_text = match response {
        Ok(reply) => {
            link_preview_options = reply.link_preview_options();
            reply.to_string()
        }
        Err(err) => {
            tracing::error!("request failed: {err:?}");
            format!("Failed to handle command:\n```\n{err}\n```")
        }
    };

    let req = WithLinkPreview {
        inner: teloxide::payloads::SendMessage::new(msg.chat.id, reply_text),
        link_preview_options,
    };

    teloxide::requests::JsonRequest::new(bot, req)
        .reply_to(&msg)
        .markdown()
        .await?;

    Ok(())
}

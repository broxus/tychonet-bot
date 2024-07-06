use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::state::{Reply, State};
use crate::util::SendMessageExt;

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

                if let Err(e) = state.reset_network(bot.clone(), &msg, &commit).await {
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
        Command::GetCommit => Ok(state.get_saved_commit()),
        Command::SetNodeConfig(expr) => {
            if state.check_auth(&msg) {
                state.set_node_config(&expr).await
            } else {
                Ok(Reply::AccessDenied)
            }
        }
        Command::GetNodeConfig(path) => state.get_node_config(&path).await,
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
        .markdown()
        .await?;

    Ok(())
}

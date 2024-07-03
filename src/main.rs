use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use teloxide::net;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::handlers::handle_command;
use crate::settings::load_settings;
use crate::state::State;

mod commands;
mod handlers;
mod jrpc_client;
mod settings;
mod state;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let settings = load_settings()?;

    let client = net::default_reqwest_settings().timeout(Duration::from_secs(600));
    let bot = Bot::with_client(&settings.bot_token, client.build().unwrap());

    tracing::info!("updating menu button");
    bot.set_my_commands(Command::bot_commands()).await?;
    tracing::info!("updated menu button");

    tracing::info!("bot started");

    let state = Arc::new(State::new(&settings)?);

    Command::repl(bot, move |bot, msg, cmd| {
        handle_command(bot, msg, cmd, state.clone())
    })
    .await;

    Ok(())
}

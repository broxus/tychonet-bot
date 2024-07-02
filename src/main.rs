use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use everscale_types::models::{AccountState, AccountStatus, StdAddr};
use everscale_types::num::Tokens;
use serde::Deserialize;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;

use self::jrpc_client::{JrpcClient, StateTimings};

mod jrpc_client;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let settings = config::Config::builder()
        .add_source(config::File::with_name("config.toml"))
        .add_source(config::Environment::with_prefix("TYCHONET"))
        .build()?
        .try_deserialize::<Settings>()?;

    let bot = Bot::new(settings.bot_token);

    {
        tracing::info!("updating menu button");
        bot.set_my_commands(Command::bot_commands()).await?;
        tracing::info!("updated menu button");
    }

    tracing::info!("bot started");

    let state = Arc::new(State {
        client: JrpcClient::new(settings.rpc_url)?,
    });
    Command::repl(bot, move |bot, msg, cmd| {
        answer(bot, msg, cmd, state.clone())
    })
    .await;
    Ok(())
}

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "display this text.")]
    Start,
    #[command(description = "get network status")]
    Status,
    #[command(description = "reset network with the given version.")]
    Reset { version: String },
    #[command(
        description = "give some tokens to the specified address.",
        parse_with = "split"
    )]
    Give {
        address: StdAddr,
        amount: DecimalTokens,
    },
    #[command(description = "get an account state of the specified address.")]
    Account { address: StdAddr },
    #[command(description = "get the blockchain config param.")]
    GetParam { param: i32 },
}

async fn answer(bot: Bot, msg: Message, cmd: Command, state: Arc<State>) -> ResponseResult<()> {
    let reply = match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await?;
            return Ok(());
        }
        Command::Status => state.get_status().await,
        Command::Reset { version } => {
            // TODO
            return Ok(());
        }
        Command::Give { address, amount } => {
            // TODO
            return Ok(());
        }
        Command::Account { address } => state.get_account(&address).await,
        Command::GetParam { param } => state.get_param(param).await,
    };

    let text = match reply {
        Ok(reply) => reply.to_string(),
        Err(e) => {
            tracing::error!("request failed: {e:?}");
            format!("Failed to handle command:\n```\n{e}\n```")
        }
    };

    bot.send_message(msg.chat.id, text)
        .reply_to_message_id(msg.id)
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

    Ok(())
}

struct State {
    client: JrpcClient,
}

impl State {
    async fn get_status(&self) -> Result<Reply> {
        self.client.get_timings().await.map(Reply::Timings)
    }

    async fn get_account(&self, address: &StdAddr) -> Result<Reply> {
        let res = self.client.get_account(address).await?;
        Ok(match res {
            jrpc_client::AccountStateResponse::NotExists { .. } => Reply::Account {
                address: address.clone(),
                balance: Default::default(),
                status: AccountStatus::NotExists,
            },
            jrpc_client::AccountStateResponse::Exists { account, .. } => Reply::Account {
                address: address.clone(),
                balance: DecimalTokens(account.balance.tokens),
                status: match account.state {
                    AccountState::Uninit => AccountStatus::Uninit,
                    AccountState::Active { .. } => AccountStatus::Active,
                    AccountState::Frozen { .. } => AccountStatus::Frozen,
                },
            },
            jrpc_client::AccountStateResponse::Unchanged { .. } => {
                anyhow::bail!("unexpected response")
            }
        })
    }

    async fn get_param(&self, param: i32) -> Result<Reply> {
        let res = self.client.get_config().await?;
        let value = serde_json::to_value(res.config.params)?;

        Ok(Reply::ConfigParam {
            global_id: res.global_id,
            seqno: res.seqno,
            value,
            param,
        })
    }
}

enum Reply {
    Timings(StateTimings),
    Account {
        address: StdAddr,
        balance: DecimalTokens,
        status: AccountStatus,
    },
    ConfigParam {
        global_id: i32,
        seqno: u32,
        value: serde_json::Value,
        param: i32,
    },
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Timings(timings) => {
                let reply_data = serde_json::to_string_pretty(&timings).unwrap();
                write!(f, "Timings:\n```json\n{reply_data}\n```")
            }
            Reply::Account {
                address,
                balance,
                status,
            } => {
                write!(
                    f,
                    "Address:\n`{address}`\n\
                    Status:\n`{status:?}`\n\
                    Balance:\n{balance} {Currency}",
                )
            }
            Reply::ConfigParam {
                global_id,
                seqno,
                value,
                param,
            } => {
                let value = serde_json::to_string_pretty(&value.get(param.to_string())).unwrap();

                write!(
                    f,
                    "Global ID: {global_id}\n\
                    Key Block Seqno: {seqno}\n\n\
                    Param {param}:\n\
                    ```json\n{value}\n```",
                )
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct Settings {
    bot_token: String,
    rpc_url: String,
}

#[derive(Debug, Default, Clone)]
struct DecimalTokens(Tokens);

impl std::fmt::Display for DecimalTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use num_format::ToFormattedString;

        let t = self.0.into_inner();
        let int = t / 1000000000;
        let frac = t % 1000000000;

        int.read_to_fmt_writer(&mut *f, &num_format::Locale::fr)?;
        if frac > 0 {
            f.write_fmt(format_args!(
                "\\.{}",
                format!("{:09}", frac).trim_end_matches('0')
            ))?;
        }

        Ok(())
    }
}

impl FromStr for DecimalTokens {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (number, _) = bigdecimal::BigDecimal::from_str(s)?
            .with_scale(9)
            .into_bigint_and_exponent();

        // TEMP
        number
            .to_string()
            .parse::<Tokens>()
            .map(Self)
            .map_err(Into::into)
    }
}

struct Currency;

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("🌭")
    }
}

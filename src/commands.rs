use everscale_types::models::StdAddr;
use everscale_types::num::Tokens;
use std::str::FromStr;
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
pub enum Command {
    #[command(description = "display this text")]
    Start,
    #[command(description = "get chat ID.")]
    GetChatId,
    #[command(description = "get network status.")]
    Status,
    #[command(description = "freeze network reset for some time.")]
    Freeze(String),
    #[command(description = "unfreeze network reset.")]
    Unfreeze,
    #[command(description = "reset network with the commit hash or branch name.")]
    Reset(String),
    #[command(description = "retrieve the current deployed commit.")]
    GetCommit,
    #[command(description = "get workspace")]
    GetWorkspace,
    #[command(description = "set current workspace")]
    SetWorkspace(String),
    #[command(description = "remove the specified workspace")]
    DelWorkspace(String),
    #[command(description = "get network inventory")]
    GetNetwork,
    #[command(description = "set current network inventory")]
    SetNetwork(String),
    #[command(description = "use one of reset types: `full` or `restart`")]
    SetResetType(String),
    #[command(description = "get current reset type.")]
    GetResetType,
    #[command(description = "set node config.")]
    SetNodeConfig(String),
    #[command(description = "get node config.")]
    GetNodeConfig(String),
    #[command(description = "set logger config.")]
    SetLoggerConfig(String),
    #[command(description = "get logger config.")]
    GetLoggerConfig(String),
    #[command(description = "set zero state.")]
    SetZeroState(String),
    #[command(description = "get zero state.")]
    GetZeroState(String),
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

#[derive(Debug, Default, Clone)]
pub struct DecimalTokens(pub Tokens);

impl std::fmt::Display for DecimalTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use num_format::ToFormattedString;
        let t = self.0.into_inner();
        let int = t / 1_000_000_000;
        let frac = t % 1_000_000_000;
        int.read_to_fmt_writer(&mut *f, &num_format::Locale::fr)?;
        if frac > 0 {
            write!(f, "\\.{}", format!("{:09}", frac).trim_end_matches('0'))?;
        }
        Ok(())
    }
}

impl FromStr for DecimalTokens {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (number, _) = bigdecimal::BigDecimal::from_str(s)?
            .with_scale(9)
            .into_bigint_and_exponent();
        number
            .to_string()
            .parse::<Tokens>()
            .map(Self)
            .map_err(Into::into)
    }
}

pub struct Currency;

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "🌭")
    }
}

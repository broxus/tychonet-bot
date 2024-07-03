use std::str::FromStr;

use anyhow::{Context, Result};
use dotenvy::dotenv;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub bot_token: String,
    pub rpc_url: String,
    pub inventory_file: String,
    pub reset_playbook: String,
    pub setup_playbook: String,
    pub allowed_groups: Vec<i64>,
    pub authentication_enabled: bool,
}

pub fn load_settings() -> Result<Settings> {
    dotenv().ok();

    Ok(Settings {
        bot_token: get_env("BOT_TOKEN")?,
        rpc_url: get_env("RPC_URL")?,
        inventory_file: get_env("INVENTORY_FILE")?,
        reset_playbook: get_env("RESET_PLAYBOOK")?,
        setup_playbook: get_env("SETUP_PLAYBOOK")?,
        allowed_groups: get_env::<List<i64>>("ALLOWED_GROUPS")?.0,
        authentication_enabled: get_env("AUTHENTICATION_ENABLED")?,
    })
}

struct List<T>(Vec<T>);

impl<T: FromStr> FromStr for List<T> {
    type Err = T::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let list = s
            .trim()
            .trim_matches(|c| c == '[' || c == ']') // Remove surrounding brackets
            .split(',')
            .map(|s| s.trim().parse::<T>())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(list))
    }
}

fn get_env<T: FromStr<Err: Into<anyhow::Error>>>(name: &str) -> Result<T> {
    let key = format!("{PREFIX}_{name}");
    let value = std::env::var(&key).with_context(|| format!("{key} not set in .env"))?;
    value
        .parse()
        .map_err(Into::into)
        .with_context(|| format!("Failed to parse {key}"))
}

const PREFIX: &str = "TYCHONET";

use std::env;
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

    let allowed_groups = env::var("TYCHONET_ALLOWED_GROUPS")
        .context("TYCHONET_ALLOWED_GROUPS not set in .env")?
        .trim_matches(|c| c == '[' || c == ']') // Remove surrounding brackets
        .split(',')
        .map(|s| s.trim().parse::<i64>())
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse TYCHONET_ALLOWED_GROUPS")?;

    let authentication_enabled = env::var("TYCHONET_AUTHENTICATION_ENABLED")
        .context("TYCHONET_AUTHENTICATION_ENABLED not set in .env")?
        .parse::<bool>()
        .context("Failed to parse TYCHONET_AUTHENTICATION_ENABLED as boolean")?;

    let settings = Settings {
        bot_token: env::var("TYCHONET_BOT_TOKEN").context("TYCHONET_BOT_TOKEN not set in .env")?,
        rpc_url: env::var("TYCHONET_RPC_URL").context("TYCHONET_RPC_URL not set in .env")?,
        inventory_file: env::var("TYCHONET_INVENTORY_FILE").context("TYCHONET_INVENTORY_FILE not set in .env")?,
        reset_playbook: env::var("TYCHONET_RESET_PLAYBOOK").context("TYCHONET_RESET_PLAYBOOK not set in .env")?,
        setup_playbook: env::var("TYCHONET_SETUP_PLAYBOOK").context("TYCHONET_SETUP_PLAYBOOK not set in .env")?,
        allowed_groups,
        authentication_enabled,
    };

    Ok(settings)
}

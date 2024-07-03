use std::process::Command as ProcessCommand;
use std::sync::Mutex;
use anyhow::{Context, Result};
use everscale_types::models::{AccountState, AccountStatus, StdAddr};
use serde_json::Value;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};
use tracing::info;
use crate::commands::{Currency, DecimalTokens};
use crate::jrpc_client;
use crate::jrpc_client::{JrpcClient, StateTimings};
use crate::settings::Settings;

pub struct State {
    client: JrpcClient,
    inventory_file: String,
    reset_playbook: String,
    setup_playbook: String,
    pub(crate) allowed_groups: Vec<i64>,
    pub(crate) authentication_enabled: bool,
    saved_commit: Mutex<String>,
}

impl State {
    pub fn new(settings: &Settings) -> Result<Self> {
        Ok(Self {
            client: JrpcClient::new(&settings.rpc_url)?,
            inventory_file: settings.inventory_file.clone(),
            reset_playbook: settings.reset_playbook.clone(),
            setup_playbook: settings.setup_playbook.clone(),
            allowed_groups: settings.allowed_groups.clone(),
            authentication_enabled: settings.authentication_enabled.clone(),
            saved_commit: Mutex::new("".to_string()),
        })
    }

    pub async fn get_status(&self) -> Result<Reply> {
        self.client.get_timings().await.map(Reply::Timings).context("Failed to get status")
    }

    pub async fn get_account(&self, address: &StdAddr) -> Result<Reply> {
        let res = self.client.get_account(address).await?;
        let reply = match res {
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
            jrpc_client::AccountStateResponse::Unchanged { .. } => anyhow::bail!("Unexpected response"),
        };
        Ok(reply)
    }

    pub async fn get_param(&self, param: i32) -> Result<Reply> {
        let res = self.client.get_config().await?;
        let value = serde_json::to_value(res.config.params)?;

        Ok(Reply::ConfigParam {
            global_id: res.global_id,
            seqno: res.seqno,
            value,
            param,
        })
    }

    pub async fn reset_network(&self, bot: Bot, chat_id: ChatId, progress_message_id: MessageId, commit: String) -> Result<()> {
        bot.edit_message_text(chat_id, progress_message_id, "🔄 Updating gate...").await?;
        let gate_update_output = ProcessCommand::new("sh")
            .arg("-c")
            .arg("gate update") // Replace with actual gate update command
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .context("Failed to execute gate update command")?;
        if !gate_update_output.status.success() {
            let error_message = String::from_utf8_lossy(&gate_update_output.stderr).to_string();
            tracing::error!("Gate update failed: {error_message}");
            bot.edit_message_text(chat_id, progress_message_id, "Gate update failed.").await?;
            return Err(anyhow::anyhow!("Gate update failed"));
        }

        bot.edit_message_text(chat_id, progress_message_id, "🔄 Gate updated. Running reset playbook...").await?;
        let reset_output = ProcessCommand::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.reset_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={}", commit))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .context("Failed to execute reset playbook")?;
        if !reset_output.status.success() {
            let error_message = String::from_utf8_lossy(&reset_output.stderr).to_string();
            tracing::error!("Reset playbook execution failed: {error_message}");
            bot.edit_message_text(chat_id, progress_message_id, "Reset playbook execution failed.").await?;
            return Err(anyhow::anyhow!("Reset playbook execution failed"));
        }

        bot.edit_message_text(chat_id, progress_message_id, "🔄 Reset completed. Running setup playbook...").await?;
        let setup_output = ProcessCommand::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.setup_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={}", commit))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .context("Failed to execute setup playbook")?;
        if !setup_output.status.success() {
            let error_message = String::from_utf8_lossy(&setup_output.stderr).to_string();
            tracing::error!("Setup playbook execution failed: {error_message}");
            bot.edit_message_text(chat_id, progress_message_id, "Setup playbook execution failed.").await?;
            return Err(anyhow::anyhow!("Setup playbook execution failed"));
        }

        bot.edit_message_text(chat_id, progress_message_id, format!("✅ Network reset completed successfully with commit: {}.", commit)).await?;
        Ok(())
    }

    pub async fn save_commit(&self, commit: String) -> Result<()> {
        let mut saved_commit = self.saved_commit.lock().unwrap();
        *saved_commit = commit;
        Ok(())
    }

    pub async fn get_saved_commit(&self) -> Result<String> {
        let saved_commit = self.saved_commit.lock().unwrap();
        Ok(saved_commit.clone())
    }
}

pub enum Reply {
    Timings(StateTimings),
    Account {
        address: StdAddr,
        balance: DecimalTokens,
        status: AccountStatus,
    },
    ConfigParam {
        global_id: i32,
        seqno: u32,
        value: Value,
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
            Reply::Account { address, balance, status } => {
                write!(
                    f,
                    "Address:\n`{address}`\nStatus:\n`{status:?}`\nBalance:\n{balance} {Currency}"
                )
            }
            Reply::ConfigParam { global_id, seqno, value, param } => {
                let value_str = serde_json::to_string_pretty(&value.get(param.to_string())).unwrap_or_default();
                write!(
                    f,
                    "Global ID: {global_id}\nKey Block Seqno: {seqno}\n\nParam {param}:\n```json\n{value_str}\n```"
                )
            }
        }
    }
}

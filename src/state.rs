use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use everscale_types::models::{AccountState, AccountStatus, StdAddr};
use serde_json::Value;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};

use crate::commands::{Currency, DecimalTokens};
use crate::config::Config;
use crate::jrpc_client;
use crate::jrpc_client::{JrpcClient, StateTimings};
use crate::settings::Settings;
use crate::util::SendMessageExt;

pub struct State {
    client: JrpcClient,
    inventory_file: String,
    tycho_config_file: String,
    reset_playbook: String,
    setup_playbook: String,
    allowed_groups: HashSet<i64>,
    authentication_enabled: bool,
    saved_commit: Mutex<String>,
    reset_running: AtomicBool,
}

impl State {
    pub fn new(settings: &Settings) -> Result<Self> {
        Ok(Self {
            client: JrpcClient::new(&settings.rpc_url)?,
            inventory_file: settings.inventory_file.clone(),
            tycho_config_file: settings.tycho_config_file.clone(),
            reset_playbook: settings.reset_playbook.clone(),
            setup_playbook: settings.setup_playbook.clone(),
            allowed_groups: settings.allowed_groups.iter().copied().collect(),
            authentication_enabled: settings.authentication_enabled.clone(),
            saved_commit: Mutex::new("".to_string()),
            reset_running: AtomicBool::new(false),
        })
    }

    pub async fn get_status(&self) -> Result<Reply> {
        self.client
            .get_timings()
            .await
            .map(Reply::Timings)
            .context("Failed to get status")
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
            jrpc_client::AccountStateResponse::Unchanged { .. } => {
                anyhow::bail!("Unexpected response")
            }
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

    pub fn get_saved_commit(&self) -> Reply {
        Reply::Commit(self.saved_commit.lock().unwrap().clone())
    }

    pub async fn reset_network(&self, bot: Bot, msg: &Message, commit: &str) -> Result<()> {
        struct ResetGuard<'a>(&'a AtomicBool);

        impl Drop for ResetGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }

        if !self.check_auth(msg) {
            bot.send_message(msg.chat.id, Reply::AccessDenied.to_string())
                .reply_to(&msg)
                .await?;
            return Ok(());
        }

        let _guard = {
            if self.reset_running.swap(true, Ordering::Relaxed) {
                bot.send_message(msg.chat.id, "Reset is already running")
                    .reply_to(&msg)
                    .await?;
                return Ok(());
            }

            ResetGuard(&self.reset_running)
        };

        let r = LongReply::begin(bot, msg, "Starting network reset...").await?;

        r.update("🔄 Updating gate...").await?;

        let gate_update_output = self.run_gate_update().await?;
        if !gate_update_output.status.success() {
            let e = String::from_utf8_lossy(&gate_update_output.stderr).to_string();
            tracing::error!("Gate update failed: {e}");
            r.update(format!("Gate update failed:\n```\n{e}\n```"))
                .await?;
            return Ok(());
        }

        r.update("🔄 Gate updated. Running reset playbook...")
            .await?;

        let reset_output = self.run_ansible_reset(commit).await?;
        if !reset_output.status.success() {
            let e = String::from_utf8_lossy(&reset_output.stderr).to_string();
            tracing::error!("Reset playbook execution failed: {e}");
            r.update(format!("Reset playbook execution failed:\n```\n{e}\n```"))
                .await?;
            return Ok(());
        }

        r.update("🔄 Reset completed. Running setup playbook...")
            .await?;

        let setup_output = self.run_ansible_setup(commit).await?;
        if !setup_output.status.success() {
            let e = String::from_utf8_lossy(&setup_output.stderr).to_string();
            tracing::error!("Setup playbook execution failed: {e}");
            r.update(format!("Setup playbook execution failed:\n```\n{e}\n```"))
                .await?;
            return Ok(());
        }

        *self.saved_commit.lock().unwrap() = commit.to_owned();

        r.update(format!(
            "✅ Network reset completed successfully with commit:\n`{commit}`",
        ))
        .await?;
        Ok(())
    }

    async fn run_gate_update(&self) -> Result<std::process::Output> {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg("gate update") // Replace with actual gate update command
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .await
            .context("Failed to execute gate update command")
    }

    // TODO: Remove `commit` ?
    async fn run_ansible_reset(&self, commit: &str) -> Result<std::process::Output> {
        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.reset_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={}", commit))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .await
            .context("Failed to execute reset playbook")
    }

    async fn run_ansible_setup(&self, commit: &str) -> Result<std::process::Output> {
        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.setup_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={}", commit))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .await
            .context("Failed to execute setup playbook")
    }

    pub async fn set_node_config(&self, config: &str) -> Result<Reply> {
        let mut current_config = Config::from_file(&self.tycho_config_file)?;

        let mut errors = Vec::new();

        for line in config.lines() {
            if line.trim().is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() != 2 {
                errors.push(format!("Invalid config line: {}", line));
                continue;
            }
            let key = parts[0].trim();
            let value = parts[1].trim();

            if let Err(e) = current_config.update(key, value) {
                errors.push(format!("Failed to update config: {}: {}", key, e));
            }
        }

        current_config.to_file(&self.tycho_config_file)?;

        Ok(())
    }

    pub async fn get_node_config(&self, path: &str) -> Result<Reply> {
        let path = parse_config_value_path(path)?;
        let mut config = Config::from_file(&self.tycho_config_file)?;
    }

    pub fn check_auth(&self, msg: &Message) -> bool {
        !self.authentication_enabled || self.allowed_groups.contains(&msg.chat.id.0)
    }
}

fn parse_config_value_path(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    let s = s.strip_prefix('.').unwrap_or(s);
    s.split('.')
        .map(|item| {
            let item = item.trim();
            anyhow::ensure!(!item.is_empty(), "empty path items are not allowed");
            Ok(item.to_owned())
        })
        .collect()
}

struct LongReply {
    bot: Bot,
    chat_id: ChatId,
    reply_msg_id: MessageId,
}

impl LongReply {
    async fn begin(bot: Bot, msg: &Message, text: impl Into<String>) -> Result<Self> {
        let chat_id = msg.chat.id;
        let reply = bot
            .send_message(chat_id, text)
            .reply_to(&msg)
            .markdown()
            .await?;
        Ok(Self {
            bot,
            chat_id,
            reply_msg_id: reply.id,
        })
    }

    async fn update(&self, text: impl Into<String>) -> Result<()> {
        self.bot
            .edit_message_text(self.chat_id, self.reply_msg_id, text)
            .markdown()
            .await?;
        Ok(())
    }
}

pub enum Reply {
    Timings(StateTimings),
    Commit(String),
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
    Message(String),
    ConfigUpdated(String),
    Config(String),
    AccessDenied,
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timings(timings) => {
                let reply_data = serde_json::to_string_pretty(&timings).unwrap();
                write!(f, "Timings:\n```json\n{reply_data}\n```")
            }
            Self::Commit(commit) => {
                write!(f, "Current deployed commit:\n`{commit}`")
            }
            Self::Account {
                address,
                balance,
                status,
            } => {
                write!(
                    f,
                    "Address:\n`{address}`\nStatus:\n`{status:?}`\nBalance:\n{balance} {Currency}"
                )
            }
            Self::ConfigParam {
                global_id,
                seqno,
                value,
                param,
            } => {
                let value_str =
                    serde_json::to_string_pretty(&value.get(param.to_string())).unwrap_or_default();
                write!(
                    f,
                    "Global ID: {global_id}\nKey Block Seqno: {seqno}\n\nParam {param}:\n```json\n{value_str}\n```"
                )
            }
            Self::Message(msg) => {
                write!(f, "{msg}")
            }
            Self::ConfigUpdated(msg) => {
                write!(f, "Node config updated:\n{msg}")
            }
            Self::Config(config) => {
                write!(f, "```json\n{config}\n```")
            }
            Self::AccessDenied => {
                write!(f, "👮‍♀️ Access denied")
            }
        }
    }
}

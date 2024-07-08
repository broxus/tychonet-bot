use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use everscale_types::models::{AccountState, AccountStatus, StdAddr};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};

use crate::commands::{Currency, DecimalTokens};
use crate::config::{Config, ConfigDiff};
use crate::github_client::GithubClient;
use crate::jrpc_client;
use crate::jrpc_client::{JrpcClient, StateTimings};
use crate::settings::Settings;
use crate::util::SendMessageExt;

const DEFAULT_BRANCH: &str = "master";

pub struct State {
    jrpc_client: JrpcClient,
    github_client: GithubClient,
    inventory_file: String,
    tycho_config_file: String,
    reset_playbook: String,
    setup_playbook: String,
    allowed_groups: HashSet<i64>,
    authentication_enabled: bool,
    state_file: Mutex<StateFile>,
    reset_running: AtomicBool,
}

impl State {
    pub async fn new(settings: &Settings) -> Result<Self> {
        let github_client = GithubClient::new(&settings.github_token, "broxus", "tycho")?;

        let mut state_file = StateFile::load(&settings.state_file)?;
        if state_file.latest_data.last_commit_info.is_none() {
            let latest_commit = github_client.get_commit_sha(DEFAULT_BRANCH).await?;
            let commit_info = github_client.get_commit_info(&latest_commit).await?;
            state_file.latest_data.last_commit_info = Some(CommitInfo {
                sha: latest_commit,
                html_url: commit_info.html_url,
                message: commit_info.message,
                branches: vec![DEFAULT_BRANCH.to_owned()],
            });
            state_file.save()?;
        }

        Ok(Self {
            jrpc_client: JrpcClient::new(&settings.rpc_url)?,
            github_client,
            inventory_file: settings.inventory_file.clone(),
            tycho_config_file: settings.tycho_config_file.clone(),
            reset_playbook: settings.reset_playbook.clone(),
            setup_playbook: settings.setup_playbook.clone(),
            allowed_groups: settings.allowed_groups.iter().copied().collect(),
            authentication_enabled: settings.authentication_enabled,
            state_file: Mutex::new(state_file),
            reset_running: AtomicBool::new(false),
        })
    }

    pub async fn get_status(&self) -> Result<Reply> {
        self.jrpc_client
            .get_timings()
            .await
            .map(Reply::Timings)
            .context("Failed to get status")
    }

    pub async fn get_account(&self, address: &StdAddr) -> Result<Reply> {
        let res = self.jrpc_client.get_account(address).await?;
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
        let res = self.jrpc_client.get_config().await?;
        let value = serde_json::to_value(res.config.params)?;

        Ok(Reply::ConfigParam {
            global_id: res.global_id,
            seqno: res.seqno,
            value,
            param,
        })
    }

    pub fn get_saved_commit(&self) -> Result<Reply> {
        let state_file = self.state_file.lock().unwrap();
        state_file
            .latest_data
            .last_commit_info
            .clone()
            .map(Reply::Commit)
            .context("no commit info saved")
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
                .reply_to(msg)
                .await?;
            return Ok(());
        }

        let _guard = {
            if self.reset_running.swap(true, Ordering::Relaxed) {
                bot.send_message(msg.chat.id, "Reset is already running")
                    .reply_to(msg)
                    .await?;
                return Ok(());
            }

            ResetGuard(&self.reset_running)
        };

        let r = LongReply::begin(bot, msg, "Starting network reset...").await?;

        let commit_info = self.get_commit_info(commit).await?;

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

        struct SucessReply<'a>(&'a CommitInfo);

        impl std::fmt::Display for SucessReply<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "✅ Network reset completed successfully with commit:\n{}\n\n{}",
                    DisplayFullCommit(self.0),
                    self.0.html_url
                )
            }
        }

        let success_reply = SucessReply(&commit_info).to_string();

        {
            let mut state_file = self.state_file.lock().unwrap();
            state_file.latest_data.last_commit_info = Some(commit_info);
            state_file.save()?;
        }

        r.update(success_reply).await?;
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

    pub async fn set_node_config(&self, expr: &str) -> Result<Reply> {
        let mut config = Config::from_file(&self.tycho_config_file)?;

        let expr = expr.trim();
        match expr.strip_prefix("delete") {
            Some(path) => {
                let path = parse_config_value_path(path)?;
                anyhow::ensure!(!path.is_empty(), "cannot delete the config root");
                config.remove(&path)?;
            }
            None => {
                let (path, value) = expr
                    .split_once('=')
                    .context("expected an expression: (.path)+ = json")?;

                let path = parse_config_value_path(path)?;
                let value = serde_json::from_str(value)?;
                config.set(&path, value)?;
            }
        }

        config.save().map(Reply::NodeConfigUpdated)
    }

    pub async fn get_node_config(&self, path: &str) -> Result<Reply> {
        let path = parse_config_value_path(path)?;
        let config = Config::from_file(&self.tycho_config_file)?;
        let value = serde_json::to_string_pretty(config.get(&path)?)?;
        Ok(Reply::NodeConfigParam(value))
    }

    pub fn check_auth(&self, msg: &Message) -> bool {
        !self.authentication_enabled || self.allowed_groups.contains(&msg.chat.id.0)
    }

    async fn get_commit_info(&self, commit: &str) -> Result<CommitInfo> {
        let commit_sha = self.github_client.get_commit_sha(commit).await?;
        let commit_info = self.github_client.get_commit_info(&commit_sha).await?;
        let commit_branches = self.github_client.get_commit_branches(&commit_sha).await?;

        Ok(CommitInfo {
            sha: commit_sha,
            html_url: commit_info.html_url,
            message: commit_info.message,
            branches: commit_branches,
        })
    }
}

struct StateFile {
    path: PathBuf,
    latest_data: StateFileData,
}

impl StateFile {
    pub fn load(path: &str) -> Result<Self> {
        let path = Path::new(path);
        let latest_data = if path.exists() {
            let content = std::fs::read_to_string(path).context("failed to read state file")?;
            serde_json::from_str(&content).context("failed to parse state file")?
        } else {
            StateFileData::default()
        };

        Ok(Self {
            path: path.to_owned(),
            latest_data,
        })
    }

    pub fn save(&self) -> Result<()> {
        let content = serde_json::to_string_pretty(&self.latest_data)
            .context("failed to serialize state file")?;
        std::fs::write(&self.path, content).context("failed to write state file")
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
struct StateFileData {
    last_commit_info: Option<CommitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub html_url: String,
    pub message: String,
    pub branches: Vec<String>,
}

struct DisplayFullCommit<'a>(&'a CommitInfo);

impl std::fmt::Display for DisplayFullCommit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.sha)?;

        if !self.0.branches.is_empty() {
            let mut first = true;
            write!(f, " (branch: ")?;
            for name in &self.0.branches {
                write!(
                    f,
                    "{}`{name}`",
                    if std::mem::take(&mut first) { "" } else { ", " }
                )?;
            }
            write!(f, ")")?;
        }

        Ok(())
    }
}

fn parse_config_value_path(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }

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
            .reply_to(msg)
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
    Commit(CommitInfo),
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
    NodeConfigUpdated(ConfigDiff),
    NodeConfigParam(String),
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
                write!(
                    f,
                    "Current deployed commit:\n{}\n\n{}",
                    DisplayFullCommit(commit),
                    commit.html_url
                )
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
            Self::NodeConfigUpdated(msg) => {
                write!(f, "Node config updated:\n```json\n{msg}\n```")
            }
            Self::NodeConfigParam(config) => {
                write!(f, "```json\n{config}\n```")
            }
            Self::AccessDenied => {
                write!(f, "👮‍♀️ Access denied")
            }
        }
    }
}

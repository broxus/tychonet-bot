use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use everscale_types::models::{AccountState, AccountStatus, StdAddr};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use teloxide::prelude::*;
use teloxide::requests::{JsonRequest, MultipartRequest};
use teloxide::types::{ChatId, MessageId};
use tokio::task::AbortHandle;

use crate::commands::{Currency, DecimalTokens};
use crate::config::{Config, ConfigDiff};
use crate::github_client::GithubClient;
use crate::jrpc_client;
use crate::jrpc_client::{JrpcClient, StateTimings};
use crate::settings::Settings;
use crate::util::{
    now_sec, Emoji, LinkPreviewOptions, ReactionType, SendMessageExt, SetMessageReaction,
    WithLinkPreview, WithLinkPreviewSetters,
};

const DEFAULT_BRANCH: &str = "master";

pub struct State {
    jrpc_client: JrpcClient,
    github_client: GithubClient,
    inventory_file: String,
    ansible_config_file: String,
    node_config_file: String,
    logger_config_file: String,
    zerostate_file: String,
    reset_playbook: String,
    setup_playbook: String,
    allowed_groups: HashSet<i64>,
    authentication_enabled: bool,
    state_file: Mutex<StateFile>,
    reset_running: AtomicBool,
    unfreeze_notify: Mutex<Option<AbortHandle>>,
}

impl State {
    pub async fn new(bot: Bot, settings: &Settings) -> Result<Arc<Self>> {
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

        let unfreeze_timestamp = state_file
            .latest_data
            .reset_frozen
            .as_ref()
            .map(|f| f.timestamp_until);

        let state = Arc::new(Self {
            jrpc_client: JrpcClient::new(&settings.rpc_url)?,
            github_client,
            inventory_file: settings.inventory_file.clone(),
            ansible_config_file: settings.ansible_config_file.clone(),
            node_config_file: settings.node_config_file.clone(),
            logger_config_file: settings.logger_config_file.clone(),
            zerostate_file: settings.zerostate_file.clone(),
            reset_playbook: settings.reset_playbook.clone(),
            setup_playbook: settings.setup_playbook.clone(),
            allowed_groups: settings.allowed_groups.iter().copied().collect(),
            authentication_enabled: settings.authentication_enabled,
            state_file: Mutex::new(state_file),
            reset_running: AtomicBool::new(false),
            unfreeze_notify: Mutex::new(None),
        });

        if let Some(at) = unfreeze_timestamp {
            let duration = Duration::from_secs(at.saturating_sub(now_sec()));
            *state.unfreeze_notify.lock().unwrap() =
                Some(tokio::spawn(state.clone().unfreeze_task(bot, duration)).abort_handle());
        }

        Ok(state)
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

    pub fn freeze(self: &Arc<Self>, bot: &Bot, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let (duration, reason) = match expr.split_once(':') {
            Some((duration, reason)) => {
                let duration = humantime::parse_duration(duration.trim())?;
                (duration, Some(reason.trim().to_owned()))
            }
            None => {
                let duration = humantime::parse_duration(expr.trim())?;
                (duration, None)
            }
        };
        anyhow::ensure!(
            duration <= Duration::from_secs(86400),
            "Cannot freeze for more than 24 hours"
        );

        let mut state_file = self.state_file.lock().unwrap();
        if let Some(frozen) = &state_file.latest_data.reset_frozen {
            return Ok(Reply::ResetFrozen(frozen.clone()));
        }

        let timestamp_until = now_sec() + duration.as_secs();
        state_file.latest_data.reset_frozen = Some(ResetFrozen {
            reason,
            timestamp_until,
            chat_id: msg.chat.id,
            message_id: msg.id,
            message_thread_id: msg.thread_id,
        });
        state_file.save()?;

        {
            let mut notify = self.unfreeze_notify.lock().unwrap();
            if let Some(notify) = notify.take() {
                notify.abort();
            }
            *notify = Some(
                tokio::spawn(self.clone().unfreeze_task(bot.clone(), duration)).abort_handle(),
            );
        }

        Ok(Reply::Freeze)
    }

    pub fn unfreeze(&self, msg: &Message) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let mut state_file = self.state_file.lock().unwrap();
        if state_file.latest_data.reset_frozen.is_some() {
            state_file.latest_data.reset_frozen = None;
            state_file.save()?;
        }

        if let Some(notify) = self.unfreeze_notify.lock().unwrap().take() {
            notify.abort();
        }

        Ok(Reply::Unfreeze)
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

    pub async fn reset_network(&self, bot: Bot, msg: &Message, params: ResetParams) -> Result<()> {
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

        'frozen: {
            let frozen = {
                let mut state_file = self.state_file.lock().unwrap();
                let Some(frozen) = &state_file.latest_data.reset_frozen else {
                    break 'frozen;
                };

                if now_sec() >= frozen.timestamp_until {
                    if let Some(notify) = self.unfreeze_notify.lock().unwrap().take() {
                        notify.abort();
                    }

                    // Unfreeze on timestamp reached
                    state_file.latest_data.reset_frozen = None;
                    state_file.save()?;
                    break 'frozen;
                }

                frozen.clone()
            };

            bot.send_message(msg.chat.id, Reply::ResetFrozen(frozen).to_string())
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

        #[derive(Clone, Copy)]
        struct ReplyText<'a> {
            commit_info: &'a CommitInfo,
            started_at: Instant,
        }

        impl<'a> ReplyText<'a> {
            fn with_title<T: std::fmt::Display>(self, title: T) -> ReplyTextWithTitle<'a, T> {
                ReplyTextWithTitle { title, body: self }
            }
        }

        impl std::fmt::Display for ReplyText<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let elapsed_secs = self.started_at.elapsed().as_secs();
                let duration = humantime::format_duration(Duration::from_secs(elapsed_secs));
                writeln!(f, "⏰ Elapsed: {duration}\n")?;

                let info = self.commit_info;

                for line in info.message.lines() {
                    writeln!(f, "> {line}")?;
                }
                writeln!(f, "Commit: `{}`\n", info.sha)?;

                if !info.branches.is_empty() {
                    let mut first = true;
                    write!(f, "Branch: ")?;
                    for name in &info.branches {
                        write!(
                            f,
                            "{}`{name}`",
                            if std::mem::take(&mut first) { "" } else { ", " }
                        )?;
                    }
                    writeln!(f, "\n")?;
                }

                f.write_str(&info.html_url)
            }
        }

        struct ReplyTextWithTitle<'a, T> {
            title: T,
            body: ReplyText<'a>,
        }

        impl<T: std::fmt::Display> std::fmt::Display for ReplyTextWithTitle<'_, T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                writeln!(f, "{}", self.title)?;
                std::fmt::Display::fmt(&self.body, f)
            }
        }

        impl LongReply {
            async fn reply_error(
                &self,
                body: ReplyText<'_>,
                title: &str,
                error: String,
            ) -> Result<()> {
                let (title, file) = if error.len() <= 256 {
                    (format!("🟥 {title}:\n```\n{error}\n```\n"), None)
                } else {
                    (format!("🟥 {title}"), Some(error))
                };

                self.update(body.with_title(title)).await?;
                if let Some(file) = file {
                    self.send_document("error.txt", file).await?;
                }

                self.react(Emoji::Clown).await?;
                Ok(())
            }
        }

        let commit_info = self.get_commit_info(&params.commit).await?;
        let reply_body = ReplyText {
            commit_info: &commit_info,
            started_at: Instant::now(),
        };

        let r = LongReply::begin(
            bot,
            msg,
            reply_body.with_title("🔄 Starting network reset..."),
        )
        .await?;

        r.update(reply_body.with_title("🔄 Updating gate..."))
            .await?;

        let gate_update_output = self.run_gate_update().await?;
        if !gate_update_output.status.success() {
            let e = String::from_utf8_lossy(&gate_update_output.stderr).to_string();
            tracing::error!("Gate update failed: {e}");

            r.reply_error(reply_body, "Gate update failed", e).await?;
            return Ok(());
        }

        r.update(reply_body.with_title("🔄 Gate updated. Running reset playbook..."))
            .await?;

        let reset_output = self.run_ansible_reset(&params.commit).await?;
        if !reset_output.status.success() {
            let e = String::from_utf8_lossy(&reset_output.stdout).to_string();
            tracing::error!("Reset playbook execution failed: {e}");

            r.reply_error(reply_body, "Reset playbook execution failed", e)
                .await?;
            return Ok(());
        }

        r.update(reply_body.with_title("🔄 Reset completed. Running setup playbook..."))
            .await?;

        let setup_output = self.run_ansible_setup(&params).await?;
        if !setup_output.status.success() {
            let e = String::from_utf8_lossy(&setup_output.stdout).to_string();
            tracing::error!("Setup playbook execution failed: {e}");

            r.reply_error(reply_body, "Setup playbook execution failed", e)
                .await?;
            return Ok(());
        }

        let link_preview = LinkPreviewOptions {
            url: commit_info.html_url.clone(),
        };

        {
            let mut state_file = self.state_file.lock().unwrap();
            state_file.latest_data.last_commit_info = Some(commit_info.clone());
            state_file.save()?;
        }

        r.update(reply_body.with_title("✅ Network reset completed successfully!"))
            .link_preview_options(Some(link_preview))
            .await?;

        r.react(Emoji::Hotdog).await?;
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

    async fn run_ansible_reset(&self, commit: &str) -> Result<std::process::Output> {
        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.reset_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={commit}"))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env(ANSIBLE_CONFIG_ENV, &self.ansible_config_file)
            .output()
            .await
            .context("Failed to execute reset playbook")
    }

    async fn run_ansible_setup(&self, params: &ResetParams) -> Result<std::process::Output> {
        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(&self.inventory_file)
            .arg(&self.setup_playbook)
            .arg("--extra-vars")
            .arg(format!(
                "tycho_commit={} tycho_build_profile={} n_nodes={}",
                params.commit, params.build_profile, params.node_count
            ))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env(ANSIBLE_CONFIG_ENV, &self.ansible_config_file)
            .output()
            .await
            .context("Failed to execute setup playbook")
    }

    pub fn set_node_config(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }
        self.set_config_impl(&self.node_config_file, expr)
            .map(Reply::NodeConfigUpdated)
    }

    pub fn get_node_config(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(&self.node_config_file, expr)
            .map(Reply::NodeConfigParam)
    }

    pub fn set_logger_config(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }
        self.set_config_impl(&self.logger_config_file, expr)
            .map(Reply::LoggerConfigUpdated)
    }

    pub fn get_logger_config(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(&self.logger_config_file, expr)
            .map(Reply::LoggerConfigParam)
    }

    pub fn set_zerostate(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }
        self.set_config_impl(&self.zerostate_file, expr)
            .map(Reply::ZerostateUpdated)
    }

    pub fn get_zerostate(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(&self.zerostate_file, expr)
            .map(Reply::ZerostateParam)
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

    fn set_config_impl(&self, path: &str, expr: &str) -> Result<ConfigDiff> {
        let mut config = Config::from_file(path)?;

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

        config.save()
    }

    fn get_config_impl(&self, path: &str, expr: &str) -> Result<String> {
        let config = Config::from_file(path)?;
        let path = parse_config_value_path(expr)?;
        let value = serde_json::to_string_pretty(config.get(&path)?)?;
        Ok(value)
    }

    async fn unfreeze_task(self: Arc<Self>, bot: Bot, duration: Duration) {
        tokio::time::sleep(duration).await;

        let frozen = {
            let mut state_file = self.state_file.lock().unwrap();
            let Some(frozen) = state_file.latest_data.reset_frozen.take() else {
                return;
            };

            if let Err(e) = state_file.save() {
                tracing::error!("Failed to save state file: {e}");
            }

            frozen
        };

        let mut msg = bot.send_message(frozen.chat_id, Reply::Unfreeze.to_string());
        msg.reply_to_message_id = Some(frozen.message_id);
        msg.message_thread_id = frozen.message_thread_id;
        if let Err(e) = msg.await {
            tracing::error!("Failed to send unfreeze message: {e}");
        }
    }
}

const ANSIBLE_CONFIG_ENV: &str = "ANSIBLE_CONFIG";

#[derive(Debug, Clone)]
pub struct ResetParams {
    pub commit: String,
    pub node_count: usize,
    pub build_profile: String,
}

impl ResetParams {
    const PARAM_NODE_COUNT: &'static str = "nodes";
    const PARAM_BUILD_PROFILE: &'static str = "profile";

    const DEFAULT_COMMIT: &'static str = "master";
    const DEFAULT_NODE_COUNT: usize = 13;
    const DEFAULT_BUILD_PROFILE: &'static str = "release";
}

impl FromStr for ResetParams {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut commit = None;
        let mut node_count = Self::DEFAULT_NODE_COUNT;
        let mut build_profile = Self::DEFAULT_BUILD_PROFILE.to_string();

        for item in s.split(';') {
            match item.split_once('=') {
                None => {
                    anyhow::ensure!(commit.is_none(), "invalid param: {item}");
                    commit = Some(item.trim().to_owned());
                }
                Some((param, value)) => match param.trim() {
                    Self::PARAM_NODE_COUNT => node_count = value.trim().parse()?,
                    Self::PARAM_BUILD_PROFILE => value.trim().clone_into(&mut build_profile),
                    param => anyhow::bail!("unknown param: {param}"),
                },
            }
        }

        Ok(Self {
            commit: commit.unwrap_or_else(|| Self::DEFAULT_COMMIT.to_owned()),
            node_count,
            build_profile,
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
    reset_frozen: Option<ResetFrozen>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub html_url: String,
    pub message: String,
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetFrozen {
    pub reason: Option<String>,
    pub timestamp_until: u64,

    pub chat_id: ChatId,
    pub message_id: MessageId,
    pub message_thread_id: Option<i32>,
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
    original_msg_id: MessageId,
    reply_msg_id: MessageId,
    reply_thread_id: Option<i32>,
}

impl LongReply {
    async fn begin(bot: Bot, msg: &Message, text: impl std::fmt::Display) -> Result<Self> {
        let chat_id = msg.chat.id;
        let reply = bot
            .send_message(chat_id, text.to_string())
            .reply_to(msg)
            .markdown()
            .await?;

        Ok(Self {
            bot,
            chat_id,
            original_msg_id: msg.id,
            reply_msg_id: reply.id,
            reply_thread_id: reply.thread_id,
        })
    }

    fn update(
        &self,
        text: impl std::fmt::Display,
    ) -> JsonRequest<WithLinkPreview<teloxide::payloads::EditMessageText>> {
        let req = WithLinkPreview {
            inner: teloxide::payloads::EditMessageText::new(
                self.chat_id,
                self.reply_msg_id,
                text.to_string(),
            ),
            link_preview_options: None,
        };
        JsonRequest::new(self.bot.clone(), req).markdown()
    }

    fn send_document(
        &self,
        name: impl Into<Cow<'static, str>>,
        error: String,
    ) -> MultipartRequest<teloxide::payloads::SendDocument> {
        let document = teloxide::types::InputFile::memory(error).file_name(name);
        let mut req = self.bot.send_document(self.chat_id, document);
        req.reply_to_message_id = Some(self.reply_msg_id);
        req.message_thread_id = self.reply_thread_id;
        req
    }

    fn react(&self, emoji: Emoji) -> JsonRequest<SetMessageReaction> {
        let req = SetMessageReaction {
            chat_id: self.chat_id.into(),
            message_id: self.original_msg_id,
            reaction: vec![ReactionType::Emoji {
                emoji: emoji.to_string(),
            }],
        };
        JsonRequest::new(self.bot.clone(), req)
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
    Freeze,
    Unfreeze,
    NodeConfigUpdated(ConfigDiff),
    NodeConfigParam(String),
    LoggerConfigUpdated(ConfigDiff),
    LoggerConfigParam(String),
    ZerostateUpdated(ConfigDiff),
    ZerostateParam(String),
    AccessDenied,
    ResetFrozen(ResetFrozen),
}

impl Reply {
    pub fn link_preview_options(&self) -> Option<LinkPreviewOptions> {
        match self {
            Self::Commit(commit) => Some(LinkPreviewOptions {
                url: commit.html_url.clone(),
            }),
            _ => None,
        }
    }
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timings(timings) => {
                let reply_data = serde_json::to_string_pretty(&timings).unwrap();
                write!(f, "Timings:\n```json\n{reply_data}\n```")
            }
            Self::Commit(commit) => {
                for line in commit.message.lines() {
                    writeln!(f, "> {line}")?;
                }
                writeln!(f, "Commit: `{}`\n", commit.sha)?;

                if !commit.branches.is_empty() {
                    let mut first = true;
                    write!(f, "Branch: ")?;
                    for name in &commit.branches {
                        write!(
                            f,
                            "{}`{name}`",
                            if std::mem::take(&mut first) { "" } else { ", " }
                        )?;
                    }
                    writeln!(f, "\n")?;
                }

                f.write_str(&commit.html_url)
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
            Self::Freeze => {
                write!(f, "Network reset is now frozen")
            }
            Self::Unfreeze => {
                write!(f, "Network reset is now available")
            }
            Self::NodeConfigUpdated(msg) => {
                write!(f, "Node config updated:\n```json\n{msg}\n```")
            }
            Self::NodeConfigParam(config) => {
                write!(f, "```json\n{config}\n```")
            }
            Self::LoggerConfigUpdated(msg) => {
                write!(f, "Logger config updated:\n```json\n{msg}\n```")
            }
            Self::LoggerConfigParam(config) => {
                write!(f, "```json\n{config}\n```")
            }
            Self::ZerostateUpdated(msg) => {
                write!(f, "Zerostate config updated:\n```json\n{msg}\n```")
            }
            Self::ZerostateParam(config) => {
                write!(f, "```json\n{config}\n```")
            }
            Self::AccessDenied => {
                write!(f, "👮‍♀️ Access denied")
            }
            Self::ResetFrozen(frozen) => {
                let time_remaining =
                    Duration::from_secs(frozen.timestamp_until.saturating_sub(now_sec()));

                write!(
                    f,
                    "❄️ Network reset is frozen\n⏰ Time remaining: {}",
                    humantime::format_duration(time_remaining),
                )?;

                if let Some(reason) = &frozen.reason {
                    write!(f, "\n\n> {reason}")?;
                }

                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_params_from_str() {
        let params = "".parse::<ResetParams>().unwrap();
        assert_eq!(params.commit, "master");
        assert_eq!(params.node_count, ResetParams::DEFAULT_NODE_COUNT);
        assert_eq!(params.build_profile, ResetParams::DEFAULT_BUILD_PROFILE);

        let params = "feature/new".parse::<ResetParams>().unwrap();
        assert_eq!(params.commit, "feature/new");
        assert_eq!(params.node_count, ResetParams::DEFAULT_NODE_COUNT);
        assert_eq!(params.build_profile, ResetParams::DEFAULT_BUILD_PROFILE);

        let params = "nodes=10; profile=debug".parse::<ResetParams>().unwrap();
        assert_eq!(params.commit, "master");
        assert_eq!(params.node_count, 10);
        assert_eq!(params.build_profile, "debug");
    }
}

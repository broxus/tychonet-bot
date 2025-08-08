use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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
use teloxide::types::{ChatId, MessageId, ReplyParameters, ThreadId};
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

struct NetworkDescr {
    jrpc_client: JrpcClient,
    inventory: String,
    reset_running: AtomicBool,
}

pub struct State {
    github_client: GithubClient,
    default_network: String,
    networks: HashMap<String, NetworkDescr>,
    ansible_config_file: String,
    node_config_file: String,
    logger_config_file: String,
    zerostate_file: String,
    reset_playbook: String,
    setup_playbook: String,
    allowed_groups: HashSet<i64>,
    authentication_enabled: bool,
    state_file: Mutex<StateFile>,
    unfreeze_notifies: Mutex<HashMap<String, AbortHandle>>,
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

        let unfreeze_timestamps = state_file
            .latest_data
            .reset_frozen
            .iter()
            .map(|(network, f)| (network.clone(), f.timestamp_until))
            .collect::<Vec<_>>();

        anyhow::ensure!(
            settings
                .inventory_files
                .contains_key(&settings.default_network),
            "no inventory found for the default network `{}`",
            settings.default_network
        );

        let networks = settings
            .inventory_files
            .iter()
            .map(|(network, inventory)| {
                let Some(jrpc_url) = settings.rpc_urls.get(network) else {
                    anyhow::bail!("no JRPC url found for network `{network}`");
                };
                let jrpc_client = JrpcClient::new(jrpc_url)
                    .with_context(|| format!("failed to create JRPC client for {network}"))?;

                let descr = NetworkDescr {
                    jrpc_client,
                    inventory: inventory.clone(),
                    reset_running: AtomicBool::new(false),
                };
                Ok::<_, anyhow::Error>((network.clone(), descr))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        let state = Arc::new(Self {
            github_client,
            default_network: settings.default_network.clone(),
            networks,
            ansible_config_file: settings.ansible_config_file.clone(),
            node_config_file: settings.node_config_file.clone(),
            logger_config_file: settings.logger_config_file.clone(),
            zerostate_file: settings.zerostate_file.clone(),
            reset_playbook: settings.reset_playbook.clone(),
            setup_playbook: settings.setup_playbook.clone(),
            allowed_groups: settings.allowed_groups.iter().copied().collect(),
            authentication_enabled: settings.authentication_enabled,
            state_file: Mutex::new(state_file),
            unfreeze_notifies: Mutex::new(Default::default()),
        });

        for (network, at) in unfreeze_timestamps {
            let duration = Duration::from_secs(at.saturating_sub(now_sec()));
            let task = tokio::spawn(state.clone().unfreeze_task(
                bot.clone(),
                network.clone(),
                duration,
            ))
            .abort_handle();

            state
                .unfreeze_notifies
                .lock()
                .unwrap()
                .insert(network, task);
        }

        Ok(state)
    }

    pub async fn get_status(&self) -> Result<Reply> {
        self.get_current_jrpc_client()?
            .get_timings()
            .await
            .map(Reply::Timings)
            .context("Failed to get status")
    }

    pub async fn get_account(&self, address: &StdAddr) -> Result<Reply> {
        let res = self.get_current_jrpc_client()?.get_account(address).await?;
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
        let res = self.get_current_jrpc_client()?.get_config().await?;
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
        // anyhow::ensure!(
        //     duration <= Duration::from_secs(86400),
        //     "Cannot freeze for more than 24 hours"
        // );

        let mut state_file = self.state_file.lock().unwrap();
        let network = state_file
            .latest_data
            .current_network_name(&self.default_network)
            .to_owned();

        if let Some(frozen) = state_file.latest_data.reset_frozen.get(&network) {
            return Ok(Reply::ResetFrozen(frozen.clone()));
        }

        let timestamp_until = now_sec() + duration.as_secs();
        state_file.latest_data.reset_frozen.insert(
            network.clone(),
            ResetFrozen {
                network: network.clone(),
                reason,
                timestamp_until,
                chat_id: msg.chat.id,
                message_id: msg.id,
                message_thread_id: msg.thread_id,
            },
        );
        state_file.save()?;

        {
            let mut notify = self.unfreeze_notifies.lock().unwrap();
            if let Some(notify) = notify.remove(&network) {
                notify.abort();
            }

            let task = tokio::spawn(self.clone().unfreeze_task(
                bot.clone(),
                network.clone(),
                duration,
            ))
            .abort_handle();
            notify.insert(network.clone(), task);
        }

        Ok(Reply::Freeze { network })
    }

    pub fn unfreeze(&self, msg: &Message) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let mut state_file = self.state_file.lock().unwrap();
        let network = state_file
            .latest_data
            .current_network_name(&self.default_network)
            .to_owned();

        if state_file
            .latest_data
            .reset_frozen
            .remove(&network)
            .is_some()
        {
            state_file.save()?;
        }

        if let Some(notify) = self.unfreeze_notifies.lock().unwrap().remove(&network) {
            notify.abort();
        }

        Ok(Reply::Unfreeze { network })
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

    pub fn set_workspace(&self, msg: &Message, expr: &str) -> Result<Reply> {
        use std::collections::hash_map;

        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let SetWorkspaceParams {
            workspace,
            copy_from,
        } = expr.parse()?;

        let mut state_file = self.state_file.lock().unwrap();

        let prev_workspace = match &copy_from {
            Some(workspace) if workspace != DEFAULT_WORKSPACE => state_file
                .latest_data
                .workspaces
                .get_mut(workspace)
                .with_context(|| format!("workspace not found {workspace}"))?,
            _ => state_file
                .latest_data
                .workspaces
                .entry(DEFAULT_WORKSPACE.to_owned())
                .or_default(),
        };

        let compute_prev_source = |object: &Option<JsonObject>| {
            if object.is_none() {
                ConfigSource::FromFile
            } else if copy_from.is_some() {
                ConfigSource::Copied
            } else {
                ConfigSource::Unchanged
            }
        };
        let node_source = compute_prev_source(&prev_workspace.node);
        let logger_source = compute_prev_source(&prev_workspace.logger);
        let zerostate_source = compute_prev_source(&prev_workspace.zerostate);

        prev_workspace.preload(
            &self.node_config_file,
            &self.logger_config_file,
            &self.zerostate_file,
        )?;
        let prev_workspace = prev_workspace.clone();

        let is_new;
        let network;
        match state_file.latest_data.workspaces.entry(workspace.clone()) {
            hash_map::Entry::Vacant(entry) => {
                is_new = true;
                network = prev_workspace.network.clone();
                entry.insert(prev_workspace);
            }
            hash_map::Entry::Occupied(mut entry) => {
                is_new = false;
                if copy_from.is_some() {
                    entry.insert(prev_workspace);
                }
                network = entry.get().network.clone();
            }
        }

        state_file.latest_data.current_workspace = Some(workspace);
        state_file.save()?;

        Ok(Reply::WorkspaceChanged {
            is_new,
            network: network.unwrap_or_else(|| self.default_network.clone()),
            node_source,
            logger_source,
            zerostate_source,
            copy_from,
        })
    }

    pub fn get_workspace(&self) -> Result<Reply> {
        let state_file = self.state_file.lock().unwrap();

        let current = state_file
            .latest_data
            .current_workspace
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_owned());

        let mut workspaces = Vec::new();
        if !state_file.latest_data.workspaces.contains_key(&current) {
            workspaces.push(current.clone());
        }
        workspaces.extend(state_file.latest_data.workspaces.keys().cloned());
        workspaces.sort_unstable();

        Ok(Reply::Workspaces {
            current,
            workspaces,
        })
    }

    pub fn delete_workspace(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let workspace_name = expr.trim();
        if workspace_name == DEFAULT_WORKSPACE {
            anyhow::bail!("cannot remove the default workspace");
        }

        let mut state_file = self.state_file.lock().unwrap();

        if matches!(
            &state_file.latest_data.current_workspace,
            Some(current) if current == workspace_name
        ) {
            state_file.latest_data.current_workspace = None;
        }

        if state_file
            .latest_data
            .workspaces
            .remove(workspace_name)
            .is_none()
        {
            anyhow::bail!("workspace does not exist: `{workspace_name}`");
        }

        state_file.save()?;
        Ok(Reply::WorkspaceRemoved)
    }

    pub fn get_network(&self) -> Result<Reply> {
        let state_file = self.state_file.lock().unwrap();

        let current = state_file
            .latest_data
            .current_network_name(&self.default_network)
            .to_owned();

        let mut networks = self.networks.keys().cloned().collect::<Vec<_>>();
        networks.sort_unstable();

        Ok(Reply::Networks { current, networks })
    }

    pub fn set_network(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let SetNetworkParams { network } = expr.parse()?;
        anyhow::ensure!(
            self.networks.contains_key(&network),
            "no inventory found for the network `{network}`"
        );

        let mut state_file = self.state_file.lock().unwrap();
        let current_workspace_name = state_file.latest_data.current_workspace_name();

        let current_workspace = state_file
            .latest_data
            .workspaces
            .entry(current_workspace_name.clone())
            .or_default();

        current_workspace.network = Some(network.clone());
        state_file.save()?;

        Ok(Reply::WorkspaceChanged {
            is_new: false,
            network,
            node_source: ConfigSource::Unchanged,
            logger_source: ConfigSource::Unchanged,
            zerostate_source: ConfigSource::Unchanged,
            copy_from: None,
        })
    }

    pub fn get_reset_type(&self) -> Result<Reply> {
        let state_file = self.state_file.lock().unwrap();
        Ok(Reply::ResetType(state_file.latest_data.reset_type))
    }

    pub fn set_reset_type(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }

        let reset_type = expr.parse()?;

        let mut state_file = self.state_file.lock().unwrap();
        state_file.latest_data.reset_type = reset_type;
        state_file.save()?;

        Ok(Reply::ResetType(reset_type))
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

        let network;
        let descr;
        let reset_type;
        'frozen: {
            let frozen = {
                let mut state_file = self.state_file.lock().unwrap();
                reset_type = params
                    .reset_type
                    .unwrap_or(state_file.latest_data.reset_type);

                network = params.network.clone().unwrap_or_else(|| {
                    state_file
                        .latest_data
                        .current_network_name(&self.default_network)
                        .to_owned()
                });

                descr = self
                    .networks
                    .get(&network)
                    .with_context(|| format!("no inventory found for the network `{network}`"))?;

                let Some(frozen) = state_file.latest_data.reset_frozen.get(&network) else {
                    state_file.latest_data.apply_workspace_configs(
                        &self.node_config_file,
                        &self.logger_config_file,
                        &self.zerostate_file,
                    )?;
                    state_file.save()?;
                    break 'frozen;
                };

                if now_sec() >= frozen.timestamp_until {
                    if let Some(notify) = self.unfreeze_notifies.lock().unwrap().remove(&network) {
                        notify.abort();
                    }

                    // Unfreeze on timestamp reached
                    state_file.latest_data.reset_frozen.remove(&network);
                    state_file.latest_data.apply_workspace_configs(
                        &self.node_config_file,
                        &self.logger_config_file,
                        &self.zerostate_file,
                    )?;
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
            if descr.reset_running.swap(true, Ordering::Relaxed) {
                bot.send_message(msg.chat.id, "Reset is already running")
                    .reply_to(msg)
                    .await?;
                return Ok(());
            }

            ResetGuard(&descr.reset_running)
        };

        #[derive(Clone, Copy)]
        struct ReplyText<'a> {
            network: &'a str,
            commit_info: &'a CommitInfo,
            reset_type: ResetType,
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
                writeln!(f, "🌐 Network: `{}`", self.network)?;
                writeln!(f, "⏰ Elapsed: {duration}")?;
                writeln!(
                    f,
                    "{} Reset type: *{}*\n",
                    self.reset_type.as_emoji(),
                    self.reset_type
                )?;

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

        trait LongReplyExt {
            async fn reply_error(
                &self,
                body: ReplyText<'_>,
                title: &str,
                error: String,
            ) -> Result<()>;
        }

        impl LongReplyExt for LongReply {
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
            network: &network,
            commit_info: &commit_info,
            reset_type,
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

        let reset_output = self
            .run_ansible_reset(&descr.inventory, &params.commit, reset_type)
            .await?;
        if !reset_output.status.success() {
            let e = String::from_utf8_lossy(&reset_output.stdout).to_string();
            tracing::error!("Reset playbook execution failed: {e}");

            r.reply_error(reply_body, "Reset playbook execution failed", e)
                .await?;
            return Ok(());
        }

        r.update(reply_body.with_title("🔄 Reset completed. Running setup playbook..."))
            .await?;

        let setup_output = self.run_ansible_setup(&descr.inventory, &params).await?;
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

    async fn run_ansible_reset(
        &self,
        inventory_path: &str,
        commit: &str,
        reset_type: ResetType,
    ) -> Result<std::process::Output> {
        let restart_only = matches!(reset_type, ResetType::Restart);

        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(inventory_path)
            .arg(&self.reset_playbook)
            .arg("--extra-vars")
            .arg(format!("tycho_commit={commit} restart_only={restart_only}"))
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env(ANSIBLE_CONFIG_ENV, &self.ansible_config_file)
            .output()
            .await
            .context("Failed to execute reset playbook")
    }

    async fn run_ansible_setup(
        &self,
        inventory_path: &str,
        params: &ResetParams,
    ) -> Result<std::process::Output> {
        let mut args = format!(
            "tycho_commit={} tycho_build_profile={} n_nodes={}",
            params.commit, params.build_profile, params.node_count,
        );

        if let Some(repo) = &params.repo {
            args = format!("{args} tycho_repo={repo}");
        }

        tokio::process::Command::new("ansible-playbook")
            .arg("-i")
            .arg(inventory_path)
            .arg(&self.setup_playbook)
            .arg("--extra-vars")
            .arg(args)
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
        self.set_config_impl(ConfigType::Node, &self.node_config_file, expr)
            .map(Reply::NodeConfigUpdated)
    }

    pub fn get_node_config(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(ConfigType::Node, &self.node_config_file, expr)
            .map(Reply::NodeConfigParam)
    }

    pub fn set_logger_config(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }
        self.set_config_impl(ConfigType::Logger, &self.logger_config_file, expr)
            .map(Reply::LoggerConfigUpdated)
    }

    pub fn get_logger_config(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(ConfigType::Logger, &self.logger_config_file, expr)
            .map(Reply::LoggerConfigParam)
    }

    pub fn set_zerostate(&self, msg: &Message, expr: &str) -> Result<Reply> {
        if !self.check_auth(msg) {
            return Ok(Reply::AccessDenied);
        }
        self.set_config_impl(ConfigType::Zerostate, &self.zerostate_file, expr)
            .map(Reply::ZerostateUpdated)
    }

    pub fn get_zerostate(&self, expr: &str) -> Result<Reply> {
        self.get_config_impl(ConfigType::Zerostate, &self.zerostate_file, expr)
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

    fn set_config_impl(&self, ty: ConfigType, path: &str, expr: &str) -> Result<ConfigDiff> {
        let mut state_file = self.state_file.lock().unwrap();

        let object = state_file.latest_data.get_config_object(ty);
        let mut config = match object {
            Some(object) => Config::from_value(path, object.clone())?,
            None => Config::from_file(path)?,
        };

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

        *object = Some(config.as_object()?);
        let diff = config.save()?;
        state_file.save()?;

        Ok(diff)
    }

    fn get_config_impl(&self, ty: ConfigType, path: &str, expr: &str) -> Result<String> {
        let field_path = parse_config_value_path(expr)?;

        let mut state_file = self.state_file.lock().unwrap();

        let object = state_file.latest_data.get_config_object(ty);
        let config = match object {
            Some(object) => Config::from_value(path, object.clone())?,
            None => {
                let config = Config::from_file(path)?;
                *object = Some(config.as_object()?);
                state_file.save()?;
                config
            }
        };

        let value = serde_json::to_string_pretty(config.get(&field_path)?)?;
        Ok(value)
    }

    fn get_current_jrpc_client(&self) -> Result<&JrpcClient> {
        let state_file = self.state_file.lock().unwrap();
        let network_name = state_file
            .latest_data
            .current_network_name(&self.default_network);
        self.networks
            .get(network_name)
            .map(|descr| &descr.jrpc_client)
            .with_context(|| format!("no JRPC client found for the network `{network_name}`"))
    }

    async fn unfreeze_task(self: Arc<Self>, bot: Bot, network: String, duration: Duration) {
        tokio::time::sleep(duration).await;

        let frozen = {
            let mut state_file = self.state_file.lock().unwrap();
            let Some(frozen) = state_file.latest_data.reset_frozen.remove(&network) else {
                return;
            };

            if let Err(e) = state_file.save() {
                tracing::error!("Failed to save state file: {e}");
            }

            frozen
        };

        let mut msg = bot.send_message(frozen.chat_id, Reply::Unfreeze { network }.to_string());
        msg.reply_parameters = Some(ReplyParameters {
            message_id: frozen.message_id,
            ..Default::default()
        });
        msg.message_thread_id = frozen.message_thread_id;
        if let Err(e) = msg.await {
            tracing::error!("Failed to send unfreeze message: {e}");
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigType {
    Logger,
    Node,
    Zerostate,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigSource {
    Unchanged,
    Copied,
    FromFile,
}

const ANSIBLE_CONFIG_ENV: &str = "ANSIBLE_CONFIG";

#[derive(Debug, Clone)]
pub struct ResetParams {
    pub commit: String,
    pub node_count: usize,
    pub build_profile: String,
    pub repo: Option<String>,
    pub reset_type: Option<ResetType>,
    pub network: Option<String>,
}

impl ResetParams {
    const PARAM_REPO: &'static str = "repo";
    const PARAM_NODE_COUNT: &'static str = "nodes";
    const PARAM_BUILD_PROFILE: &'static str = "profile";
    const PARAM_RESET_TYPE: &'static str = "type";
    const PARAM_NETWORK: &'static str = "network";

    const DEFAULT_COMMIT: &'static str = "master";
    const DEFAULT_NODE_COUNT: usize = 13;
    const DEFAULT_BUILD_PROFILE: &'static str = "release";
}

impl FromStr for ResetParams {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut commit = None;
        let mut repo = None;
        let mut node_count = Self::DEFAULT_NODE_COUNT;
        let mut build_profile = Self::DEFAULT_BUILD_PROFILE.to_string();
        let mut reset_type = None::<ResetType>;
        let mut network = None::<String>;

        for item in s.split(';') {
            match item.split_once('=') {
                None => {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }

                    anyhow::ensure!(commit.is_none(), "invalid param: {item}");
                    commit = Some(item.trim().to_owned());
                }
                Some((param, value)) => match param.trim() {
                    Self::PARAM_REPO => repo = Some(value.trim().to_owned()),
                    Self::PARAM_NODE_COUNT => node_count = value.trim().parse()?,
                    Self::PARAM_BUILD_PROFILE => value.trim().clone_into(&mut build_profile),
                    Self::PARAM_RESET_TYPE => reset_type = Some(value.trim().parse()?),
                    Self::PARAM_NETWORK => network = Some(value.trim().to_owned()),
                    param => anyhow::bail!("unknown param: {param}"),
                },
            }
        }

        Ok(Self {
            commit: commit.unwrap_or_else(|| Self::DEFAULT_COMMIT.to_owned()),
            node_count,
            repo,
            build_profile,
            reset_type,
            network,
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
    reset_frozen: HashMap<String, ResetFrozen>,
    #[serde(default)]
    reset_type: ResetType,
    #[serde(default)]
    current_workspace: Option<String>,
    #[serde(default)]
    workspaces: HashMap<String, Workspace>,
}

impl StateFileData {
    fn apply_workspace_configs(
        &mut self,
        node_path: &str,
        logger_path: &str,
        zerostate_path: &str,
    ) -> Result<()> {
        for (ty, path) in [
            (ConfigType::Node, node_path),
            (ConfigType::Logger, logger_path),
            (ConfigType::Zerostate, zerostate_path),
        ] {
            let object = self.get_config_object(ty);
            match object {
                Some(object) => {
                    let config = Config::from_value(path, object.clone())?;
                    config.save()?;
                }
                None => {
                    let config = Config::from_file(path)?;
                    *object = Some(config.as_object()?);
                }
            }
        }
        Ok(())
    }

    fn get_config_object(&mut self, ty: ConfigType) -> &mut Option<JsonObject> {
        self.workspaces
            .entry(self.current_workspace_name())
            .or_default()
            .get_config_object(ty)
    }

    fn current_workspace_name(&self) -> String {
        self.current_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_owned()
    }

    fn current_network_name<'a>(&'a self, default_network: &'a str) -> &'a str {
        let current_workspace = self.current_workspace_name();
        self.workspaces
            .get(&current_workspace)
            .and_then(|w| w.network.as_deref())
            .unwrap_or(default_network)
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct Workspace {
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    node: Option<JsonObject>,
    #[serde(default)]
    logger: Option<JsonObject>,
    #[serde(default)]
    zerostate: Option<JsonObject>,
}

impl Workspace {
    fn preload(&mut self, node_path: &str, logger_path: &str, zerostate_path: &str) -> Result<()> {
        for (ty, path) in [
            (ConfigType::Node, node_path),
            (ConfigType::Logger, logger_path),
            (ConfigType::Zerostate, zerostate_path),
        ] {
            let object = self.get_config_object(ty);
            if object.is_none() {
                let config = Config::from_file(path)?;
                *object = Some(config.as_object()?);
            }
        }
        Ok(())
    }

    fn get_config_object(&mut self, ty: ConfigType) -> &mut Option<JsonObject> {
        match ty {
            ConfigType::Node => &mut self.node,
            ConfigType::Logger => &mut self.logger,
            ConfigType::Zerostate => &mut self.zerostate,
        }
    }
}

type JsonObject = serde_json::Map<String, serde_json::Value>;

struct SetWorkspaceParams {
    workspace: String,
    copy_from: Option<String>,
}

impl SetWorkspaceParams {
    const PARAM_COPY_FROM: &'static str = "copy_from";
}

impl FromStr for SetWorkspaceParams {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut workspace = None;
        let mut copy_from = None::<String>;

        for item in s.split(';') {
            match item.split_once('=') {
                None => {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }

                    anyhow::ensure!(workspace.is_none(), "invalid param: {item}");
                    workspace = Some(item.trim().to_owned());
                }
                Some((param, value)) => match param.trim() {
                    Self::PARAM_COPY_FROM => copy_from = Some(value.trim().to_owned()),
                    param => anyhow::bail!("unknown param: {param}"),
                },
            }
        }

        Ok(Self {
            workspace: workspace.context("workspace name expected")?,
            copy_from,
        })
    }
}

const DEFAULT_WORKSPACE: &str = "default";

struct SetNetworkParams {
    network: String,
}

impl FromStr for SetNetworkParams {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut network = None;

        for item in s.split(';') {
            match item.split_once('=') {
                None => {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }

                    anyhow::ensure!(network.is_none(), "invalid param: {item}");
                    network = Some(item.trim().to_owned());
                }
                Some((param, _)) => anyhow::bail!("unknown param: {}", param.trim()),
            }
        }

        Ok(Self {
            network: network.context("network name expected")?,
        })
    }
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
    pub network: String,
    pub reason: Option<String>,
    pub timestamp_until: u64,

    pub chat_id: ChatId,
    pub message_id: MessageId,
    pub message_thread_id: Option<ThreadId>,
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

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ResetType {
    #[default]
    Full,
    Restart,
}

impl ResetType {
    const FULL: &'static str = "full";
    const RESTART: &'static str = "restart";

    fn as_emoji(&self) -> &'static str {
        match self {
            Self::Full => "💣",
            Self::Restart => "🔄",
        }
    }
}

impl std::fmt::Display for ResetType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => Self::FULL,
            Self::Restart => Self::RESTART,
        })
    }
}

impl FromStr for ResetType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            Self::FULL => Ok(Self::Full),
            Self::RESTART => Ok(Self::Restart),
            _ => anyhow::bail!("unknown reset type"),
        }
    }
}

struct LongReply {
    bot: Bot,
    chat_id: ChatId,
    original_msg_id: MessageId,
    reply_msg_id: MessageId,
    reply_thread_id: Option<ThreadId>,
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
        req.reply_parameters = Some(ReplyParameters {
            message_id: self.reply_msg_id,
            ..Default::default()
        });
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
    Workspaces {
        current: String,
        workspaces: Vec<String>,
    },
    Networks {
        current: String,
        networks: Vec<String>,
    },
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
    Freeze {
        network: String,
    },
    Unfreeze {
        network: String,
    },
    NodeConfigUpdated(ConfigDiff),
    NodeConfigParam(String),
    LoggerConfigUpdated(ConfigDiff),
    LoggerConfigParam(String),
    ZerostateUpdated(ConfigDiff),
    ZerostateParam(String),
    AccessDenied,
    ResetFrozen(ResetFrozen),
    ResetType(ResetType),
    WorkspaceRemoved,
    WorkspaceChanged {
        is_new: bool,
        network: String,
        node_source: ConfigSource,
        logger_source: ConfigSource,
        zerostate_source: ConfigSource,
        copy_from: Option<String>,
    },
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
            Self::Workspaces {
                current,
                workspaces,
            } => {
                for workspace in workspaces {
                    let current = if workspace == current {
                        " // <- current"
                    } else {
                        ""
                    };
                    writeln!(f, "- `{workspace}`{current}")?;
                }
                Ok(())
            }
            Self::Networks { current, networks } => {
                for network in networks {
                    let current = if network == current {
                        " // <- current"
                    } else {
                        ""
                    };
                    writeln!(f, "- `{network}`{current}")?;
                }
                Ok(())
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
            Self::Freeze { network } => {
                writeln!(f, "🌐 Network: `{network}`\n")?;
                writeln!(f, "Reset is now frozen")
            }
            Self::Unfreeze { network } => {
                writeln!(f, "🌐 Network: `{network}`\n")?;
                writeln!(f, "Reset is now available")
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

                writeln!(f, "🌐 Network: `{}`", frozen.network)?;
                writeln!(f, "❄️ Reset is frozen")?;
                write!(
                    f,
                    "⏰ Time remaining: {}",
                    humantime::format_duration(time_remaining),
                )?;

                if let Some(reason) = &frozen.reason {
                    write!(f, "\n\n> {reason}")?;
                }

                Ok(())
            }
            Self::ResetType(reset_type) => {
                write!(f, "Reset type: *{reset_type}*")
            }
            Self::WorkspaceRemoved => {
                write!(f, "Workspace removed")
            }
            Self::WorkspaceChanged {
                is_new,
                network,
                logger_source,
                node_source,
                zerostate_source,
                copy_from,
            } => {
                struct SourceWithWorkspace<'a> {
                    workspace: &'a str,
                    source: ConfigSource,
                }

                impl std::fmt::Display for SourceWithWorkspace<'_> {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        match self.source {
                            ConfigSource::Unchanged => write!(f, "unchanged"),
                            ConfigSource::Copied => write!(f, "copied from `{}`", self.workspace),
                            ConfigSource::FromFile => write!(f, "loaded from file"),
                        }
                    }
                }

                let workspace = copy_from.as_deref().unwrap_or(DEFAULT_WORKSPACE);

                let action = if *is_new { "created" } else { "selected" };
                writeln!(f, "✅ Workspace {action}.")?;
                writeln!(f, "🌐 Network: `{network}`\n")?;
                writeln!(
                    f,
                    "- Node config: {};",
                    SourceWithWorkspace {
                        workspace,
                        source: *node_source
                    }
                )?;
                writeln!(
                    f,
                    "- Logger config: {};",
                    SourceWithWorkspace {
                        workspace,
                        source: *logger_source
                    }
                )?;
                writeln!(
                    f,
                    "- Zerostate config: {};",
                    SourceWithWorkspace {
                        workspace,
                        source: *zerostate_source
                    }
                )?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use everscale_types::boc::BocRepr;
    use everscale_types::models::BlockchainConfig;

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

    #[test]
    fn test_config() -> anyhow::Result<()> {
        let data = "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";
        let params = BocRepr::decode_base64::<BlockchainConfig, _>(data)?;

        let test = serde_json::to_value(&params.params)?;
        println!("{test:?}");

        let value_str =
            serde_json::to_string_pretty(&test.get(15u32.to_string())).unwrap_or_default();
        println!("{value_str:?}");

        Ok(())
    }
}

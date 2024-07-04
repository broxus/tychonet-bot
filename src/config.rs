use serde::{Deserialize, Serialize};
use std::fs;
use anyhow::{Context, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub local_ip: String,
    pub port: u32,
    pub threads: Threads,
    pub network: Network,
    pub dht: Dht,
    pub peer_resolver: PeerResolver,
    pub overlay: Overlay,
    pub public_overlay_client: PublicOverlayClient,
    pub storage: Storage,
    pub blockchain_rpc_service: BlockchainRpcService,
    pub blockchain_block_provider: BlockchainBlockProvider,
    pub rpc: Rpc,
    pub collator: Collator,
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Threads {
    pub rayon_threads: u32,
    pub tokio_workers: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub quic: Option<()>,
    pub connection_manager_channel_capacity: u32,
    pub connectivity_check_interval: String,
    pub max_frame_size: Option<()>,
    pub connect_timeout: String,
    pub connection_backoff: String,
    pub max_connection_backoff: String,
    pub max_concurrent_outstanding_connections: u32,
    pub max_concurrent_connections: Option<()>,
    pub active_peers_event_channel_capacity: u32,
    pub shutdown_idle_timeout: String,
    pub enable_0rtt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dht {
    pub max_k: u32,
    pub max_peer_info_ttl: String,
    pub max_stored_value_ttl: String,
    pub max_storage_capacity: u32,
    pub storage_item_time_to_idle: Option<()>,
    pub local_info_refresh_period: String,
    pub local_info_announce_period: String,
    pub local_info_announce_period_max_jitter: String,
    pub routing_table_refresh_period: String,
    pub routing_table_refresh_period_max_jitter: String,
    pub announced_peers_channel_capacity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerResolver {
    pub max_parallel_resolve_requests: u32,
    pub min_ttl_sec: u32,
    pub update_before_sec: u32,
    pub fast_retry_count: u32,
    pub min_retry_interval: String,
    pub max_retry_interval: String,
    pub stale_retry_interval: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlay {
    pub public_overlay_peer_store_period: String,
    pub public_overlay_peer_store_max_jitter: String,
    pub public_overlay_peer_store_max_entries: u32,
    pub public_overlay_peer_exchange_period: String,
    pub public_overlay_peer_exchange_max_jitter: String,
    pub public_overlay_peer_discovery_period: String,
    pub public_overlay_peer_discovery_max_jitter: String,
    pub exchange_public_entries_batch: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicOverlayClient {
    pub neighbours_update_interval: String,
    pub neighbours_ping_interval: String,
    pub max_neighbours: u32,
    pub max_ping_tasks: u32,
    pub default_roundtrip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Storage {
    pub root_dir: String,
    pub rocksdb_enable_metrics: bool,
    pub rocksdb_lru_capacity: String,
    pub cells_cache_size: String,
    pub archives: Option<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainRpcService {
    pub max_key_blocks_list_len: u32,
    pub serve_persistent_states: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainBlockProvider {
    pub get_next_block_polling_interval: String,
    pub get_block_polling_interval: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rpc {
    pub listen_addr: String,
    pub generate_stub_keyblock: bool,
    pub transactions_gc: TransactionsGc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionsGc {
    pub tx_ttl: String,
    pub interval: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collator {
    pub supported_block_version: u32,
    pub supported_capabilities: Vec<String>,
    pub mc_block_min_interval: String,
    pub max_mc_block_delta_from_bc_to_await_own: u32,
    pub max_uncommitted_chain_length: u32,
    pub uncommitted_chain_to_import_next_anchor: u32,
    pub block_txs_limit: u32,
    pub msgs_exec_params: MsgsExecParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgsExecParams {
    pub set_size: u32,
    pub min_externals_per_set: u32,
    pub group_limit: u32,
    pub group_vert_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub listen_addr: String,
}

impl Config {
    pub fn from_file(path: &str) -> Result<Self> {
        let config_str = fs::read_to_string(path).context("Failed to read config file")?;
        let config: Config = serde_json::from_str(&config_str).context("Failed to parse config file")?;
        Ok(config)
    }

    pub fn to_file(&self, path: &str) -> Result<()> {
        let config_str = serde_json::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(path, config_str).context("Failed to write config file")?;
        Ok(())
    }

    pub fn update(&mut self, key: &str, value: &str) -> Result<()> {
        let mut current = serde_json::to_value(self.clone()).context("Failed to convert config to JSON value")?;
        let parts: Vec<&str> = key.split('.').collect();
        let mut current_ref = &mut current;

        for &part in &parts[..parts.len() - 1] {
            current_ref = current_ref
                .get_mut(part)
                .ok_or_else(|| anyhow::anyhow!("Key path does not exist: {}", key))?;
        }

        *current_ref
            .get_mut(parts.last().unwrap())
            .ok_or_else(|| anyhow::anyhow!("Key path does not exist: {}", key))? = serde_json::from_str(value)?;

        *self = serde_json::from_value(current).context("Failed to convert JSON value back to config")?;

        Ok(())
    }
}



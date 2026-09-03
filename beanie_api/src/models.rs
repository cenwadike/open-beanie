pub use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
pub use serde::{Deserialize, Serialize};
pub use std::sync::Arc;
pub use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};
pub use tokio::sync::mpsc;

use crate::{
    Config, DualRateLimiter,
    stealth_routes::{CallDataPayload, ClientSignature},
};

/// Global application state shared across Axum route handlers.
#[derive(Clone)]
pub struct AppState {
    pub app_config: Arc<Config>,
    pub starknet_config: Arc<beanie_keeper::config::StarknetConfig>,
    pub evm_config: Arc<beanie_keeper::config::EvmConfig>,
    pub limiter: Arc<DualRateLimiter>,
    pub deploy_tx: Arc<mpsc::Sender<DeployTask>>,
    pub stealth_tx: Arc<mpsc::Sender<StealthTask>>,
    pub evm_provider: Arc<ethers::providers::Provider<ethers::providers::Http>>,
    pub starknet_provider:
        Arc<starknet::providers::JsonRpcClient<starknet::providers::jsonrpc::HttpTransport>>,
    pub reqwest_client: Arc<reqwest::Client>,
}

// ── 1. Data Models & API Schemas ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Chain {
    Base,
    Starknet,
    Ethereum,
    Solana,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployTask {
    pub chain: Chain,
    pub merchant_address: String,
    pub target_chain: Chain,
    pub webhook_url: Option<String>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentTask {
    pub chain: Chain,
    pub merchant_address: String,
    pub receiver_address: String,
    pub webhook_url: Option<String>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StealthTask {
    pub chain: Chain,
    pub tx_hash: String,
    pub derived_address: String,
    pub client_sig: ClientSignature,
    pub credential_id: String,
    pub calls: Vec<CallDataPayload>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

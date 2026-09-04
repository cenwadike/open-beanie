pub use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethers::contract::abigen;
use ethers::utils::format_bytes32_string;
use ethers::utils::keccak256;
pub use serde::{Deserialize, Serialize};
use starknet::core::types::Felt;
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
    pub stealth_tx: Arc<mpsc::Sender<StealthTask>>,
    pub payment_tx: Arc<mpsc::Sender<PaymentTask>>,
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
pub struct PaymentTask {
    pub source_chain: Chain,
    pub destination_chain: Chain,
    pub merchant_address: String,
    pub receiver_address: String,
    pub tx_hash: String,
    pub from_address: String,
    pub amount_raw: String,
    pub webhook_url: Option<String>,
    pub attempts: u32,
    pub create_if_missing: bool,
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

// WebhookJob moved to `webhook_worker.rs` to keep payment workers focused
// Webhook job type used across workers (kept in models so library modules can refer to it)
#[derive(Debug, Clone)]
pub struct WebhookJob {
    pub cfg: beanie_keeper::config::Config,
    pub webhook_url: String,
    pub deposit: beanie_keeper::config::Deposit,
    pub sweep_tx: Option<String>,
    pub max_retries: u32,
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

pub(crate) fn chain_to_bytes32(chain: Chain) -> [u8; 32] {
    let name = match chain {
        Chain::Base => "BASE",
        Chain::Ethereum => "ETHEREUM",
        Chain::Starknet => "STARKNET",
        Chain::Solana => "SOLANA",
    };

    format_bytes32_string(name).expect("Chain name fits in bytes32")
}

pub(crate) fn chain_to_felt(chain: Chain) -> Felt {
    match chain {
        Chain::Base => Felt::from_hex(&hex::encode("BASE")).unwrap(),
        Chain::Ethereum => Felt::from_hex(&hex::encode("ETHEREUM")).unwrap(),
        Chain::Solana => Felt::from_hex(&hex::encode("SOLANA")).unwrap(),
        Chain::Starknet => Felt::from_hex(&hex::encode("STARKNET")).unwrap(),
    }
}

pub(crate) fn derive_felt_from_foreign_address(addr: &str) -> Felt {
    let hash = keccak256(addr.as_bytes());
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(&hash[12..32]);
    Felt::from_bytes_be(&buf)
}

abigen!(
    MerchantFactory,
    r#"[
        function registerMerchant(address merchant, bytes32 cctpMintChain, bytes32 cctpMintRecipient) external returns (address)
        function getReceiverCount(address merchant) external view returns (uint256)
    ]"#;
    MerchantWebhookRegistry,
    r#"[
        function setWebhookUrl(address merchant, string calldata url) external
    ]"#;
);

abigen!(
    ChainXReceiverLocal,
    r#"[
        function sweep() external returns (uint256 net, uint256 feeToCaller, uint256 feeToTreasury, uint256 fee)
        function initialized() external view returns (bool)
    ]"#;
);

abigen!(
    Multicall3,
    r#"[]"#;
);

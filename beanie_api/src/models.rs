pub use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethers::utils::keccak256;
use ethers::{contract::abigen, types::H256};
pub use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use starknet::core::types::Felt;
pub use std::net::SocketAddr;
pub use std::sync::Arc;
pub use tokio::sync::mpsc;

use crate::{
    Config,
    auth::{AuthState, RateLimiter},
    stealth_routes::{CallDataPayload, ClientSignature},
};

/// Global application state shared across Axum route handlers.
#[derive(Clone)]
pub struct AppState {
    pub auth: Arc<AuthState>,
    pub app_config: Arc<Config>,
    pub starknet_config: Arc<beanie_keeper::config::StarknetConfig>,
    pub evm_config: Arc<beanie_keeper::config::EvmConfig>,
    pub solana_config: Arc<beanie_keeper::config::SolanaConfig>,
    pub limiter: Arc<RateLimiter>,
    pub announce_tx: Arc<mpsc::Sender<AnnounceTask>>,
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
    Arbitrum,
    Monad,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvmAuth {
    pub nonce: H256,
    pub valid_after: u64,
    pub valid_before: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StarknetAuth {
    pub outside_execution: OutsideExecutionDto,
    pub signature: Vec<String>,
    pub user_address: String,
}

/// Everything needed to complete and submit the payer's gasless SPL-token
/// transfer. `message` is the exact bytes the payer signed — Beanie's
/// keeper must not rebuild or alter it, only add its own fee-payer
/// signature, or `signature` no longer verifies against what's broadcast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaAuth {
    /// Base64-encoded, bincode-serialized `solana_sdk::message::Message`,
    /// exactly as compiled client-side (fee payer = Beanie's keeper at
    /// account index 0, one SPL Token `Transfer`/`TransferChecked`
    /// instruction).
    pub message: String,
    /// Base64-encoded ed25519 signature the payer produced over `message`.
    pub signature: String,
    /// Base58 pubkey of the payer / token transfer authority. Must match
    /// `from_address` on the surrounding `IncomingPaymentRequest`.
    pub owner: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutsideExecutionDto {
    pub caller: String,
    pub nonce: String,
    pub execute_after: u64,
    pub execute_before: u64,
    pub calls: Vec<CallDto>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CallDto {
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    pub entrypoint: String,
    pub calldata: Vec<String>,
}

/// Chain-agnostic on purpose. Solana does NOT get extra fields here: the
/// `receiver` keypair its on-chain `announce_merchant` wants is a disposable
/// signer generated and discarded by the worker (see
/// `tests/solana_beanie.ts`'s `prepare()` and `create_workers.rs`'s Solana
/// arm), never something the client produces or that needs to survive past
/// the worker's own function call. `merchant_address` here is that stable
/// merchant identity for every chain, Solana included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnounceTask {
    pub chain: Chain,
    pub merchant_address: String,
    pub credential_id: String,
    pub target_chain: Chain,
    pub target_recipient: String,
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
    pub evm_auth: Option<EvmAuth>,
    pub starknet_auth: Option<StarknetAuth>,
    pub solana_auth: Option<SolanaAuth>,
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

pub fn derive_felt_from_foreign_address(addr: &str) -> Felt {
    let hash = keccak256(addr.as_bytes());
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(&hash[12..32]);
    Felt::from_bytes_be(&buf)
}

pub fn derive_pubkey_from_foreign_address(address: &str) -> solana_sdk::pubkey::Pubkey {
    let mut hasher = Sha256::new();
    hasher.update(address.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    solana_sdk::pubkey::Pubkey::new_from_array(hash)
}

abigen!(
    ReceiverFactory,
    r#"[
        function registerMerchant(address merchant, bytes32 cctpMintChain, bytes32 cctpMintRecipient) external returns (address)
        function getReceiverCount(address merchant) external view returns (uint256)
        function announceReceiver(address merchant, bytes32 cctpMintChain, bytes32 cctpMintRecipient) external
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

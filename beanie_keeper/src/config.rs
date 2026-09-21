use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ethers::signers::Signer;
use ethers::{signers::LocalWallet as EvmLocalWallet, types::Address};

use starknet::core::types::Felt;
use starknet::signers::{LocalWallet as StarknetLocalWallet, SigningKey};

// ── Unified Config Enum ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Config {
    Evm(EvmConfig),
    Starknet(StarknetConfig),
}

impl Config {
    pub fn chain_name(&self) -> &str {
        match self {
            Self::Evm(cfg) => &cfg.chain_name,
            Self::Starknet(cfg) => &cfg.chain_name,
        }
    }

    pub fn token_address_str(&self) -> String {
        match self {
            Self::Evm(cfg) => format!("{:?}", cfg.token_address),
            Self::Starknet(cfg) => format!("{:#x}", cfg.token_address),
        }
    }

    pub fn keeper_address_str(&self) -> String {
        match self {
            Self::Evm(cfg) => format!("{:?}", cfg.keeper_wallet.address()),
            Self::Starknet(cfg) => format!("{:#x}", cfg.keeper_address),
        }
    }

    pub fn signature_scheme(&self) -> &'static str {
        match self {
            Self::Evm(_) => "eip191",
            Self::Starknet(_) => "starknet-poseidon",
        }
    }
}

impl From<EvmConfig> for Config {
    fn from(cfg: EvmConfig) -> Self {
        Self::Evm(cfg)
    }
}

impl From<StarknetConfig> for Config {
    fn from(cfg: StarknetConfig) -> Self {
        Self::Starknet(cfg)
    }
}

// ── StarknetConfig ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StarknetConfig {
    pub rpc_url: String,
    pub chain_name: String,
    pub token_address: Felt,
    pub factory_address: Felt,
    pub keeper_address: Felt,
    pub keeper_wallet: StarknetLocalWallet,
    pub registry_start_block: u64,
    pub deposit_start_block: u64,
    pub webhook_registry_start_block: u64,
    pub poll_interval: Duration,
    pub starknet_events_rpc_url: String,
    pub starknet_events_api_key: Option<String>,
}

impl StarknetConfig {
    pub fn from_env() -> Result<Self> {
        let priv_key_hex =
            env("STARKNET_SWEEP_PRIVATE_KEY").context("missing STARKNET_SWEEP_PRIVATE_KEY")?;
        let signer_scalar = Felt::from_hex(&priv_key_hex)
            .context("invalid STARKNET_SWEEP_PRIVATE_KEY hex string")?;

        let wallet = StarknetLocalWallet::from(SigningKey::from_secret_scalar(signer_scalar));

        Ok(Self {
            rpc_url: env("STARKNET_RPC_URL").context("missing STARKNET_RPC_URL")?,
            chain_name: "starknet".into(),
            token_address: parse_felt_env("STARKNET_TOKEN_ADDRESS")?,
            factory_address: parse_felt_env("STARKNET_FACTORY_ADDRESS")?,
            keeper_address: parse_felt_env("STARKNET_KEEPER_ADDRESS")?,
            keeper_wallet: wallet,
            registry_start_block: env("STARKNET_REGISTRY_START_BLOCK")?
                .parse()
                .context("invalid START_BLOCK")?,
            deposit_start_block: env("STARKNET_DEPOSIT_START_BLOCK")?
                .parse()
                .context("invalid STARKNET_DEPOSIT_START_BLOCK")?,
            webhook_registry_start_block: env("STARKNET_WEBHOOK_REGISTRY_START_BLOCK")?
                .parse()
                .context("invalid REGISTRY_START_BLOCK")?,
            poll_interval: Duration::from_secs(
                env("STARKNET_POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(12),
            ),
            starknet_events_rpc_url: env("STARKNET_EVENT_STREAM_URL")
                .context("missing STARKNET_EVENT_STREAM_URL")?,
            starknet_events_api_key: env("STARKNET_EVENT_API_KEY").ok(),
        })
    }
}

fn parse_felt_env(var_name: &str) -> Result<Felt> {
    let raw = env(var_name).with_context(|| format!("missing env var {var_name}"))?;
    Felt::from_hex(&raw).with_context(|| format!("invalid felt for {var_name}: {raw}"))
}

// ── EvmConfig ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EvmConfig {
    pub evm_rpc_url: String,
    // Optional now: only feeds the newHeads-only push in evm_ws.rs, which
    // is itself an optional minor optimization (free base_fee_per_gas per
    // block) rather than something anything downstream depends on. `None`
    // means fee calculation always falls back to `estimate_eip1559_fees`.
    // See evm_ws.rs's module doc for what did and didn't survive Phase 2.
    pub ws_url: Option<String>,
    pub chain_name: String, // "base" | "ethereum" — carried into the webhook payload
    pub token_address: Address, // the stablecoin ERC20 contract
    pub factory_address: Address, // Beanie's EVM MerchantFactory
    pub registry_start_block: u64, // block MerchantFactory was deployed at
    pub deposit_start_block: u64, // deposit-scan watermark
    pub webhook_registry_address: Address, // MerchantWebhookRegistry
    pub webhook_registry_start_block: u64, // block MerchantWebhookRegistry was deployed at
    // Same keypair as sweep_private_key, parsed once here without a chain ID (message
    // signing via EIP-191 personal_sign doesn't need one — only tx signing does).
    // Merchants verify webhooks by recovering the signer address from the signature and
    // checking it matches this wallet's address — the same address they already see as
    // the `from` on every sweep tx. One identity, nothing separate to publish.
    pub keeper_wallet: EvmLocalWallet,
    pub poll_interval: Duration,

    // ── Subsquid Portal (event discovery — see evm_subsquid.rs) ──────────
    //
    // Replaces `backfill_rpc_url` (a second, Etherscan-backed explorer
    // backfill path that only ran if `etherscan_api_key` happened to be
    // set, with the live RPC path carrying the full cold-start backlog
    // against `evm_rpc_url`'s quota otherwise) and the whole
    // adaptive-bisection / CdpBudget / RPC-chunked eth_getLogs discovery
    // path in evm_keeper.rs. There is exactly one data source for event
    // discovery now and it is not optional — unlike the old Etherscan
    // key, `subsquid_portal_url` is required, because there's no
    // "fall back to the live RPC path instead" option left: `evm_rpc_url`
    // above is used exclusively for state reads and sending transactions.
    //
    // Base URL is `https://portal.sqd.dev/datasets/{network}` — see
    // evm_subsquid.rs's module doc for the exact request/response shape.
    pub subsquid_portal_url: String,
    pub subsquid_portal_api_key: String,
}

impl EvmConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            evm_rpc_url: env("BASE_RPC_URL")?,
            ws_url: env("BASE_WS_URL").ok(),
            chain_name: "base".into(),
            token_address: addr(&env("BASE_TOKEN_ADDRESS")?)?,
            factory_address: addr(&env("BASE_FACTORY_ADDRESS")?)?,
            registry_start_block: env("BASE_REGISTRY_START_BLOCK")?
                .parse()
                .context("BASE_REGISTRY_START_BLOCK must be a valid u64")?,
            webhook_registry_address: addr(&env("BASE_WEBHOOK_REGISTRY_ADDRESS")?)?,
            webhook_registry_start_block: env("BASE_WEBHOOK_REGISTRY_START_BLOCK")?
                .parse()
                .context("BASE_WEBHOOK_REGISTRY_START_BLOCK must be a valid u64")?,
            keeper_wallet: load_keeper_wallet(&env("BASE_SWEEP_PRIVATE_KEY")?)?,
            poll_interval: Duration::from_secs(
                env("BASE_POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(12),
            ),
            deposit_start_block: env("BASE_DEPOSIT_START_BLOCK")?
                .parse()
                .context("BASE_DEPOSIT_START_BLOCK must be a valid u64")?,
            subsquid_portal_url: env("BASE_SUBSQUID_PORTAL_URL").context(
                "missing BASE_SUBSQUID_PORTAL_URL (e.g. https://portal.sqd.dev/datasets/base-mainnet)",
            )?,
            subsquid_portal_api_key: env("BASE_SUBSQUID_PORTAL_API_KEY").context(
                "missing BASE_SUBSQUID_PORTAL_API_KEY (Get one from https://portal.sqd.dev/)",
            )?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Deposit {
    pub tx_hash: String,
    pub from_address: String,
    pub receiver: String, // which merchant's ChainXReceiver clone this landed in
    pub amount_raw: String, // U256 as decimal string — avoids precision loss, wider than Solana's u64
    pub block_number: u64,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}

fn addr(s: &str) -> Result<Address> {
    s.parse::<Address>()
        .with_context(|| format!("invalid address: {s}"))
}

/// WEBHOOK_ED25519_SEED_HEX is gone — same key as BASE_SWEEP_PRIVATE_KEY, parsed without a
/// chain ID since personal_sign message signing doesn't bind to one.
fn load_keeper_wallet(hex_key: &str) -> Result<EvmLocalWallet> {
    hex_key
        .parse::<EvmLocalWallet>()
        .context("BASE_SWEEP_PRIVATE_KEY is not a valid private key")
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_formatted() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

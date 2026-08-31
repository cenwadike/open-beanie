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

// ── EvmConfig ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StarknetConfig {
    pub rpc_url: String,
    pub chain_name: String,
    pub token_address: Felt,
    pub factory_address: Felt,
    pub webhook_registry_address: Felt,
    pub keeper_address: Felt,
    pub keeper_wallet: StarknetLocalWallet,
    pub poll_interval: Duration,
    pub start_block: u64,
    pub registry_start_block: u64,
    pub webhook_registry_start_block: u64,
    pub log_chunk_blocks: u64,
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
            webhook_registry_address: parse_felt_env("WEBHOOK_REGISTRY_ADDRESS")?,
            keeper_address: parse_felt_env("STARKNET_KEEPER_ADDRESS")?,
            keeper_wallet: wallet,
            poll_interval: Duration::from_secs(
                env("POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(12),
            ),
            start_block: env("STARKNET_START_BLOCK")?
                .parse()
                .context("invalid START_BLOCK")?,
            registry_start_block: env("STARKNET_START_BLOCK")?
                .parse()
                .context("invalid REGISTRY_START_BLOCK")?,
            webhook_registry_start_block: env("WEBHOOK_REGISTRY_START_BLOCK")?
                .parse()
                .context("invalid WEBHOOK_REGISTRY_START_BLOCK")?,
            log_chunk_blocks: env("LOG_CHUNK_BLOCKS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1000),
        })
    }
}

fn parse_felt_env(var_name: &str) -> Result<Felt> {
    let raw = env(var_name).with_context(|| format!("missing env var {var_name}"))?;
    Felt::from_hex(&raw).with_context(|| format!("invalid felt for {var_name}: {raw}"))
}

#[derive(Debug, Clone)]
pub struct EvmConfig {
    pub rpc_url: String,
    pub chain_name: String, // "base" | "ethereum" — carried into the webhook payload
    pub token_address: Address, // the stablecoin ERC20 contract
    pub factory_address: Address, // Beanie's EVM MerchantFactory
    pub registry_start_block: u64, // block MerchantFactory was deployed at
    pub webhook_registry_address: Address, // MerchantWebhookRegistry
    pub webhook_registry_start_block: u64, // block MerchantWebhookRegistry was deployed at
    // Same keypair as sweep_private_key, parsed once here without a chain ID (message
    // signing via EIP-191 personal_sign doesn't need one — only tx signing does).
    // Merchants verify webhooks by recovering the signer address from the signature and
    // checking it matches this wallet's address — the same address they already see as
    // the `from` on every sweep tx. One identity, nothing separate to publish.
    pub keeper_wallet: EvmLocalWallet,
    pub poll_interval: Duration,
    pub start_block: u64,      // deposit-scan watermark
    pub log_chunk_blocks: u64, // most RPC providers cap eth_getLogs to a block range — confirm the real limit for your provider before trusting this
}

impl EvmConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            rpc_url: env("BASE_RPC_URL")?,
            chain_name: "base".into(),
            token_address: addr(&env("BASE_TOKEN_ADDRESS")?)?,
            factory_address: addr(&env("BASE_FACTORY_ADDRESS")?)?,
            registry_start_block: env("REGISTRY_START_BLOCK")?
                .parse()
                .context("REGISTRY_START_BLOCK must be a valid u64")?,
            webhook_registry_address: addr(&env("WEBHOOK_REGISTRY_ADDRESS")?)?,
            webhook_registry_start_block: env("WEBHOOK_REGISTRY_START_BLOCK")?
                .parse()
                .context("WEBHOOK_REGISTRY_START_BLOCK must be a valid u64")?,
            keeper_wallet: load_keeper_wallet(&env("BASE_SWEEP_PRIVATE_KEY")?)?,
            poll_interval: Duration::from_secs(
                env("POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(12), // ~1 EVM block on most L2s; tune per chain
            ),
            start_block: env("BASE_START_BLOCK")?
                .parse()
                .context("BASE_START_BLOCK must be a valid u64")?,
            log_chunk_blocks: env("LOG_CHUNK_BLOCKS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2000),
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

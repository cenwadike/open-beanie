use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ethers::{signers::LocalWallet, types::Address};

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
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
    pub keeper_wallet: LocalWallet,
    pub poll_interval: Duration,
    pub start_block: u64,      // deposit-scan watermark
    pub log_chunk_blocks: u64, // most RPC providers cap eth_getLogs to a block range — confirm the real limit for your provider before trusting this
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            rpc_url: env("RPC_URL")?,
            chain_name: env("CHAIN_NAME")?,
            token_address: addr(&env("TOKEN_ADDRESS")?)?,
            factory_address: addr(&env("FACTORY_ADDRESS")?)?,
            registry_start_block: env("REGISTRY_START_BLOCK")?
                .parse()
                .context("REGISTRY_START_BLOCK must be a valid u64")?,
            webhook_registry_address: addr(&env("WEBHOOK_REGISTRY_ADDRESS")?)?,
            webhook_registry_start_block: env("WEBHOOK_REGISTRY_START_BLOCK")?
                .parse()
                .context("WEBHOOK_REGISTRY_START_BLOCK must be a valid u64")?,
            keeper_wallet: load_keeper_wallet(&env("SWEEP_PRIVATE_KEY")?)?,
            poll_interval: Duration::from_secs(
                env("POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(12), // ~1 EVM block on most L2s; tune per chain
            ),
            start_block: env("START_BLOCK")?
                .parse()
                .context("START_BLOCK must be a valid u64")?,
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

/// WEBHOOK_ED25519_SEED_HEX is gone — same key as SWEEP_PRIVATE_KEY, parsed without a
/// chain ID since personal_sign message signing doesn't bind to one.
fn load_keeper_wallet(hex_key: &str) -> Result<LocalWallet> {
    hex_key
        .parse::<LocalWallet>()
        .context("SWEEP_PRIVATE_KEY is not a valid private key")
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

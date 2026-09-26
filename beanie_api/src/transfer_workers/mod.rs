//! Native transfer (deposit-sweep) workers.
//!
//! Split from a single `transfer_workers.rs` into one file per chain
//! (`evm`, `starknet`, `solana`) plus this thin orchestrator. Each
//! submodule owns its full pipeline — historical catch-up, live-tip
//! handling, and its own reconciliation backstop — independently of the
//! other two.
//!
//! `RECONCILE_EVERY` is the one piece genuinely shared by all three and
//! lives in `common`. Everything else — types, helpers, constants,
//! vendor integration — is private to its own chain's file; nothing is
//! shared between them beyond what's passed in as arguments.

mod common;
mod evm;
mod solana;
mod starknet;

use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient as SolanaRpcClient;
use solana_sdk::signature::Keypair as SolanaKeypair;
use tokio::sync::mpsc;

use beanie_keeper::log_cache::LogCache;
use beanie_keeper::starknet_keeper::StarknetAccount;

/// Starts all three chain workers concurrently and never returns. Each one
/// runs its own catch-up retry loop, live-tip loop, and reconciliation
/// ticker independently — a failure or slow backfill on one chain never
/// blocks or delays the others.
pub async fn run_native_transfer_poller(
    evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    solana_rpc: Arc<SolanaRpcClient>,
    solana_keeper_wallet: Arc<SolanaKeypair>,
    evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    solana_cfg: Arc<beanie_keeper::config::SolanaConfig>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    log::info!("Transfer worker starting");

    let cache_path =
        std::env::var("LOG_CACHE_PATH").unwrap_or_else(|_| ".beanie-chain-log-cache".to_string());
    let log_cache =
        Arc::new(LogCache::open(cache_path).expect("failed to open persistent log cache"));

    tokio::join!(
        evm::run_evm_worker(evm_client, evm_cfg, log_cache.clone(), webhook_tx.clone()),
        starknet::run_starknet_worker(
            starknet_account,
            starknet_cfg,
            log_cache.clone(),
            webhook_tx.clone()
        ),
        solana::run_solana_worker(
            solana_rpc,
            solana_keeper_wallet,
            solana_cfg,
            log_cache,
            webhook_tx
        ),
    );
}

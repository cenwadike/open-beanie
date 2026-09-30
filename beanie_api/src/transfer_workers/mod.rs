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
//!
//! `evm.rs` itself is chain-agnostic (Base, Arbitrum, or any other
//! EVM-compatible chain run through the exact same pipeline via its own
//! `EvmConfig`/`SignerProvider` pair) — see `evm_chains` below. Each EVM
//! chain gets its own `run_evm_worker` instance, its own Subsquid Portal
//! client + rate-limit bucket (`evm_indexer::portal`, keyed per
//! `subsquid_portal_url`), and its own chain-scoped `LogCache`
//! checkpoints (`evm_indexer::registry_webhook_scan_id`/`deposits_scan_id`,
//! keyed per `cfg.chain_name`) — so adding a chain here never risks one
//! chain's progress or pacing colliding with another's.

pub mod common;
pub mod evm;
pub mod solana;
pub mod starknet;

use std::sync::Arc;

use futures_util::future::join_all;
use solana_client::nonblocking::rpc_client::RpcClient as SolanaRpcClient;
use solana_sdk::signature::Keypair as SolanaKeypair;
use tokio::sync::mpsc;

use beanie_keeper::log_cache::LogCache;
use beanie_keeper::starknet_keeper::StarknetAccount;

use crate::models::Chain;
pub(crate) use evm::SharedEvmRegistry;
pub(crate) use solana::SharedSolanaRegistry;
pub(crate) use starknet::SharedStarknetRegistry;

/// Starts every EVM chain's worker, plus Starknet's and Solana's,
/// concurrently, and never returns. Each one runs its own catch-up retry
/// loop, live-tip loop, and reconciliation ticker independently — a
/// failure or slow backfill on any single chain never blocks or delays
/// any other, EVM chains included: `evm_chains` may hold one entry
/// (Base only) or several (Base, Arbitrum, ...) and every entry runs
/// concurrently with every other, same as the EVM/Starknet/Solana split
/// already does.
pub(crate) async fn run_native_transfer_poller(
    // `Chain` tagged onto each entry (in addition to its config, which
    // already carries `chain_name` as a display string) so this same list
    // can also carry that chain's registry handle — `payment_workers.rs`
    // needs to pick the right one by `Chain`, not by string, since that's
    // what `PaymentTask::source_chain` already is.
    evm_chains: Vec<(
        Chain,
        Arc<beanie_keeper::evm_keeper::SignerProvider>,
        Arc<beanie_keeper::config::EvmConfig>,
        SharedEvmRegistry,
    )>,
    base_evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    base_evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_account: Arc<StarknetAccount>,
    solana_rpc: Arc<SolanaRpcClient>,
    solana_keeper_wallet: Arc<SolanaKeypair>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    solana_cfg: Arc<beanie_keeper::config::SolanaConfig>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
    starknet_registry: SharedStarknetRegistry,
    solana_registry: SharedSolanaRegistry,
) {
    log::info!(
        "Transfer worker starting ({} EVM chain(s): {})",
        evm_chains.len(),
        evm_chains
            .iter()
            .map(|(_, _, cfg, _)| cfg.chain_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let cache_path =
        std::env::var("LOG_CACHE_PATH").unwrap_or_else(|_| ".beanie-chain-log-cache".to_string());
    let log_cache =
        Arc::new(LogCache::open(cache_path).expect("failed to open persistent log cache"));

    // One `run_evm_worker` future per configured EVM chain, run
    // concurrently via `join_all` rather than `tokio::join!` since the
    // count isn't known until runtime (1 chain today, N once Ethereum,
    // etc. are added to `main.rs`'s `evm_chains` vec).
    let evm_futures = evm_chains
        .into_iter()
        .map(|(chain, evm_client, evm_cfg, evm_registry)| {
            let chain_name = evm_cfg.chain_name.clone();
            let log_cache = log_cache.clone();
            let webhook_tx = webhook_tx.clone();
            async move {
                evm::run_evm_worker(evm_client, evm_cfg, log_cache, webhook_tx, evm_registry).await;
                // run_evm_worker runs an unconditional `loop {}` internally and
                // is not expected to return under normal operation — this only
                // fires if that ever changes (e.g. a future refactor adds an
                // early-return error path) and is here so that case isn't silent.
                log::error!("EVM worker for chain '{chain_name}' ({chain:?}) exited unexpectedly");
            }
        });

    tokio::join!(
        join_all(evm_futures),
        starknet::run_starknet_worker(
            starknet_account,
            starknet_cfg,
            base_evm_client.clone(),
            base_evm_cfg.clone(),
            log_cache.clone(),
            webhook_tx.clone(),
            starknet_registry,
        ),
        solana::run_solana_worker(
            solana_rpc,
            solana_keeper_wallet,
            solana_cfg,
            base_evm_client,
            base_evm_cfg,
            log_cache,
            webhook_tx,
            solana_registry,
        ),
    );
}

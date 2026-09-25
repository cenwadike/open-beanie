//! src/solana_ws.rs
//!
//! Live tips over the same archive-gRPC vendor connection `solana_indexer.rs`
//! uses for catch-up (one vendor covers both — see that module's doc comment
//! for why raw Yellowstone-replay depth was rejected for this role). Thin by
//! design, same spirit as `starknet_ws.rs`: its only job is "tell the caller
//! something changed," not to decode or act on it — `process_solana_tip` in
//! transfer_workers.rs does the real work via solana_indexer.rs, same as
//! every other chain.

use log::warn;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub struct SolanaTip {
    pub slot: u64,
    pub registry_activity: bool, // program logged a MerchantAnnounced/Registered this slot
    pub deposit_activity: bool,  // a tracked receiver ATA saw a transfer this slot
}

/// Vendor-specific push subscription. Implemented against whichever
/// archive-gRPC client is chosen — see `solana_indexer::SolanaEventSource`
/// for the same abstraction boundary on the catch-up side. Reconnects with
/// the same 1s -> 60s exponential backoff as `evm_ws.rs`'s Portal stream.
#[async_trait::async_trait]
pub trait SolanaTipSource: Send + Sync {
    async fn subscribe_once(
        &self,
        program_id: &Pubkey,
        mint: &Pubkey,
        tracked_receiver_tas: &[Pubkey],
        tips_tx: &mpsc::Sender<SolanaTip>,
    ) -> anyhow::Result<()>;
}

pub async fn run_solana_subscription(
    source: std::sync::Arc<dyn SolanaTipSource>,
    program_id: Pubkey,
    mint: Pubkey,
    tracked_receiver_tas: std::sync::Arc<tokio::sync::RwLock<Vec<Pubkey>>>,
    tips_tx: mpsc::Sender<SolanaTip>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let receiver_tas = tracked_receiver_tas.read().await.clone();
        let result = source
            .subscribe_once(&program_id, &mint, &receiver_tas, &tips_tx)
            .await;

        match result {
            Ok(()) => backoff = Duration::from_secs(1), // clean end (shutdown) or reconnectable gap
            Err(e) => {
                warn!("solana gRPC stream disconnected: {e:#}. Reconnecting in {backoff:?}...");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

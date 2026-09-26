//! src/solana_ws.rs
//!
//! Live tips over the same Subsquid Portal connection solana_indexer.rs
//! uses for catch-up. Thin by design, same spirit as evm_ws.rs /
//! starknet_ws.rs: its only job is "tell the caller something changed" —
//! `process_solana_tip` in transfer_workers.rs does the real work via
//! solana_indexer.rs.

use anyhow::{Context, Result, bail};
use log::warn;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

use crate::config::SolanaConfig;

#[derive(Debug, Clone, Copy)]
pub struct SolanaTip {
    pub slot: u64,
    pub registry_activity: bool, // program logged a MerchantAnnounced/Registered this slot
    pub deposit_activity: bool,  // a tracked receiver ATA saw a transfer this slot
}

const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

#[derive(Deserialize)]
struct PortalBlock {
    header: PortalHeader,
    #[serde(default)]
    logs: Vec<serde_json::Value>,
    #[serde(default)]
    instructions: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct PortalHeader {
    number: u64,
}

/// Polls Subsquid Portal's /stream from wherever the last block left off,
/// same posture as evm_ws.rs's run_evm_subscription. Reconnects with the
/// same 1s -> 60s exponential backoff as every other tip source here.
pub async fn run_solana_subscription(
    cfg: Arc<SolanaConfig>,
    program_id: Pubkey,
    tracked_receiver_tas: Arc<RwLock<Vec<Pubkey>>>,
    tips_tx: mpsc::Sender<SolanaTip>,
) {
    let mut current_slot = crate::solana_indexer::current_slot(&cfg)
        .await
        .unwrap_or(cfg.deposit_start_slot.max(cfg.registry_start_slot));
    let mut backoff = Duration::from_secs(1);

    loop {
        let receiver_tas = tracked_receiver_tas.read().await.clone();
        let slot_before = current_slot;

        let result = stream_solana_tips_once(
            &cfg,
            &program_id,
            &receiver_tas,
            &mut current_slot,
            &tips_tx,
        )
        .await;

        match result {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                if current_slot == slot_before {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Err(e) => {
                warn!("solana Portal stream disconnected: {e:#}. Reconnecting in {backoff:?}...");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn stream_solana_tips_once(
    cfg: &SolanaConfig,
    program_id: &Pubkey,
    tracked_receiver_tas: &[Pubkey],
    current_slot: &mut u64,
    tips_tx: &mpsc::Sender<SolanaTip>,
) -> Result<()> {
    let program_str = program_id.to_string();
    let dest_strs: Vec<String> = tracked_receiver_tas.iter().map(|p| p.to_string()).collect();

    let instructions_filter = if dest_strs.is_empty() {
        serde_json::json!([])
    } else {
        serde_json::json!([
            { "programId": [SPL_TOKEN_PROGRAM], "d1": ["0x03"], "a1": dest_strs.clone() },
            { "programId": [SPL_TOKEN_PROGRAM], "d1": ["0x0c"], "a1": dest_strs }
        ])
    };

    let body = serde_json::json!({
        "type": "solana",
        "fromBlock": *current_slot,
        "fields": {
            "block": { "number": true },
            "log": { "programId": true, "kind": true },
            "instruction": { "accounts": true, "d1": true }
        },
        "logs": [{ "programId": [program_str], "kind": ["data"] }],
        "instructions": instructions_filter
    });

    let client = reqwest::Client::builder()
        .tcp_keepalive(Duration::from_secs(15))
        .build()?;

    let url = format!("{}/stream", cfg.subsquid_portal_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("failed initiating Portal Solana stream")?;

    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Portal Solana stream HTTP {status}: {text}");
    }

    let text = resp
        .text()
        .await
        .context("failed reading Portal Solana stream body")?;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let block: PortalBlock = match serde_json::from_str(line) {
            Ok(b) => b,
            Err(e) => {
                warn!("undecodable Portal NDJSON line: {e}\nline: {line}");
                continue;
            }
        };

        let slot = block.header.number;
        let registry_activity = !block.logs.is_empty();
        let deposit_activity = !block.instructions.is_empty();

        *current_slot = slot + 1;

        if registry_activity || deposit_activity {
            let tip = SolanaTip {
                slot,
                registry_activity,
                deposit_activity,
            };
            if tips_tx.send(tip).await.is_err() {
                return Ok(()); // worker shutdown
            }
        }
    }

    Ok(())
}

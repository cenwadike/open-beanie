//! Solana native transfer worker.
//!
//! NOTE — half-integrated: this worker's cross-chain webhook lookup is a
//! hardcoded empty map (`empty_webhook_map` below), the same gap that
//! exists on the Starknet side. Solana webhooks will not fire until that's
//! wired up to the real `MerchantWebhookRegistry` state that currently
//! only lives inside the EVM worker's own `EvmState`. That's not new here
//! — it's the pre-existing gap, just made explicit per-chain instead of
//! silently inherited.
//!
//! Pipeline: historical catch-up (retried until it succeeds) -> live
//! Subsquid Portal tip loop (one vendor covers both registry and deposit
//! push activity, unlike Starknet's plain poll) -> periodic reconciliation
//! backstop that also re-checks every known receiver's balance directly.
//!
//! Registration is NOT part of the atomic sweep multicall here, unlike EVM
//! and Starknet: `reg_tx` is pre-signed and broadcasts standalone (see
//! `solana_keeper::broadcast_pending_registration`'s doc comment). A
//! receiver promoted from `Announced` to `Registered` in one pass is only
//! swept on the *next* call.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result as AnyhowResult;
use log::{debug, error, info};
use solana_client::nonblocking::rpc_client::RpcClient as SolanaRpcClient;
use solana_sdk::pubkey::Pubkey as SolanaPubkey;
use solana_sdk::signature::Keypair as SolanaKeypair;
use spl_associated_token_account::get_associated_token_address;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};

use beanie_keeper::log_cache::LogCache;
use beanie_keeper::solana_indexer::{self, ReceiverStatus, SolanaReceiverRecord};
use beanie_keeper::solana_keeper;
use beanie_keeper::solana_ws::{self, SolanaTip};

use super::common::RECONCILE_EVERY;

struct SolanaState {
    /// Keyed by `receiver`, not `merchant` — a merchant can have up to 32
    /// receivers (`MAX_RECEIVERS_PER_MERCHANT`), same one-to-many shape as
    /// EVM/Starknet's registries.
    merchant_map: HashMap<SolanaPubkey, SolanaReceiverRecord>,
    /// merchant_token_account is always ATA(merchant, mint) — derivable
    /// with no RPC round trip, so this is a memoization, not a fetch cache.
    merchant_ta_cache: HashMap<SolanaPubkey, SolanaPubkey>,
}

/// Never let a `MerchantAnnounced` merge downgrade an already-`Registered`
/// receiver, and never drop the pinned `reg_tx` until it's actually been
/// consumed — see `solana_indexer.rs`'s module doc for why Announced is
/// tracked at all (JIT registration).
fn remember_solana_receiver(
    map: &mut HashMap<SolanaPubkey, SolanaReceiverRecord>,
    rec: SolanaReceiverRecord,
) {
    match map.get(&rec.receiver) {
        Some(existing) if existing.status == ReceiverStatus::Registered => {
            // Already registered — an announce arriving late (or replayed
            // by the reconciliation backstop) must not undo that.
        }
        _ => {
            map.insert(rec.receiver, rec);
        }
    }
}

/// Boxes a `pending_registration` account read as a `solana_indexer::BoxFuture`
/// — shared by both the startup catch-up path and the live-tip path so
/// there's exactly one place that shape gets built.
fn box_fetch_reg_tx(
    rpc: Arc<SolanaRpcClient>,
) -> impl FnMut(SolanaPubkey) -> solana_indexer::BoxFuture<AnyhowResult<Vec<u8>>> {
    move |pending_pda: SolanaPubkey| {
        let rpc = rpc.clone();
        Box::pin(async move {
            let acct = rpc.get_account(&pending_pda).await.map_err(|e| {
                anyhow::anyhow!("failed fetching pending_registration account: {e}")
            })?;
            // PendingRegistration layout: 8-byte Anchor discriminator +
            // 1-byte bump + 4-byte Vec<u8> length prefix, then reg_tx.
            anyhow::ensure!(
                acct.data.len() > 13,
                "pending_registration account too short"
            );
            Ok(acct.data[13..].to_vec())
        })
    }
}

pub(super) async fn run_solana_worker(
    solana_rpc: Arc<SolanaRpcClient>,
    solana_keeper_wallet: Arc<SolanaKeypair>,
    solana_cfg: Arc<beanie_keeper::config::SolanaConfig>,
    log_cache: Arc<LogCache>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    let mut state = SolanaState {
        merchant_map: HashMap::new(),
        merchant_ta_cache: HashMap::new(),
    };

    // Same gap as Starknet's `empty_webhook_map`: cross-chain webhook
    // lookup against Base's real `MerchantWebhookRegistry` isn't wired
    // between workers yet (EVM's is only populated inside its own
    // `EvmState`). Until that's shared across tasks, Solana webhooks
    // won't fire either — not a new gap introduced here, the existing one
    // extended consistently rather than silently patched over.
    let empty_webhook_map: HashMap<String, String> = HashMap::new();

    // 1. Run Solana historical backfill. Retried until it succeeds, same
    // reasoning as EVM: the live loop needs the FULL receiver set or every
    // tip's deposit scan silently no-ops.
    let mut retry_delay = Duration::from_secs(5);
    let summary = loop {
        let fetch_reg_tx = box_fetch_reg_tx(solana_rpc.clone());

        match solana_indexer::run_solana_catchup(&solana_cfg, &log_cache, fetch_reg_tx).await {
            Ok(summary) => break summary,
            Err(e) => {
                error!("Solana startup catch-up failed: {e:#} — retrying in {retry_delay:?}");
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            }
        }
    };

    for rec in summary.receivers {
        remember_solana_receiver(&mut state.merchant_map, rec);
    }
    info!(
        "Solana worker state ready: {} receiver(s)",
        state.merchant_map.len()
    );

    act_on_solana_deposits(
        &solana_rpc,
        &solana_keeper_wallet,
        &solana_cfg,
        &webhook_tx,
        &empty_webhook_map,
        &mut state,
        summary.deposits,
    )
    .await;

    // 2. Start the live Subsquid Portal subscription — one vendor covers
    // both registry and deposit activity (see solana_ws.rs doc comment).
    let (solana_tips_tx, mut solana_tips_rx) = mpsc::channel::<SolanaTip>(16);
    let tracked_receiver_tas = Arc::new(tokio::sync::RwLock::new(
        state
            .merchant_map
            .values()
            .map(|r| r.receiver_token_account)
            .collect::<Vec<_>>(),
    ));
    tokio::spawn(solana_ws::run_solana_subscription(
        solana_cfg.clone(),
        solana_cfg.program_id,
        tracked_receiver_tas.clone(),
        solana_tips_tx,
    ));

    let mut reconcile_ticker = interval_at(Instant::now() + RECONCILE_EVERY, RECONCILE_EVERY);

    // 3. Independent Solana loop
    loop {
        tokio::select! {
            Some(tip) = solana_tips_rx.recv() => {
                process_solana_tip(
                    &solana_rpc,
                    &solana_keeper_wallet,
                    &solana_cfg,
                    &log_cache,
                    &webhook_tx,
                    &empty_webhook_map,
                    &mut state,
                    tip,
                ).await;
                // Keep the live subscription's watch-list current — a
                // receiver folded in during this tip's registry scan needs
                // to be watched on the NEXT stream reconnect. Cheap: a
                // Vec clone behind a lock, not a network call.
                let mut watched = tracked_receiver_tas.write().await;
                *watched = state.merchant_map.values().map(|r| r.receiver_token_account).collect();
            }
                _ = reconcile_ticker.tick() => {
                if let Ok(head) = solana_rpc.get_slot().await {
                    process_solana_tip(
                        &solana_rpc,
                        &solana_keeper_wallet,
                        &solana_cfg,
                        &log_cache,
                        &webhook_tx,
                        &empty_webhook_map,
                        &mut state,
                        SolanaTip { slot: head, registry_activity: true, deposit_activity: true },
                    ).await;

                    let all_receivers: Vec<SolanaReceiverRecord> =
                        state.merchant_map.values().cloned().collect();
                    if !all_receivers.is_empty() {
                        sweep_solana_receivers(
                            &solana_rpc,
                            &solana_keeper_wallet,
                            &solana_cfg,
                            &mut state,
                            all_receivers,
                        ).await;
                    }
                }
            }
        }
    }
}

async fn process_solana_tip(
    solana_rpc: &Arc<SolanaRpcClient>,
    solana_keeper_wallet: &Arc<SolanaKeypair>,
    solana_cfg: &Arc<beanie_keeper::config::SolanaConfig>,
    log_cache: &LogCache,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    webhook_map: &HashMap<String, String>,
    state: &mut SolanaState,
    tip: SolanaTip,
) {
    let tip_slot = tip.slot;

    // --- Registry scan, same watermark+backlog+activity-flag shape as
    // EVM's process_evm_tip. -------------------------------------------
    let registry_checkpoint = match log_cache.get_checkpoint(solana_indexer::REGISTRY_SCAN_ID) {
        Ok(cp) => cp,
        Err(e) => {
            error!(
                "failed reading solana registry checkpoint: {e:#} — treating as no progress yet"
            );
            None
        }
    };
    let registry_watermark = registry_checkpoint
        .map(|s| s + 1)
        .unwrap_or(solana_cfg.registry_start_slot);
    let has_backlog = registry_watermark < tip_slot;
    let registry_due = registry_watermark <= tip_slot && (tip.registry_activity || has_backlog);

    if registry_due {
        match solana_indexer::fetch_program_events(
            solana_cfg,
            log_cache,
            registry_watermark,
            tip_slot,
        )
        .await
        {
            Ok((events, last_seen)) => {
                for ev in &events {
                    if ev.name == "MerchantRegistered" {
                        if let Ok(raw) = solana_indexer::decode_merchant_registered(&ev.data) {
                            remember_solana_receiver(
                                &mut state.merchant_map,
                                SolanaReceiverRecord {
                                    merchant: raw.merchant,
                                    receiver: raw.receiver,
                                    receiver_token_account: raw.receiver_token_account,
                                    receiver_config: raw.receiver_config,
                                    status: ReceiverStatus::Registered,
                                    reg_tx: None,
                                },
                            );
                        }
                    }
                }

                let fetch_reg_tx = box_fetch_reg_tx(solana_rpc.clone());
                solana_indexer::attach_reg_tx_and_merge(
                    &mut state.merchant_map,
                    &events,
                    &solana_cfg.program_id,
                    &solana_cfg.mint,
                    fetch_reg_tx,
                )
                .await;

                if let Some(seen) = last_seen {
                    if let Err(e) = log_cache.set_checkpoint(solana_indexer::REGISTRY_SCAN_ID, seen)
                    {
                        error!("failed advancing solana registry checkpoint to {seen}: {e:#}");
                    }
                }
            }
            Err(e) => error!("solana fetch_program_events failed: {e:#}"),
        }
    } else if registry_watermark == tip_slot {
        if let Err(e) = log_cache.set_checkpoint(solana_indexer::REGISTRY_SCAN_ID, tip_slot) {
            error!("failed advancing solana registry checkpoint to {tip_slot}: {e:#}");
        }
    }

    if state.merchant_map.is_empty() {
        debug!("solana tip {tip_slot}: no receivers known, skipping deposit scan");
        return;
    }

    // --- Deposits: Announced ∪ Registered — see solana_indexer.rs module
    // doc for why an unregistered receiver still needs to be watched -------
    let receiver_tas: Vec<SolanaPubkey> = state
        .merchant_map
        .values()
        .map(|r| r.receiver_token_account)
        .collect();

    let deposits = match solana_indexer::fetch_deposits_since_slot(
        solana_cfg,
        log_cache,
        &receiver_tas,
        tip_slot,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => {
            error!("solana fetch_deposits_since_slot failed: {e:#}");
            return;
        }
    };

    act_on_solana_deposits(
        solana_rpc,
        solana_keeper_wallet,
        solana_cfg,
        webhook_tx,
        webhook_map,
        state,
        deposits,
    )
    .await;
}

fn merchant_token_account(
    state: &mut SolanaState,
    mint: &SolanaPubkey,
    merchant: &SolanaPubkey,
) -> SolanaPubkey {
    *state
        .merchant_ta_cache
        .entry(*merchant)
        .or_insert_with(|| get_associated_token_address(merchant, mint))
}

/// Balance filter + JIT-register + sweep for a set of candidate receivers,
/// same "one function, called from both the deposit path and the
/// reconciliation backstop" shape as EVM's `sweep_evm_receivers`.
///
/// Unlike EVM/Starknet, registration here is NOT part of this function's
/// atomic send — `reg_tx` is pre-signed and broadcasts standalone (see
/// `solana_keeper::broadcast_pending_registration`'s doc comment). A
/// receiver promoted from Announced to Registered in this pass is only
/// swept on the *next* call.
async fn sweep_solana_receivers(
    solana_rpc: &Arc<SolanaRpcClient>,
    solana_keeper_wallet: &Arc<SolanaKeypair>,
    solana_cfg: &Arc<beanie_keeper::config::SolanaConfig>,
    state: &mut SolanaState,
    candidates: Vec<SolanaReceiverRecord>,
) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }

    let receiver_tas: Vec<SolanaPubkey> = candidates
        .iter()
        .map(|r| r.receiver_token_account)
        .collect();
    let nonzero = match solana_keeper::batch_check_nonzero_balance(solana_rpc, &receiver_tas).await
    {
        Ok(set) => set,
        Err(e) => {
            error!(
                "solana batch_check_nonzero_balance failed ({e:#}) — proceeding without the \
                 balance filter this pass rather than risk skipping a real sweep."
            );
            receiver_tas.into_iter().collect()
        }
    };

    // --- JIT-register: Announced + nonzero balance --------------------------
    let to_register: Vec<(SolanaPubkey, Vec<u8>)> = candidates
        .iter()
        .filter(|r| nonzero.contains(&r.receiver_token_account))
        .filter_map(|r| match (r.status, &r.reg_tx) {
            (ReceiverStatus::Announced, Some(reg_tx)) => Some((r.receiver, reg_tx.clone())),
            _ => None,
        })
        .collect();

    for (receiver, reg_tx) in to_register {
        match solana_keeper::broadcast_pending_registration(solana_rpc, &reg_tx).await {
            Ok(sig) => {
                info!("solana JIT register {receiver} -> {sig}");
                if let Some(rec) = state.merchant_map.get_mut(&receiver) {
                    rec.status = ReceiverStatus::Registered;
                    rec.reg_tx = None;
                }
            }
            Err(e) => error!("solana register broadcast failed for {receiver}: {e:#}"),
        }
    }

    // --- Sweep: Registered ∩ nonzero only -----------------------------------
    let sweepable: Vec<(SolanaReceiverRecord, SolanaPubkey)> = candidates
        .into_iter()
        .filter(|r| nonzero.contains(&r.receiver_token_account))
        .filter_map(|r| {
            let is_registered = state
                .merchant_map
                .get(&r.receiver)
                .map(|rec| rec.status == ReceiverStatus::Registered)
                .unwrap_or(false);
            is_registered.then(|| {
                let merchant = r.merchant;
                let merchant_ta = merchant_token_account(state, &solana_cfg.mint, &merchant);
                (r, merchant_ta)
            })
        })
        .collect();

    solana_keeper::multicall_sweep_same_chain(
        solana_rpc,
        &solana_keeper_wallet.insecure_clone(),
        solana_cfg,
        &sweepable,
    )
    .await
    .unwrap_or_else(|e| {
        error!("solana multicall_sweep_same_chain failed: {e:#}");
        None
    })
}

/// JIT-register/sweep/webhook pipeline for a batch of already-discovered
/// Solana deposits.
///
/// The webhook fires for every discovered deposit regardless of sweep
/// outcome (`sweep_tx: None` when nothing settled) — this mirrors
/// Starknet's `act_on_starknet_deposits`, NOT EVM's `act_on_evm_deposits`,
/// which withholds entirely until a sweep settles.
async fn act_on_solana_deposits(
    solana_rpc: &Arc<SolanaRpcClient>,
    solana_keeper_wallet: &Arc<SolanaKeypair>,
    solana_cfg: &Arc<beanie_keeper::config::SolanaConfig>,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    webhook_map: &HashMap<String, String>,
    state: &mut SolanaState,
    deposits: Vec<beanie_keeper::config::Deposit>,
) {
    if deposits.is_empty() {
        return;
    }

    let mut unique: Vec<SolanaPubkey> = deposits
        .iter()
        .filter_map(|d| d.receiver.parse().ok())
        .collect();
    unique.sort();
    unique.dedup();

    let candidates: Vec<SolanaReceiverRecord> = unique
        .iter()
        .filter_map(|receiver| state.merchant_map.get(receiver).cloned())
        .collect();

    let sweep_tx = sweep_solana_receivers(
        solana_rpc,
        solana_keeper_wallet,
        solana_cfg,
        state,
        candidates,
    )
    .await;

    for d in &deposits {
        let Ok(receiver) = d.receiver.parse::<SolanaPubkey>() else {
            continue;
        };
        let Some(rec) = state.merchant_map.get(&receiver) else {
            error!("no merchant known for solana receiver {receiver}");
            continue;
        };
        let webhook_key = rec.merchant.to_string();

        if let Some(url) = webhook_map.get(&webhook_key) {
            let cfg = beanie_keeper::config::Config::Solana((**solana_cfg).clone());
            let job = crate::models::WebhookJob {
                cfg,
                webhook_url: url.clone(),
                deposit: d.clone(),
                sweep_tx: sweep_tx.clone(),
                max_retries: 5,
            };
            if let Err(e) = webhook_tx.send(job).await {
                error!("failed enqueuing webhook job: {e}");
            }
        } else {
            debug!("no webhook URL for solana merchant {webhook_key}");
        }
    }
}

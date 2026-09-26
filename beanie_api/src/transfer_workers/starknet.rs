//! Starknet native transfer worker.
//!
//! Pipeline: historical catch-up -> live tip loop, debounced (see
//! `STARKNET_MIN_LIVE_SCAN_INTERVAL`) -> periodic reconciliation backstop
//! that always scans regardless of the debounce.
//!
//! Unlike EVM's push subscription (real per-block activity flags) or
//! Solana's gRPC push (same), Starknet's tip source is a plain
//! `block_number` poll with no activity signal at all — so live-tip
//! scanning here is gated purely on a minimum-interval debounce, not an
//! activity flag or backlog check. `RECONCILE_EVERY` still guarantees a
//! scan on its own cadence independent of that debounce, so a missed
//! window is bounded the same way it is on the other two chains.
//!
//! Register + sweep are sent as a single atomic multicall per batch, same
//! as EVM — unlike Solana, where registration is a separate, pre-signed,
//! standalone broadcast (see `solana.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, error, info};
use starknet::accounts::{Account, ConnectedAccount};
use starknet::core::types::{BlockId, BlockTag, Call, Felt};
use starknet::core::utils::get_selector_from_name;
use starknet::providers::Provider;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};

use beanie_keeper::log_cache::LogCache;
use beanie_keeper::starknet_indexer::{StarknetReceiverRecord, StarknetRoute, StarknetTip};
use beanie_keeper::starknet_keeper::StarknetAccount;

use super::common::RECONCILE_EVERY;

/// Minimum time between *live-tip-triggered* Starknet registry/deposit
/// scans (`discover_merchants` / `fetch_deposits_since_block`), independent
/// of how often new tips arrive.
///
/// `run_starknet_tip_source` polls `block_number` every 4s and emits a tip
/// on every new block. Unlike the EVM path — which only actually scans when
/// `process_evm_tip` sees real backlog or an activity flag — nothing here
/// tells us whether a given Starknet block is worth scanning, so without a
/// floor on frequency, every new block was firing its own
/// `starknet_getEvents` call straight at Starkscan's rate-limited
/// `rpc-beta` proxy. That's what was producing the "gateway locally
/// saturated" (-32005) and dropped-connection errors in production: a live
/// scan every few seconds, indefinitely. This debounce caps that to once
/// per interval; `RECONCILE_EVERY` still guarantees a scan on its own
/// cadence regardless of this floor, so a missed window is bounded the
/// same way it always was.
const STARKNET_MIN_LIVE_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// What the registry told us about a receiver: who it belongs to and, for
/// receivers learned from `ReceiverAnnounced`, the CCTP route that is part
/// of its address. Registration replays that route verbatim.
#[derive(Clone, Copy)]
struct StarknetReceiverInfo {
    merchant: Felt,
    route: Option<StarknetRoute>,
}

/// Never let a route-less `MerchantRegistered` row erase a route we already
/// learned from `ReceiverAnnounced`.
fn remember_starknet_receiver(
    map: &mut HashMap<Felt, StarknetReceiverInfo>,
    rec: StarknetReceiverRecord,
) {
    let entry = map.entry(rec.receiver).or_insert(StarknetReceiverInfo {
        merchant: rec.merchant,
        route: None,
    });
    entry.merchant = rec.merchant;
    if rec.route.is_some() {
        entry.route = rec.route;
    }
}

struct StarknetState {
    merchant_map: HashMap<Felt, StarknetReceiverInfo>,
    deployed: HashSet<Felt>,
    next_nonce: Option<Felt>,
    /// Wall-clock time of the last live-tip-triggered registry/deposit
    /// scan. `None` means none has happened yet this process — the first
    /// tip always scans. See `STARKNET_MIN_LIVE_SCAN_INTERVAL`.
    last_live_scan: Option<Instant>,
}

pub(super) async fn run_starknet_worker(
    starknet_account: Arc<StarknetAccount>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    log_cache: Arc<LogCache>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    let mut sn_state = StarknetState {
        merchant_map: HashMap::new(),
        deployed: HashSet::new(),
        next_nonce: None,
        last_live_scan: None,
    };

    // Placeholder map or shared cross-chain lookup if needed
    let empty_webhook_map = HashMap::new();

    // 1. Run Starknet historical backfill
    if let Ok(summary) =
        beanie_keeper::starknet_indexer::run_starknet_catchup(&starknet_cfg, &log_cache).await
    {
        for rec in &summary.merchants {
            remember_starknet_receiver(&mut sn_state.merchant_map, *rec);
        }

        act_on_starknet_deposits(
            &starknet_account,
            &starknet_cfg,
            &webhook_tx,
            &empty_webhook_map,
            &mut sn_state,
            summary.deposits,
        )
        .await;
    }

    // 2. Start Starknet Tip Source Stream
    let (sn_tips_tx, mut sn_tips_rx) = mpsc::channel::<StarknetTip>(16);
    tokio::spawn(beanie_keeper::starknet_ws::run_starknet_tip_source(
        Some(starknet_cfg.starknet_events_rpc_url.clone()),
        starknet_account.clone(),
        sn_tips_tx,
    ));

    let mut reconcile_ticker = interval_at(Instant::now() + RECONCILE_EVERY, RECONCILE_EVERY);

    // 3. Independent Starknet loop
    loop {
        tokio::select! {
            Some(tip) = sn_tips_rx.recv() => {
                process_starknet_tip(
                    &starknet_account,
                    &starknet_cfg,
                    &log_cache,
                    &webhook_tx,
                    &empty_webhook_map,
                    &mut sn_state,
                    tip,
                    false, // live tip: respect STARKNET_MIN_LIVE_SCAN_INTERVAL
                ).await;
            }
            _ = reconcile_ticker.tick() => {
                if let Ok(bn) = starknet_account.provider().block_number().await {
                    process_starknet_tip(
                        &starknet_account,
                        &starknet_cfg,
                        &log_cache,
                        &webhook_tx,
                        &empty_webhook_map,
                        &mut sn_state,
                        StarknetTip { block_number: bn },
                        true, // reconciliation backstop: always scan
                    ).await;
                }
            }
        }
    }
}

async fn process_starknet_tip(
    starknet_account: &Arc<StarknetAccount>,
    starknet_cfg: &Arc<beanie_keeper::config::StarknetConfig>,
    log_cache: &LogCache,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    webhook_map: &HashMap<String, String>,
    state: &mut StarknetState,
    tip: StarknetTip,
    force_scan: bool,
) {
    let sn_tip = tip.block_number;

    // `discover_merchants`'s checkpoint always advances to exactly the
    // block it was last called with, so on a live push of one new block
    // per tip, `current_from` lands right back at `to_block` on every
    // call — its own early-exit never fires in steady state. Without this
    // floor, every new Starknet block (as often as every few seconds) was
    // firing its own `starknet_getEvents` call. `force_scan` lets the
    // reconciliation backstop bypass this and always scan.
    let due = force_scan
        || state
            .last_live_scan
            .map(|t| t.elapsed() >= STARKNET_MIN_LIVE_SCAN_INTERVAL)
            .unwrap_or(true);

    if !due {
        return;
    }
    state.last_live_scan = Some(Instant::now());

    match beanie_keeper::starknet_indexer::discover_merchants(&**starknet_cfg, log_cache, sn_tip)
        .await
    {
        Ok(found) => {
            for rec in found {
                remember_starknet_receiver(&mut state.merchant_map, rec);
            }
        }
        Err(e) => error!("starknet discover_merchants failed: {e:#}"),
    }

    if state.merchant_map.is_empty() {
        return;
    }

    let receivers_sn: Vec<Felt> = state.merchant_map.keys().copied().collect();
    let sn_deposits = match beanie_keeper::starknet_indexer::fetch_deposits_since_block(
        &**starknet_cfg,
        log_cache,
        &receivers_sn,
        sn_tip,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => {
            error!("starknet fetch_deposits_since_block failed: {e:#}");
            return;
        }
    };

    act_on_starknet_deposits(
        starknet_account,
        starknet_cfg,
        webhook_tx,
        webhook_map,
        state,
        sn_deposits,
    )
    .await;
}

/// JIT-deploy/sweep/webhook pipeline for a batch of already-discovered
/// Starknet deposits.
async fn act_on_starknet_deposits(
    starknet_account: &Arc<StarknetAccount>,
    starknet_cfg: &Arc<beanie_keeper::config::StarknetConfig>,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    webhook_map: &HashMap<String, String>,
    state: &mut StarknetState,
    sn_deposits: Vec<beanie_keeper::config::Deposit>,
) {
    if sn_deposits.is_empty() {
        return;
    }

    let mut unique: Vec<Felt> = sn_deposits
        .iter()
        .filter_map(|d| Felt::from_hex(&d.receiver).ok())
        .collect();
    unique.sort_by(|a, b| a.to_bytes_be().cmp(&b.to_bytes_be()));
    unique.dedup();

    let register_selector = match get_selector_from_name("register_merchant") {
        Ok(s) => s,
        Err(e) => {
            error!("selector register_merchant: {e}");
            return;
        }
    };
    let sweep_selector = match get_selector_from_name("sweep") {
        Ok(s) => s,
        Err(e) => {
            error!("selector sweep: {e}");
            return;
        }
    };

    // --- Skip receivers with nothing left to sweep -------------------------
    let nonzero_balances = match beanie_keeper::starknet_keeper::batch_check_nonzero_balance(
        starknet_account.provider(),
        starknet_cfg.token_address,
        &unique,
    )
    .await
    {
        Ok(set) => Some(set),
        Err(e) => {
            error!(
                "starknet batch_check_nonzero_balance failed ({e:#}) — proceeding without \
                 the balance filter this batch rather than risk skipping a real sweep."
            );
            None
        }
    };

    let mut calls: Vec<Call> = Vec::new();
    // Same pattern as the EVM side: only folded into state.deployed once
    // execute_v3(...).send() confirms success below.
    let mut pending_deploys: Vec<Felt> = Vec::new();

    for &receiver in &unique {
        let (merchant, route) = match state.merchant_map.get(&receiver) {
            Some(info) => (info.merchant, info.route),
            None => continue,
        };

        // `None` means the batch check itself failed (see above) — don't
        // let a check we couldn't run block a real sweep.
        let has_balance = nonzero_balances
            .as_ref()
            .map(|set| set.contains(&receiver))
            .unwrap_or(true);
        if !has_balance {
            // Nothing to collect right now — skip BOTH the register and
            // sweep calls for this receiver. Deploying a receiver purely
            // to sweep a zero balance would waste exactly the gas this
            // check exists to save; it'll be picked up again if a future
            // tip finds a new Transfer event for it.
            info!(
                "skipping starknet sweep for {receiver:#x} — balanceOf reports zero (already swept, or nothing to collect yet)"
            );
            continue;
        }

        // Fix #3, Starknet side: only ask the chain once per receiver, ever.
        let needs_deploy = if state.deployed.contains(&receiver) {
            false
        } else {
            let deployed = match starknet_account
                .provider()
                .get_class_hash_at(BlockId::Tag(BlockTag::L1Accepted), receiver)
                .await
            {
                Ok(ch) => ch != Felt::ZERO,
                Err(_) => false,
            };
            if deployed {
                state.deployed.insert(receiver);
            }
            !deployed
        };

        if needs_deploy {
            // The route is part of the receiver's address, so it must be the
            // one from its ReceiverAnnounced event, never a default.
            let Some(route) = route else {
                error!("no announced route for undeployed receiver {receiver:#x}");
                continue;
            };
            calls.push(Call {
                to: starknet_cfg.factory_address,
                selector: register_selector,
                calldata: vec![
                    merchant,
                    route.chain,
                    route.recipient_low,
                    route.recipient_high,
                ],
            });
            // Do NOT mark state.deployed here — same reasoning as the EVM
            // side. execute_v3(...).send() can fail without the receiver
            // ever having been deployed, and state.deployed is a
            // permanent, never-re-checked cache (fix #3). Marking it
            // early would permanently strand the receiver on failure.
            pending_deploys.push(receiver);
        }

        calls.push(Call {
            to: receiver,
            selector: sweep_selector,
            calldata: vec![],
        });
    }

    if state.next_nonce.is_none() {
        state.next_nonce = Some(
            starknet_account
                .provider()
                .get_nonce(
                    BlockId::Tag(BlockTag::L1Accepted),
                    starknet_account.address(),
                )
                .await
                .unwrap_or(Felt::ZERO),
        );
    }
    let nonce = state.next_nonce.unwrap();

    let sweep_tx_sn = if calls.is_empty() {
        None
    } else {
        match starknet_account.execute_v3(calls).nonce(nonce).send().await {
            Ok(pending) => {
                let tx_hash = format!("{:#x}", pending.transaction_hash);
                info!("starknet native atomic register+sweep -> {tx_hash}");
                state.next_nonce = Some(nonce + Felt::ONE);
                // Send succeeded — safe to treat these receivers as
                // deployed now.
                state.deployed.extend(pending_deploys.iter().copied());
                Some(tx_hash)
            }
            Err(e) => {
                error!("starknet native atomic invoke failed: {e}");
                state.next_nonce = None; // force a fresh get_nonce() next time
                None
            }
        }
    };

    for d in &sn_deposits {
        let merchant_felt = match Felt::from_hex(&d.receiver) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let merchant_for_webhook = state
            .merchant_map
            .get(&merchant_felt)
            .map(|info| info.merchant)
            .unwrap_or(merchant_felt);

        let webhook_key = format!("{:#x}", merchant_for_webhook);

        if let Some(url) = webhook_map.get(&webhook_key) {
            let cfg = beanie_keeper::config::Config::Starknet((**starknet_cfg).clone());
            let job = crate::models::WebhookJob {
                cfg,
                webhook_url: url.clone(),
                deposit: d.clone(),
                sweep_tx: sweep_tx_sn.clone(),
                max_retries: 5,
            };
            if let Err(e) = webhook_tx.send(job).await {
                error!("failed enqueuing webhook job: {e}");
            }
        } else {
            debug!("no webhook URL for merchant {webhook_key}");
        }
    }
}

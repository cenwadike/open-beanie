//! EVM native transfer worker — one instance per EVM-compatible chain
//! (Base, Arbitrum, ...), each given its own `EvmConfig`/`SignerProvider`
//! by `transfer_workers/mod.rs`'s `evm_chains` list. Nothing in this file
//! is Base-specific; everything chain-identifying comes from the `cfg`
//! passed in.
//!
//! Pipeline: historical catch-up -> live push-tip loop (registry/webhook
//! discovery + deposit scan, combined where possible into one
//! `eth_getLogs`) -> periodic reconciliation backstop that also re-checks
//! every known receiver's on-chain balance directly (see
//! `sweep_evm_receivers`'s doc comment for why that backstop exists).
//!
//! Register + sweep are sent as a single atomic Multicall3 `aggregate3`
//! call per batch — unlike Solana, where registration is a separate,
//! pre-signed, standalone broadcast (see `solana.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result as AnyhowResult};
use ethers::abi::{ParamType, Token, decode};
use ethers::contract::abigen;
use ethers::providers::Middleware;
use ethers::types::{Address, U256};
use log::{debug, error, info};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};

use crate::models::{ChainXReceiverLocal, ReceiverFactory};
use beanie_keeper::evm_indexer::{EvmReceiverRecord, EvmRoute};
use beanie_keeper::evm_ws::{self, EvmTip};
use beanie_keeper::log_cache::LogCache;

use super::common::RECONCILE_EVERY;

abigen!(
    Multicall3,
    r#"[
        {
            "inputs": [
                {
                    "components": [
                        { "internalType": "address", "name": "target", "type": "address" },
                        { "internalType": "bool", "name": "allowFailure", "type": "bool" },
                        { "internalType": "bytes", "name": "callData", "type": "bytes" }
                    ],
                    "internalType": "struct Multicall3.Call3[]",
                    "name": "calls",
                    "type": "tuple[]"
                }
            ],
            "name": "aggregate3",
            "outputs": [
                {
                    "components": [
                        { "internalType": "bool", "name": "success", "type": "bool" },
                        { "internalType": "bytes", "name": "returnData", "type": "bytes" }
                    ],
                    "internalType": "struct Multicall3.Result[]",
                    "name": "returnData",
                    "type": "tuple[]"
                }
            ],
            "stateMutability": "payable",
            "type": "function"
        }
    ]"#
);

const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

abigen!(
    Erc20BalanceView,
    r#"[
        function balanceOf(address) external view returns (uint256)
    ]"#
);

/// Batched ERC-20 `balanceOf` check via Multicall3's `aggregate3`, called
/// as a read (`.call()` — a plain `eth_call`, no transaction broadcast, no
/// gas spent) rather than sent. One RPC round trip covers every receiver
/// in `receivers`.
async fn batch_check_evm_nonzero_balance(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    token_address: Address,
    receivers: &[Address],
) -> AnyhowResult<HashSet<Address>> {
    if receivers.is_empty() {
        return Ok(HashSet::new());
    }

    debug!("batch_check_evm_nonzero_balance: starting");

    let multicall_addr: Address = MULTICALL3_ADDRESS.parse().expect("valid multicall addr");
    let multicall = Multicall3::new(multicall_addr, evm_client.clone());
    let token = Erc20BalanceView::new(token_address, evm_client.clone());

    let mut calls = Vec::with_capacity(receivers.len());
    for &receiver in receivers {
        let call_data = token
            .balance_of(receiver)
            .calldata()
            .context("failed encoding balanceOf calldata")?;
        calls.push(Call3 {
            target: token_address,
            allow_failure: true,
            call_data,
        });
    }

    debug!("batch_check_evm_nonzero_balance: batched view call");

    let results = multicall
        .aggregate_3(calls)
        .call()
        .await
        .context("multicall aggregate3 (balanceOf batch) eth_call failed")?;

    let mut nonzero = HashSet::with_capacity(receivers.len());
    for (&receiver, result) in receivers.iter().zip(results.iter()) {
        if !result.success {
            // Reverted — can't tell if that means "zero balance" or
            // something else entirely (non-standard token, paused
            // contract, etc). Don't guess: treat as unknown, not zero.
            nonzero.insert(receiver);
            continue;
        }
        match decode(&[ParamType::Uint(256)], &result.return_data) {
            Ok(mut tokens) => match tokens.remove(0) {
                Token::Uint(balance) => {
                    if !balance.is_zero() {
                        nonzero.insert(receiver);
                    }
                }
                _ => {
                    nonzero.insert(receiver);
                }
            },
            Err(e) => {
                error!(
                    "batch_check_evm_nonzero_balance: 
                    failed decoding balanceOf return data for {receiver:?}: {e} — treating as unknown, not zero"
                );
                nonzero.insert(receiver);
            }
        }
    }

    debug!("batch_check_evm_nonzero_balance: complete");
    Ok(nonzero)
}

/// What the registry told us about a receiver: who it belongs to and, for
/// receivers learned from `ReceiverAnnounced`, the CCTP route that is part
/// of its address. Registration replays that route verbatim.
#[derive(Clone, Copy)]
struct EvmReceiverInfo {
    merchant: Address,
    route: Option<EvmRoute>,
}

/// Never let a route-less `MerchantRegistered` row erase a route we already
/// learned from `ReceiverAnnounced`.
fn remember_evm_receiver(map: &mut HashMap<Address, EvmReceiverInfo>, rec: EvmReceiverRecord) {
    let entry = map.entry(rec.receiver).or_insert(EvmReceiverInfo {
        merchant: rec.merchant,
        route: None,
    });
    entry.merchant = rec.merchant;
    if rec.route.is_some() {
        entry.route = rec.route;
    }
}

/// Shared, cross-tick state. Bundled into one struct so `process_evm_tip`
/// and the reconciliation pass operate on identical state instead of two
/// slightly-diverged copies.
struct EvmState {
    merchant_map: HashMap<Address, EvmReceiverInfo>,
    /// Fix #5: persistent across ticks, only ever added to.
    webhook_map: HashMap<String, String>,
    /// Fix #3: once true, never checked again. Only ever set once a
    /// deploy send is *confirmed* successful (or an existence check
    /// confirms it), never eagerly at call-construction time.
    deployed: HashSet<Address>,
}

pub(super) async fn run_evm_worker(
    evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    log_cache: Arc<LogCache>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    let mut state = EvmState {
        merchant_map: HashMap::new(),
        webhook_map: HashMap::new(),
        deployed: HashSet::new(),
    };

    // 1. Run EVM historical backfill. Retried until it succeeds: the live
    // loop below relies on `state.merchant_map` holding the FULL receiver set,
    // so starting it after a failed catch-up would leave every deposit
    // unmatched (empty receiver set => every tip silently returns early).
    let mut retry_delay = Duration::from_secs(5);
    let summary = loop {
        match beanie_keeper::evm_indexer::run_evm_catchup(&evm_cfg, &log_cache).await {
            Ok(summary) => break summary,
            Err(e) => {
                error!("EVM startup catch-up failed: {e:#} — retrying in {retry_delay:?}");
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            }
        }
    };

    for rec in &summary.merchants {
        remember_evm_receiver(&mut state.merchant_map, *rec);
    }
    for (merchant, url) in &summary.webhooks {
        state
            .webhook_map
            .insert(format!("{merchant:?}"), url.clone());
    }
    info!(
        "EVM worker state ready: {} receiver(s), {} webhook(s)",
        state.merchant_map.len(),
        state.webhook_map.len()
    );

    // The live stream resumes right after whatever block catch-up covered.
    let ws_start_block = summary
        .caught_up_to_block
        .map(|b| b + 1)
        .unwrap_or(evm_cfg.registry_start_block);

    let synthetic_tip = EvmTip {
        block_number: summary.caught_up_to_block.unwrap_or(0),
        base_fee_per_gas: None,
        registry_activity: false,
        webhook_activity: false,
    };

    act_on_evm_deposits(
        &evm_client,
        &evm_cfg,
        &evm_cfg.evm_rpc_url,
        &webhook_tx,
        &mut state,
        synthetic_tip,
        summary.deposits,
    )
    .await;

    // 2. Start EVM Push Subscription Stream
    let (evm_tips_tx, mut evm_tips_rx) = mpsc::channel::<EvmTip>(16);
    let factory_addr_alloy = alloy::primitives::Address::from(evm_cfg.factory_address.0);
    let webhook_addr_alloy = alloy::primitives::Address::from(evm_cfg.webhook_registry_address.0);

    tokio::spawn(beanie_keeper::evm_ws::run_evm_subscription(
        evm_cfg.clone(),
        factory_addr_alloy,
        webhook_addr_alloy,
        ws_start_block,
        evm_tips_tx,
    ));

    let mut reconcile_ticker = interval_at(Instant::now() + RECONCILE_EVERY, RECONCILE_EVERY);

    // 3. Independent EVM loop
    loop {
        tokio::select! {
            Some(tip) = evm_tips_rx.recv() => {
                process_evm_tip(
                    &evm_client,
                    &evm_cfg,
                    &log_cache,
                    &evm_cfg.evm_rpc_url,
                    &webhook_tx,
                    &mut state,
                    tip,
                ).await;
            }
            _ = reconcile_ticker.tick() => {
                if let Ok(bn) = evm_client.provider().get_block_number().await {
                    let block_number = bn.as_u64();
                    let synthetic_tip = EvmTip {
                        block_number,
                        base_fee_per_gas: None,
                        registry_activity: true,
                        webhook_activity: true,
                    };
                    process_evm_tip(
                        &evm_client,
                        &evm_cfg,
                        &log_cache,
                        &evm_cfg.evm_rpc_url,
                        &webhook_tx,
                        &mut state,
                        synthetic_tip,
                    ).await;

                    // Balance-driven sweep backstop. `process_evm_tip` above
                    // only re-attempts a sweep for deposits it re-discovers
                    // from the log checkpoint — a deposit whose block is
                    // already checkpointed is never re-surfaced, even if its
                    // earlier sweep attempt failed (e.g. a transient RPC/gas
                    // error). The receiver's on-chain balance stays nonzero
                    // regardless, so re-check every KNOWN receiver's balance
                    // here and re-sweep anything still holding funds.
                    // Deliberately no separate "pending sweep" set: the
                    // chain's own balanceOf is already the source of truth,
                    // so this just asks it directly every reconcile tick.
                    let all_receivers: Vec<Address> = state.merchant_map.keys().copied().collect();
                    if !all_receivers.is_empty() {
                        let sweep_tip = EvmTip {
                            block_number,
                            base_fee_per_gas: None,
                            registry_activity: true,
                            webhook_activity: true,
                        };
                        sweep_evm_receivers(
                            &evm_client,
                            &evm_cfg,
                            &evm_cfg.evm_rpc_url,
                            &mut state,
                            sweep_tip,
                            all_receivers,
                        ).await;
                        // No webhook here: this pass has no Deposit record
                        // to attach a notification to (deposit results
                        // aren't persisted — see log_cache.rs). A receiver
                        // swept only via this backstop won't produce a
                        // settlement webhook for its original transfer.
                    }
                }
            }
        }
    }
}

async fn process_evm_tip(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    evm_cfg: &Arc<beanie_keeper::config::EvmConfig>,
    log_cache: &LogCache,
    evm_http_rpc_url: &str,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    state: &mut EvmState,
    tip: EvmTip,
) {
    let tip_bn = tip.block_number;

    // --- Registry + webhook scan, combined into one eth_getLogs call ------
    // `tip.registry_activity` / `tip.webhook_activity` only reflect whether
    // the relevant contract logged anything in THIS single block (see
    // evm_ws.rs — reset to false after every tip). They say nothing about
    // blocks further back. If a watermark is more than one block behind
    // the tip — on startup, or after a WS reconnect gap — trusting the
    // flag alone would let us skip a whole unscanned range and mark it
    // "empty" forever. Only rely on the flag when there's no backlog to
    // lose; otherwise scan for real.
    let registry_webhook_checkpoint = match log_cache.get_checkpoint(
        &beanie_keeper::evm_indexer::registry_webhook_scan_id(&**evm_cfg),
    ) {
        Ok(cp) => cp,
        Err(e) => {
            error!(
                "failed reading registry/webhook checkpoint: {e:#} — treating as no progress yet"
            );
            None
        }
    };
    let registry_webhook_watermark =
        registry_webhook_checkpoint
            .map(|b| b + 1)
            .unwrap_or_else(|| {
                std::cmp::min(
                    evm_cfg.registry_start_block,
                    evm_cfg.webhook_registry_start_block,
                )
            });
    let has_backlog = registry_webhook_watermark < tip_bn;
    let registry_webhook_due = registry_webhook_watermark <= tip_bn
        && (tip.registry_activity || tip.webhook_activity || has_backlog);

    if registry_webhook_due {
        match beanie_keeper::evm_indexer::discover_registry_activity(&**evm_cfg, log_cache, tip_bn)
            .await
        {
            Ok((found_merchants, found_webhooks)) => {
                for rec in found_merchants {
                    remember_evm_receiver(&mut state.merchant_map, rec);
                }
                for (merchant, url) in found_webhooks {
                    // Fix #5: insert into the persistent map, never reset it.
                    state.webhook_map.insert(format!("{merchant:?}"), url);
                }
                // No local watermark to advance: discover_registry_activity
                // already committed its own checkpoint per-chunk, including
                // on a partial failure (it returns Ok with whatever it
                // found so far and an honest checkpoint — see its doc
                // comment). The next tip re-reads that checkpoint fresh, so
                // a partial failure is retried on the very next push
                // notification instead of silently waiting for the
                // 5-minute reconciliation backstop.
            }
            Err(e) => error!("discover_registry_activity failed: {e:#}"),
        }
    } else if registry_webhook_watermark == tip_bn {
        // The stream already reported no factory/webhook-registry logs in this
        // exact block and there's no gap behind it, so it's safe to mark scanned.
        if let Err(e) = log_cache.set_checkpoint(
            &beanie_keeper::evm_indexer::registry_webhook_scan_id(&**evm_cfg),
            tip_bn,
        ) {
            error!("failed advancing registry/webhook checkpoint to {tip_bn}: {e:#}");
        }
    }
    // If it wasn't due, there's nothing to advance: no backlog + no
    // activity flag means the cache checkpoint is already caught up to
    // tip_bn - 1 with nothing new in tip_bn itself, and the next tip will
    // re-derive the same conclusion from the checkpoint directly.

    // --- Deposits: still checked every tip (dynamic receiver set) ---------
    let receivers: Vec<Address> = state.merchant_map.keys().copied().collect();
    if receivers.is_empty() {
        debug!("evm tip {tip_bn}: no receivers in merchant_map, skipping deposit scan");
        return;
    }
    let deposit_watermark =
        match log_cache.get_checkpoint(&beanie_keeper::evm_indexer::deposits_scan_id(&**evm_cfg)) {
            Ok(cp) => cp,
            Err(e) => {
                error!("failed reading deposit checkpoint: {e:#} — treating as no progress yet");
                None
            }
        }
        .map(|b| b + 1)
        .unwrap_or(evm_cfg.deposit_start_block);
    if deposit_watermark > tip_bn {
        return;
    }

    let deposits = match beanie_keeper::evm_indexer::fetch_deposits_since_block(
        &**evm_cfg, log_cache, &receivers, tip_bn,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => {
            error!("fetch_deposits_since_block failed: {e}");
            return;
        }
    };

    // No local deposit_watermark to set on the empty-result path either:
    // fetch_deposits_since_block already checkpointed (per chunk, honestly
    // reflecting a partial failure if one occurred) before returning.
    // act_on_evm_deposits below no-ops on an empty Vec, same as
    // act_on_starknet_deposits does — no need to duplicate the check here.
    act_on_evm_deposits(
        evm_client,
        evm_cfg,
        evm_http_rpc_url,
        webhook_tx,
        state,
        tip,
        deposits,
    )
    .await;
}

/// Runs the balance filter + deploy-check + sweep multicall for a set of
/// candidate receivers, independent of *how* those receivers were
/// identified — a fresh deposit scan (`act_on_evm_deposits`) or a
/// reconciliation balance sweep (`run_evm_worker`'s reconcile tick) both
/// call into this one function. That matters because `state.deployed` is a
/// permanent, never-re-checked cache (fix #3): there must be exactly one
/// place that builds register/sweep calls and folds a receiver into it, or
/// the two callers would drift.
///
/// Returns the sweep tx hash if a multicall was sent and confirmed
/// on-chain, or `None` if there was nothing to sweep or the send failed.
async fn sweep_evm_receivers(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    evm_cfg: &Arc<beanie_keeper::config::EvmConfig>,
    evm_http_rpc_url: &str,
    state: &mut EvmState,
    tip: EvmTip,
    candidates: Vec<Address>,
) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }

    let mut unique = candidates;
    unique.sort();
    unique.dedup();

    // --- Skip receivers with nothing left to sweep --------------------------
    let unique: Vec<Address> = match batch_check_evm_nonzero_balance(
        evm_client,
        evm_cfg.token_address,
        &unique,
    )
    .await
    {
        Ok(nonzero) => {
            let before = unique.len();
            let filtered: Vec<Address> =
                unique.into_iter().filter(|a| nonzero.contains(a)).collect();
            if filtered.len() < before {
                info!(
                    "skipping {} receiver(s) this pass — balanceOf reports zero (already swept, or nothing to collect yet)",
                    before - filtered.len()
                );
            }
            filtered
        }
        Err(e) => {
            error!(
                "batch_check_evm_nonzero_balance failed ({e:#}) — proceeding without the \
                 balance filter this pass rather than risk skipping a real sweep."
            );
            unique
        }
    };

    if unique.is_empty() {
        return None;
    }

    // --- Existence check: cache first, batch the rest ----------------------
    let unknown: Vec<Address> = unique
        .iter()
        .copied()
        .filter(|a| !state.deployed.contains(a))
        .collect();

    if !unknown.is_empty() {
        match evm_ws::batch_check_deployed(evm_http_rpc_url, &unknown).await {
            Ok(results) => {
                for (addr, exists) in results {
                    if exists {
                        state.deployed.insert(addr);
                    }
                }
            }
            Err(e) => {
                // Batched check failed outright (e.g. provider doesn't
                // support batching over this transport) — fall back to the
                // original one-by-one check for just this pass rather than
                // silently treating every receiver as undeployed.
                error!("batched existence check failed, falling back to per-call: {e}");
                for &addr in &unknown {
                    match evm_client.provider().get_code(addr, None).await {
                        Ok(code) if !code.0.is_empty() => {
                            state.deployed.insert(addr);
                        }
                        Ok(_) => {}
                        Err(e) => error!("get_code failed for {addr:?}: {e}"),
                    }
                }
            }
        }
    }

    // --- Build the atomic register(if needed) + sweep multicall -----------
    let mut calls: Vec<Call3> = Vec::new();
    // Receivers we're attempting to register in THIS batch. Only folded
    // into state.deployed once send_evm_multicall confirms success — see
    // note below on why marking it eagerly was unsafe.
    let mut pending_deploys: Vec<Address> = Vec::new();

    for &receiver_addr in &unique {
        let exists = state.deployed.contains(&receiver_addr);

        if !exists {
            let (merchant, route) = match state.merchant_map.get(&receiver_addr) {
                Some(info) => (info.merchant, info.route),
                None => {
                    error!("no merchant mapping for undeployed receiver {receiver_addr:?}");
                    continue;
                }
            };
            // The route is part of the receiver's address, so it must be the
            // one from its ReceiverAnnounced event, never a default.
            let Some(route) = route else {
                error!("no announced route for undeployed receiver {receiver_addr:?}");
                continue;
            };
            let cctp_chain_bytes = route.chain;
            let recipient_bytes = route.recipient;

            let reg_call = ReceiverFactory::new(evm_cfg.factory_address, evm_client.clone())
                .register_merchant(merchant, cctp_chain_bytes, recipient_bytes);

            match reg_call.calldata() {
                Some(bytes) => {
                    calls.push(Call3 {
                        target: evm_cfg.factory_address,
                        allow_failure: false,
                        call_data: bytes,
                    });
                    // Do NOT mark state.deployed here. send_evm_multicall
                    // can fail before ever broadcasting (agg.send() err),
                    // return Ok(None) (tx dropped from mempool), or time
                    // out — none of which are on-chain reverts, but all of
                    // which leave the receiver genuinely undeployed. Since
                    // state.deployed is a permanent cache that's never
                    // re-checked (fix #3), marking it early on any of
                    // those paths would permanently strand this receiver:
                    // every future pass would build sweep-only calls
                    // against a contract that doesn't exist, forever.
                    pending_deploys.push(receiver_addr);
                }
                None => {
                    error!("failed encoding registerMerchant for {receiver_addr:?}");
                    continue;
                }
            }
        }

        let receiver_contract = ChainXReceiverLocal::new(receiver_addr, evm_client.clone());
        match receiver_contract.sweep().calldata() {
            Some(bytes) => {
                calls.push(Call3 {
                    target: receiver_addr,
                    allow_failure: false,
                    call_data: bytes,
                });
            }
            None => error!("failed encoding sweep for {receiver_addr:?}"),
        }
    }

    let sweep_tx = if calls.is_empty() {
        None
    } else {
        send_evm_multicall(evm_client, tip, calls).await
    };

    // Only now — once we know the multicall actually landed — is it safe
    // to treat these receivers as deployed.
    if sweep_tx.is_some() {
        state.deployed.extend(pending_deploys);
    }

    sweep_tx
}

/// JIT-deploy/sweep/webhook pipeline for a batch of already-discovered EVM
/// deposits.
async fn act_on_evm_deposits(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    evm_cfg: &Arc<beanie_keeper::config::EvmConfig>,
    evm_http_rpc_url: &str,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    state: &mut EvmState,
    tip: EvmTip,
    deposits: Vec<beanie_keeper::config::Deposit>,
) {
    if deposits.is_empty() {
        return;
    }

    let candidates: Vec<Address> = deposits
        .iter()
        .filter_map(|d| d.receiver.parse::<Address>().ok())
        .collect();

    let sweep_tx = sweep_evm_receivers(
        evm_client,
        evm_cfg,
        evm_http_rpc_url,
        state,
        tip,
        candidates,
    )
    .await;

    // Webhooks only fire once a multicall actually settled the sweep — "a
    // transfer was made, and THIS transaction settled it." If nothing
    // settled here (RPC/send error, dropped tx, or the balance filter came
    // back zero), withhold the notification rather than sending one with no
    // tx behind it. The receiver's balance, if still nonzero, gets picked
    // back up by the reconciliation backstop in run_evm_worker — no
    // separate "pending deposit" set to maintain, since balanceOf is
    // already the source of truth. The one accepted trade-off: if
    // reconciliation is what eventually sweeps it, that later sweep has no
    // Deposit record to attach, so no webhook fires for it either. Flagging
    // this in case you'd rather synthesize one from the observed balance.
    let Some(sweep_tx) = sweep_tx else {
        debug!(
            "no sweep tx settled for this batch of {} deposit(s) — webhook(s) withheld, \
             reconciliation will retry the underlying balance",
            deposits.len()
        );
        return;
    };

    for deposit in &deposits {
        let merchant = state
            .merchant_map
            .get(&deposit.receiver.parse().unwrap_or_default());
        let merchant_addr = match merchant {
            Some(info) => &info.merchant,
            None => {
                error!("no merchant known for receiver {}", deposit.receiver);
                continue;
            }
        };
        let webhook_key = format!("{merchant_addr:?}");
        if let Some(url) = state.webhook_map.get(&webhook_key) {
            let cfg = beanie_keeper::config::Config::Evm((**evm_cfg).clone());
            let job = crate::models::WebhookJob {
                cfg,
                webhook_url: url.clone(),
                deposit: deposit.clone(),
                sweep_tx: Some(sweep_tx.clone()),
                max_retries: 5,
            };
            if let Err(e) = webhook_tx.send(job).await {
                error!("failed enqueuing webhook job: {e}");
            }
        } else {
            // Webhook URLs are optional per merchant — only those who
            // registered one get notified. Nothing wrong here.
            debug!(
                "merchant {} has no webhook url for deposit {} — skipping notification",
                merchant_addr, deposit.tx_hash
            );
        }
    }
}

/// Computes (max_fee_per_gas, max_priority_fee_per_gas) for a sweep tx.
///
/// This always asks the provider what it currently considers a correct
/// priority fee via `estimate_eip1559_fees`, rather than a hardcoded
/// constant — "correct" priority fee is provider/explorer-dependent and
/// drifts with mainnet conditions, and a fixed floor is exactly what
/// caused the `-32003 max priority fee per gas higher than max fee per
/// gas` rejections: a constant 0.05 gwei priority floor stopped being
/// satisfiable the moment `base_fee * 2` (Base's fees run near-zero) dipped
/// below it. `max_fee` is then built as base_fee-derived-headroom PLUS the
/// priority fee, so `max_fee >= priority_fee` holds by construction — not
/// by clamping two independently-computed numbers against each other after
/// the fact, which is what let them invert last time.
async fn compute_sweep_fees(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    tip_base_fee: Option<u64>,
) -> (U256, U256) {
    let (rpc_max_fee, rpc_priority_fee) = match evm_client.estimate_eip1559_fees(None).await {
        Ok(pair) => pair,
        Err(e) => {
            // No RPC-suggested numbers to work with at all. Fall back to a
            // minimal fee pair where max_fee == priority_fee — this can
            // never trigger the max<priority rejection, even though it may
            // end up underpriced and simply not get included, which is a
            // safe failure mode (retried next tip/reconciliation pass).
            error!(
                "fee estimation failed: {e:#} — using a minimal max_fee == priority_fee pair \
                 so the send can't be rejected as inverted, even if it ends up underpriced"
            );
            let minimal: U256 = ethers::utils::parse_units("0.001", "gwei")
                .expect("valid gwei literal")
                .into();
            return (minimal, minimal);
        }
    };

    let max_fee = match tip_base_fee {
        // Prefer the tip's own block-header base fee when available
        // (already in hand, saves an RPC round trip) but always ADD the
        // RPC-suggested priority fee on top rather than comparing the two
        // independently — this is what guarantees max_fee >= priority_fee.
        Some(base_fee) => std::cmp::max(U256::from(base_fee) * 2 + rpc_priority_fee, rpc_max_fee),
        None => rpc_max_fee,
    };

    (max_fee, rpc_priority_fee)
}

/// Sends the Multicall3 aggregate3 call, using the block-header base fee
/// from the push tip when available (fixes item #4) and falling back to
/// `estimate_eip1559_fees` for the priority fee in all cases (see
/// `compute_sweep_fees`).
async fn send_evm_multicall(
    evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    tip: EvmTip,
    calls: Vec<Call3>,
) -> Option<String> {
    let multicall_addr: Address = MULTICALL3_ADDRESS.parse().expect("valid multicall addr");
    let multicall = Multicall3::new(multicall_addr, evm_client.clone());
    let mut agg = multicall.aggregate_3(calls);

    let (max_fee, priority_fee) = compute_sweep_fees(evm_client, tip.base_fee_per_gas).await;

    if let Some(eip1559_req) = agg.tx.as_eip1559_mut() {
        eip1559_req.max_priority_fee_per_gas = Some(priority_fee);
        eip1559_req.max_fee_per_gas = Some(max_fee);
    }

    match agg.send().await {
        Ok(pending) => match pending.await {
            Ok(Some(receipt)) => {
                let tx_hash = format!("{:?}", receipt.transaction_hash);
                info!("native atomic register+sweep -> {tx_hash}");
                Some(tx_hash)
            }
            Ok(None) => {
                error!("native multicall dropped");
                None
            }
            Err(e) => {
                error!("native multicall failed: {e}");
                None
            }
        },
        Err(e) => {
            error!("failed sending native multicall: {e}");
            None
        }
    }
}

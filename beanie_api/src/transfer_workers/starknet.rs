//! Starknet native transfer worker.
//!
//! Pipeline: historical catch-up -> live tip loop, debounced (see
//! `STARKNET_MIN_LIVE_SCAN_INTERVAL`) -> periodic reconciliation backstop
//! that always scans regardless of the debounce.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, error, info};
use starknet::accounts::{Account, ConnectedAccount};
use starknet::core::types::{BlockId, BlockTag, Call, Felt, StarknetError};
use starknet::core::utils::get_selector_from_name;
use starknet::providers::{Provider, ProviderError};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};

use beanie_keeper::log_cache::LogCache;
use beanie_keeper::starknet_indexer::{StarknetReceiverRecord, StarknetRoute, StarknetTip};
use beanie_keeper::starknet_keeper::StarknetAccount;

use super::common::RECONCILE_EVERY;

const STARKNET_MIN_LIVE_SCAN_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(crate) struct StarknetReceiverInfo {
    pub(crate) merchant: Felt,
    pub(crate) route: Option<StarknetRoute>,
}

pub(crate) type SharedStarknetRegistry = Arc<AsyncRwLock<HashMap<Felt, StarknetReceiverInfo>>>;

async fn publish_starknet_registry(state: &StarknetState, shared: &SharedStarknetRegistry) {
    *shared.write().await = state.merchant_map.clone();
}

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
    last_live_scan: Option<Instant>,
}

pub(super) async fn run_starknet_worker(
    starknet_account: Arc<StarknetAccount>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    base_evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    base_evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    log_cache: Arc<LogCache>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
    starknet_registry: SharedStarknetRegistry,
) {
    let mut sn_state = StarknetState {
        merchant_map: HashMap::new(),
        deployed: HashSet::new(),
        next_nonce: None,
        last_live_scan: None,
    };

    let empty_webhook_map = HashMap::new();

    // 1. Run Starknet historical backfill
    if let Ok(summary) =
        beanie_keeper::starknet_indexer::run_starknet_catchup(&starknet_cfg, &log_cache).await
    {
        for rec in &summary.merchants {
            remember_starknet_receiver(&mut sn_state.merchant_map, *rec);
        }
        publish_starknet_registry(&sn_state, &starknet_registry).await;

        act_on_starknet_deposits(
            &starknet_account,
            &starknet_cfg,
            &base_evm_client,
            &base_evm_cfg,
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
                    &base_evm_client,
                    &base_evm_cfg,
                    &log_cache,
                    &webhook_tx,
                    &empty_webhook_map,
                    &mut sn_state,
                    tip,
                    false,
                    &starknet_registry,
                ).await;
            }
            _ = reconcile_ticker.tick() => {
                if let Ok(bn) = starknet_account.provider().block_number().await {
                    process_starknet_tip(
                        &starknet_account,
                        &starknet_cfg,
                        &base_evm_client,
                        &base_evm_cfg,
                        &log_cache,
                        &webhook_tx,
                        &empty_webhook_map,
                        &mut sn_state,
                        StarknetTip { block_number: bn },
                        true,
                        &starknet_registry,
                    ).await;
                }
            }
        }
    }
}

async fn process_starknet_tip(
    starknet_account: &Arc<StarknetAccount>,
    starknet_cfg: &Arc<beanie_keeper::config::StarknetConfig>,
    base_evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    base_evm_cfg: &Arc<beanie_keeper::config::EvmConfig>,
    log_cache: &LogCache,
    webhook_tx: &Arc<mpsc::Sender<crate::models::WebhookJob>>,
    webhook_map: &HashMap<String, String>,
    state: &mut StarknetState,
    tip: StarknetTip,
    force_scan: bool,
    starknet_registry: &SharedStarknetRegistry,
) {
    let sn_tip = tip.block_number;

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
    publish_starknet_registry(state, starknet_registry).await;

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
        base_evm_client,
        base_evm_cfg,
        webhook_tx,
        webhook_map,
        state,
        sn_deposits,
    )
    .await;
}

async fn act_on_starknet_deposits(
    starknet_account: &Arc<StarknetAccount>,
    starknet_cfg: &Arc<beanie_keeper::config::StarknetConfig>,
    base_evm_client: &Arc<beanie_keeper::evm_keeper::SignerProvider>,
    base_evm_cfg: &Arc<beanie_keeper::config::EvmConfig>,
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
    let mut pending_deploys: Vec<Felt> = Vec::new();
    let mut updated_webhooks: std::collections::HashSet<Felt> = std::collections::HashSet::new();

    for &receiver in &unique {
        let (merchant, route) = match state.merchant_map.get(&receiver) {
            Some(info) => (info.merchant, info.route),
            None => continue,
        };

        let has_balance = nonzero_balances
            .as_ref()
            .map(|set| set.contains(&receiver))
            .unwrap_or(true);
        if !has_balance {
            info!(
                "skipping starknet sweep for {receiver:#x} — balanceOf reports zero (already swept, or nothing to collect yet)"
            );
            continue;
        }

        let needs_deploy = if state.deployed.contains(&receiver) {
            false
        } else {
            match starknet_account
                .provider()
                .get_class_hash_at(BlockId::Tag(BlockTag::L1Accepted), receiver)
                .await
            {
                Ok(ch) if ch != Felt::ZERO => {
                    state.deployed.insert(receiver);
                    false
                }
                Ok(_) => true,
                Err(ProviderError::StarknetError(StarknetError::ContractNotFound)) => true,
                Err(e) => {
                    error!(
                        "get_class_hash_at failed for {receiver:#x}: {e} — deployment unknown, skipping this pass (balance stays nonzero, reconciliation retries)"
                    );
                    continue;
                }
            }
        };

        if needs_deploy {
            let Some(route) = route else {
                error!("no announced route for undeployed receiver {receiver:#x}");
                continue;
            };

            // 1. Update webhook URL on Base contract if receiver is not yet registered/deployed
            if !updated_webhooks.contains(&merchant) {
                let merchant_key = format!("{:#x}", merchant);
                if let Some(url) = webhook_map.get(&merchant_key) {
                    let merchant_eth_addr: ethers::types::Address = merchant_key
                        .parse()
                        .unwrap_or_else(|_| ethers::types::Address::zero());

                    if merchant_eth_addr != ethers::types::Address::zero() {
                        let webhook_contract = crate::models::WebhookRegistry::new(
                            base_evm_cfg.webhook_registry_address,
                            base_evm_client.clone(),
                        );
                        match webhook_contract
                            .set_webhook_url(merchant_eth_addr, url.clone())
                            .send()
                            .await
                        {
                            Ok(_) => {
                                info!(
                                    "Updated webhook URL on Base for Starknet merchant {merchant_key}"
                                );
                                updated_webhooks.insert(merchant);
                            }
                            Err(e) => {
                                error!(
                                    "Failed to set webhook URL on Base for Starknet merchant {merchant_key}: {e:#}"
                                );
                            }
                        }
                    }
                }
            }

            // 2. Queue Starknet registration call
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
                    BlockId::Tag(BlockTag::PreConfirmed),
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
                state.deployed.extend(pending_deploys.iter().copied());
                Some(tx_hash)
            }
            Err(e) => {
                error!("starknet native atomic invoke failed: {e}");
                state.next_nonce = None;
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

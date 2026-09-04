use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use ethers::types::Address;
use ethers::utils::keccak256;
use starknet::core::types::Felt;

use crate::payment_workers::EvmSignerProvider;
use ethers::providers::Middleware;
use starknet::accounts::ConnectedAccount;
use starknet::providers::Provider;

/// Polls for native/ERC20 transfers to known receivers and triggers multicall sweeps
/// and webhook delivery. This re-uses the `beanie_keeper::evm_keeper` helpers.
pub async fn run_native_transfer_poller(
    evm_client: Arc<EvmSignerProvider>,
    evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    use std::collections::HashMap;

    println!("Native transfer poller starting...");

    // Build a keeper-compatible client (uses RetryClient) to call beanie_keeper helpers
    let keeper_client = match beanie_keeper::evm_keeper::build_client(&*evm_cfg).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed building keeper client for native poller: {e}");
            // retry later
            sleep(Duration::from_secs(5)).await;
            match beanie_keeper::evm_keeper::build_client(&*evm_cfg).await {
                Ok(c2) => c2,
                Err(e2) => {
                    eprintln!("retry failed building keeper client: {e2}");
                    return;
                }
            }
        }
    };

    let mut registry_watermark = evm_cfg.registry_start_block;
    let mut webhook_watermark = evm_cfg.webhook_registry_start_block;
    let mut deposit_watermark = evm_cfg.deposit_start_block;
    let mut sn_registry_watermark = starknet_cfg.registry_start_block;
    let mut sn_deposit_watermark = starknet_cfg.registry_start_block;

    // Build Starknet keeper account
    let starknet_account =
        match beanie_keeper::starknet_keeper::build_starknet_account(&*starknet_cfg) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("failed building starknet keeper account for native poller: {e}");
                return;
            }
        };

    loop {
        // get tip
        let tip_bn = match evm_client.provider().get_block_number().await {
            Ok(n) => n.as_u64(),
            Err(e) => {
                eprintln!("failed fetching block number for poller: {e}");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Refresh EVM merchant registry
        let mut evm_merchant_map: HashMap<ethers::types::Address, ethers::types::Address> =
            HashMap::new();
        if registry_watermark <= tip_bn {
            match beanie_keeper::evm_keeper::discover_merchants(
                &keeper_client,
                &*evm_cfg,
                registry_watermark,
                tip_bn,
            )
            .await
            {
                Ok(found) => {
                    for (merchant, receiver) in found {
                        evm_merchant_map.insert(receiver, merchant);
                    }
                    registry_watermark = tip_bn + 1;
                }
                Err(e) => {
                    eprintln!("discover_merchants failed: {e}");
                }
            }
        }

        // Refresh webhook urls
        let mut webhook_map: HashMap<String, String> = HashMap::new();
        if webhook_watermark <= tip_bn {
            match beanie_keeper::evm_keeper::discover_webhook_urls(
                &keeper_client,
                &*evm_cfg,
                webhook_watermark,
                tip_bn,
            )
            .await
            {
                Ok(found) => {
                    for (merchant, url) in found {
                        webhook_map.insert(format!("{merchant:?}"), url);
                    }
                    webhook_watermark = tip_bn + 1;
                }
                Err(e) => eprintln!("discover_webhook_urls failed: {e}"),
            }
        }

        // Build EVM receivers list
        let receivers: Vec<ethers::types::Address> = evm_merchant_map.keys().copied().collect();

        // Fetch deposits for this window
        if deposit_watermark <= tip_bn && !receivers.is_empty() {
            match beanie_keeper::evm_keeper::fetch_deposits_since_block(
                &keeper_client,
                &*evm_cfg,
                &receivers,
                deposit_watermark,
                tip_bn,
            )
            .await
            {
                Ok(deposits) => {
                    if deposits.is_empty() {
                        deposit_watermark = tip_bn + 1;
                    } else {
                        // Deduplicate receivers and sweep
                        let mut sweep_receivers: Vec<ethers::types::Address> = Vec::new();
                        for d in &deposits {
                            if let Ok(addr) = d.receiver.parse::<ethers::types::Address>() {
                                sweep_receivers.push(addr);
                            }
                        }
                        sweep_receivers.sort();
                        sweep_receivers.dedup();

                        let sweep_tx = match beanie_keeper::evm_keeper::multicall_sweep(
                            keeper_client.clone(),
                            &sweep_receivers,
                        )
                        .await
                        {
                            Ok(tx) => tx,
                            Err(e) => {
                                eprintln!("multicall_sweep failed: {e}");
                                None
                            }
                        };

                        // Deliver webhooks per deposit (EVM)
                        for deposit in deposits {
                            let merchant =
                                evm_merchant_map.get(&deposit.receiver.parse().unwrap_or_default());
                            if merchant.is_none() {
                                eprintln!("no merchant known for receiver {}", deposit.receiver);
                                continue;
                            }
                            let merchant_addr = merchant.unwrap();
                            let webhook_key = format!("{merchant_addr:?}");
                            if let Some(url) = webhook_map.get(&webhook_key) {
                                let cfg = beanie_keeper::config::Config::Evm((*evm_cfg).clone());
                                let job = crate::models::WebhookJob {
                                    cfg,
                                    webhook_url: url.clone(),
                                    deposit: deposit.clone(),
                                    sweep_tx: sweep_tx.as_deref().map(|s| s.to_string()),
                                    max_retries: 5,
                                };

                                if let Err(e) = webhook_tx.send(job).await {
                                    eprintln!("failed enqueuing webhook job: {e}");
                                }
                            } else {
                                eprintln!(
                                    "merchant {} has no webhook url for deposit {}",
                                    merchant_addr, deposit.tx_hash
                                );
                            }
                        }

                        deposit_watermark = tip_bn + 1;
                    }
                }
                Err(e) => eprintln!("fetch_deposits_since_block failed: {e}"),
            }
        }

        // ----- Starknet side -----
        // get starknet tip (block number)
        let sn_tip = match starknet_account.provider().block_number().await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("failed fetching starknet tip: {e}");
                0u64
            }
        };

        if sn_registry_watermark <= sn_tip {
            match beanie_keeper::starknet_keeper::discover_merchants(
                &starknet_account,
                &*starknet_cfg,
                sn_registry_watermark,
                sn_tip,
            )
            .await
            {
                Ok(found) => {
                    // build mapping felt->felt for starknet
                    let mut sn_merchant_map: HashMap<Felt, Felt> = HashMap::new();
                    for (merchant, receiver) in found {
                        sn_merchant_map.insert(receiver, merchant);
                    }
                    sn_registry_watermark = sn_tip + 1;

                    // fetch deposits
                    if sn_deposit_watermark <= sn_tip && !sn_merchant_map.is_empty() {
                        let receivers_sn: Vec<Felt> = sn_merchant_map.keys().copied().collect();
                        match beanie_keeper::starknet_keeper::fetch_deposits_since_block(
                            &starknet_account,
                            &*starknet_cfg,
                            &receivers_sn,
                            sn_deposit_watermark,
                            sn_tip,
                        )
                        .await
                        {
                            Ok(sn_deposits) => {
                                if !sn_deposits.is_empty() {
                                    // sweep receivers
                                    let mut sweep_receivers_sn: Vec<Felt> = Vec::new();
                                    for d in &sn_deposits {
                                        if let Ok(f) = Felt::from_hex(&d.receiver) {
                                            sweep_receivers_sn.push(f);
                                        }
                                    }
                                    sweep_receivers_sn
                                        .sort_by(|a, b| a.to_bytes_be().cmp(&b.to_bytes_be()));
                                    sweep_receivers_sn.dedup();

                                    let sweep_tx_sn =
                                        match beanie_keeper::starknet_keeper::multicall_sweep(
                                            starknet_account.clone(),
                                            &sweep_receivers_sn,
                                        )
                                        .await
                                        {
                                            Ok(tx) => tx,
                                            Err(e) => {
                                                eprintln!("starknet multicall_sweep failed: {e}");
                                                None
                                            }
                                        };

                                    // deliver webhooks for each deposit. lookup webhook url via EVM webhook_map using derived EVM merchant address
                                    for d in sn_deposits {
                                        // derive evm merchant address from merchant felt string
                                        let merchant_felt = match Felt::from_hex(&d.receiver) {
                                            Ok(f) => f,
                                            Err(_) => continue,
                                        };
                                        let merchant_str = format!("{:#x}", merchant_felt);
                                        let hash = keccak256(merchant_str.as_bytes());
                                        let evm_merchant = Address::from_slice(&hash[12..32]);
                                        let webhook_key = format!("{evm_merchant:?}");

                                        if let Some(url) = webhook_map.get(&webhook_key) {
                                            let cfg = beanie_keeper::config::Config::Starknet(
                                                (*starknet_cfg).clone(),
                                            );
                                            let job = crate::models::WebhookJob {
                                                cfg,
                                                webhook_url: url.clone(),
                                                deposit: d.clone(),
                                                sweep_tx: sweep_tx_sn
                                                    .as_deref()
                                                    .map(|s| s.to_string()),
                                                max_retries: 5,
                                            };

                                            if let Err(e) = webhook_tx.send(job).await {
                                                eprintln!("failed enqueuing webhook job: {e}");
                                            }
                                        } else {
                                            eprintln!(
                                                "no webhook URL for merchant {}",
                                                webhook_key
                                            );
                                        }
                                    }
                                }
                                sn_deposit_watermark = sn_tip + 1;
                            }
                            Err(e) => eprintln!("starknet fetch_deposits_since_block failed: {e}"),
                        }
                    }
                }
                Err(e) => eprintln!("starknet discover_merchants failed: {e}"),
            }
        }

        sleep(evm_cfg.poll_interval).await;
    }
}

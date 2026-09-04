use std::collections::HashMap;
use std::sync::Arc;

use ethers::contract::abigen;
use ethers::providers::Middleware;
use ethers::types::{Address, U256};
use ethers::utils::keccak256;
use starknet::accounts::{Account, ConnectedAccount};
use starknet::core::types::{BlockId, BlockTag, Call, Felt};
use starknet::core::utils::get_selector_from_name;
use starknet::providers::Provider;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use crate::models::{ChainXReceiverLocal, MerchantFactory};
use crate::payment_workers::EvmSignerProvider;

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

/// Polls for native/ERC20 transfers to known (deployed **or announced**) receivers.
/// When funds are found on a still-undeployed receiver it JIT-deploys via
/// `registerMerchant` and sweeps in a single atomic multicall / execute_v3.
pub async fn run_native_transfer_poller(
    evm_client: Arc<EvmSignerProvider>,
    evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    println!("Native transfer poller starting...");

    // Keeper-compatible client (RetryClient) for the existing helper functions
    let keeper_client = match beanie_keeper::evm_keeper::build_client(&*evm_cfg).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed building keeper client for native poller: {e}");
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

    let starknet_account =
        match beanie_keeper::starknet_keeper::build_starknet_account(&*starknet_cfg) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("failed building starknet keeper account for native poller: {e}");
                return;
            }
        };

    loop {
        // ------------------------------------------------------------------
        // EVM tip
        // ------------------------------------------------------------------
        let tip_bn = match evm_client.provider().get_block_number().await {
            Ok(n) => n.as_u64(),
            Err(e) => {
                eprintln!("failed fetching block number for poller: {e}");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // ------------------------------------------------------------------
        // Refresh EVM merchant registry (now includes ReceiverAnnounced)
        // ------------------------------------------------------------------
        let mut evm_merchant_map: HashMap<Address, Address> = HashMap::new();
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
                Err(e) => eprintln!("discover_merchants failed: {e}"),
            }
        }

        // ------------------------------------------------------------------
        // Refresh webhook URLs
        // ------------------------------------------------------------------
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

        // ------------------------------------------------------------------
        // EVM deposits → atomic register (if missing) + sweep
        // ------------------------------------------------------------------
        let receivers: Vec<Address> = evm_merchant_map.keys().copied().collect();

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
                        // Unique receivers that received funds
                        let mut unique: Vec<Address> = deposits
                            .iter()
                            .filter_map(|d| d.receiver.parse::<Address>().ok())
                            .collect();
                        unique.sort();
                        unique.dedup();

                        // Build Multicall3 calls: optional registerMerchant + sweep
                        let mut calls: Vec<Call3> = Vec::new();

                        for &receiver_addr in &unique {
                            // Does the contract already exist?
                            let code =
                                match evm_client.provider().get_code(receiver_addr, None).await {
                                    Ok(b) => b,
                                    Err(e) => {
                                        eprintln!("get_code failed for {receiver_addr:?}: {e}");
                                        continue;
                                    }
                                };
                            let exists = !code.0.is_empty();

                            // JIT deploy if still counterfactual
                            if !exists {
                                let merchant = match evm_merchant_map.get(&receiver_addr) {
                                    Some(m) => *m,
                                    None => {
                                        eprintln!(
                                            "no merchant mapping for undeployed receiver {receiver_addr:?}"
                                        );
                                        continue;
                                    }
                                };

                                // Default to same-chain (zeros).
                                // If you later extend ReceiverAnnounced with CCTP
                                // params, read them here instead.
                                let cctp_chain_bytes = [0u8; 32];
                                let recipient_bytes = [0u8; 32];

                                let reg_call = MerchantFactory::new(
                                    evm_cfg.factory_address,
                                    evm_client.clone(),
                                )
                                .register_merchant(
                                    merchant,
                                    cctp_chain_bytes,
                                    recipient_bytes,
                                );

                                match reg_call.calldata() {
                                    Some(bytes) => {
                                        calls.push(Call3 {
                                            target: evm_cfg.factory_address,
                                            allow_failure: false,
                                            call_data: bytes,
                                        });
                                    }
                                    None => {
                                        eprintln!(
                                            "failed encoding registerMerchant for {receiver_addr:?}"
                                        );
                                        continue;
                                    }
                                }
                            }

                            // Always sweep
                            let receiver_contract =
                                ChainXReceiverLocal::new(receiver_addr, evm_client.clone());
                            match receiver_contract.sweep().calldata() {
                                Some(bytes) => {
                                    calls.push(Call3 {
                                        target: receiver_addr,
                                        allow_failure: false,
                                        call_data: bytes,
                                    });
                                }
                                None => {
                                    eprintln!("failed encoding sweep for {receiver_addr:?}");
                                }
                            }
                        }

                        // Send the single atomic multicall (if anything to do)
                        let sweep_tx = if calls.is_empty() {
                            None
                        } else {
                            let multicall_addr: Address =
                                MULTICALL3_ADDRESS.parse().expect("valid multicall addr");
                            let multicall = Multicall3::new(multicall_addr, evm_client.clone());
                            let mut agg = multicall.aggregate_3(calls);

                            // Fee caps (same policy as payment worker)
                            let (suggested_max_fee, suggested_priority_fee) = evm_client
                                .estimate_eip1559_fees(None)
                                .await
                                .unwrap_or((U256::zero(), U256::zero()));
                            let max_allowed_priority = ethers::utils::parse_units("0.05", "gwei")
                                .expect("priority fee ceiling");
                            let priority_fee =
                                std::cmp::min(suggested_priority_fee, max_allowed_priority.into());
                            let max_fee_cap =
                                ethers::utils::parse_units("0.1", "gwei").expect("max fee cap");
                            let max_fee = std::cmp::min(suggested_max_fee, max_fee_cap.into());

                            if let Some(eip1559_req) = agg.tx.as_eip1559_mut() {
                                eip1559_req.max_priority_fee_per_gas = Some(priority_fee);
                                eip1559_req.max_fee_per_gas = Some(max_fee);
                            }

                            match agg.send().await {
                                Ok(pending) => match pending.await {
                                    Ok(Some(receipt)) => {
                                        let tx_hash = format!("{:#x}", receipt.transaction_hash);
                                        println!("native atomic register+sweep -> {tx_hash}");
                                        Some(tx_hash)
                                    }
                                    Ok(None) => {
                                        eprintln!("native multicall dropped");
                                        None
                                    }
                                    Err(e) => {
                                        eprintln!("native multicall failed: {e}");
                                        None
                                    }
                                },
                                Err(e) => {
                                    eprintln!("failed sending native multicall: {e}");
                                    None
                                }
                            }
                        };

                        // Webhooks
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
                                    sweep_tx: sweep_tx.clone(),
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

        // ------------------------------------------------------------------
        // Starknet side
        // ------------------------------------------------------------------
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
                    let mut sn_merchant_map: HashMap<Felt, Felt> = HashMap::new();
                    for (merchant, receiver) in found {
                        sn_merchant_map.insert(receiver, merchant);
                    }
                    sn_registry_watermark = sn_tip + 1;

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
                                    // Unique receivers
                                    let mut unique: Vec<Felt> = sn_deposits
                                        .iter()
                                        .filter_map(|d| Felt::from_hex(&d.receiver).ok())
                                        .collect();
                                    unique.sort_by(|a, b| a.to_bytes_be().cmp(&b.to_bytes_be()));
                                    unique.dedup();

                                    // Build atomic calls: register (if needed) + sweep
                                    let mut calls: Vec<Call> = Vec::new();

                                    let register_selector =
                                        match get_selector_from_name("register_merchant") {
                                            Ok(s) => s,
                                            Err(e) => {
                                                eprintln!("selector register_merchant: {e}");
                                                continue;
                                            }
                                        };
                                    let sweep_selector = match get_selector_from_name("sweep") {
                                        Ok(s) => s,
                                        Err(e) => {
                                            eprintln!("selector sweep: {e}");
                                            continue;
                                        }
                                    };

                                    for &receiver in &unique {
                                        let merchant = match sn_merchant_map.get(&receiver) {
                                            Some(m) => *m,
                                            None => continue,
                                        };

                                        // Existence check via class hash.
                                        // If the call fails or returns zero we treat
                                        // the address as still counterfactual.
                                        let needs_deploy = match starknet_account
                                            .provider()
                                            .get_class_hash_at(
                                                BlockId::Tag(BlockTag::Latest),
                                                receiver,
                                            )
                                            .await
                                        {
                                            Ok(ch) => ch == Felt::ZERO,
                                            Err(_) => true, // no class → needs deploy
                                        };

                                        if needs_deploy {
                                            // Same-chain default (zeros).
                                            // Extend ReceiverAnnounced if you need
                                            // cross-chain params here.
                                            calls.push(Call {
                                                to: starknet_cfg.factory_address,
                                                selector: register_selector,
                                                calldata: vec![
                                                    merchant,
                                                    Felt::ZERO, // cctp_mint_chain
                                                    Felt::ZERO, // recipient low
                                                    Felt::ZERO, // recipient high
                                                ],
                                            });
                                        }

                                        calls.push(Call {
                                            to: receiver,
                                            selector: sweep_selector,
                                            calldata: vec![],
                                        });
                                    }

                                    let sweep_tx_sn = if calls.is_empty() {
                                        None
                                    } else {
                                        match starknet_account.execute_v3(calls).send().await {
                                            Ok(pending) => {
                                                let tx_hash =
                                                    format!("{:#x}", pending.transaction_hash);
                                                println!(
                                                    "starknet native atomic register+sweep -> {tx_hash}"
                                                );
                                                Some(tx_hash)
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "starknet native atomic invoke failed: {e}"
                                                );
                                                None
                                            }
                                        }
                                    };

                                    // Webhooks (lookup via derived EVM merchant address)
                                    for d in sn_deposits {
                                        let merchant_felt = match Felt::from_hex(&d.receiver) {
                                            Ok(f) => f,
                                            Err(_) => continue,
                                        };
                                        // Prefer the real merchant from the map
                                        let merchant_for_webhook = sn_merchant_map
                                            .get(&merchant_felt)
                                            .copied()
                                            .unwrap_or(merchant_felt);

                                        let merchant_str = format!("{:#x}", merchant_for_webhook);
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
                                                sweep_tx: sweep_tx_sn.clone(),
                                                max_retries: 5,
                                            };
                                            if let Err(e) = webhook_tx.send(job).await {
                                                eprintln!("failed enqueuing webhook job: {e}");
                                            }
                                        } else {
                                            eprintln!("no webhook URL for merchant {webhook_key}");
                                        }
                                    }
                                }
                                sn_deposit_watermark = sn_tip + 1;
                            }
                            Err(e) => {
                                eprintln!("starknet fetch_deposits_since_block failed: {e}")
                            }
                        }
                    }
                }
                Err(e) => eprintln!("starknet discover_merchants failed: {e}"),
            }
        }

        sleep(evm_cfg.poll_interval).await;
    }
}

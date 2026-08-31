mod config;
mod sweep_evm;
mod sweep_starknet;
mod webhook;

use anyhow::Result;
use config::Config;
use ethers::{providers::Middleware, signers::Signer, types::Address};
use starknet::{accounts::ConnectedAccount, core::types::Felt, providers::Provider};
use std::{collections::HashMap, ops::Deref, sync::Arc};
use tokio::time::sleep;

use crate::config::{EvmConfig, StarknetConfig, now_formatted};

const MAX_WEBHOOK_RETRIES: u32 = 5;

#[derive(Clone)]
pub enum Client {
    Base {
        base_provider: Arc<sweep_evm::SignerProvider>,
        base_config: Arc<config::EvmConfig>,
        base_registry: Arc<tokio::sync::Mutex<HashMap<Address, Address>>>,
    },
    Starknet {
        starknet_account: Arc<sweep_starknet::StarknetAccount>,
        starknet_config: Arc<config::StarknetConfig>,
        starknet_registry: Arc<tokio::sync::Mutex<HashMap<Felt, Felt>>>,
    },
}

#[derive(PartialEq, Eq, Hash, Clone)]
pub enum BeanieAddress {
    Address,
    Felt,
}

fn merchant_key_addr(a: Address) -> String {
    format!("{:#x}", a)
}
fn merchant_key_felt(f: Felt) -> String {
    format!("{:#x}", f)
}

/// Pulls any newly-registered merchants into the in-memory receiver set, and tracks
/// which merchant owns each receiver so a deposit can be traced back to a webhook URL.
async fn refresh_registry(
    client: &Arc<Client>,
    registry_watermark: &mut u64,
    tip: u64,
) -> Result<()> {
    match client.deref() {
        Client::Base {
            base_provider,
            base_config,
            base_registry,
        } => {
            if *registry_watermark > tip {
                return Ok(());
            }
            let found =
                sweep_evm::discover_merchants(base_provider, base_config, *registry_watermark, tip)
                    .await?;
            let mut map = base_registry.lock().await;
            for (merchant, receiver) in found {
                map.entry(receiver).or_insert(merchant);
            }
            *registry_watermark = tip + 1;
            Ok(())
        }
        Client::Starknet {
            starknet_account,
            starknet_config,
            starknet_registry,
        } => {
            if *registry_watermark > tip {
                return Ok(());
            }
            let found = sweep_starknet::discover_merchants(
                starknet_account,
                starknet_config,
                *registry_watermark,
                tip,
            )
            .await?;
            let mut map = starknet_registry.lock().await;
            for (merchant, receiver) in found {
                map.entry(receiver).or_insert(merchant);
            }
            *registry_watermark = tip + 1;
            Ok(())
        }
    }
}

/// Pulls any new/updated webhook URL registrations. Each merchant's own server, not one
/// shared endpoint — this is what makes per-merchant delivery possible at all.
async fn refresh_webhooks(
    client: &Arc<sweep_evm::SignerProvider>,
    cfg: &EvmConfig,
    webhook_watermark: &mut u64,
    merchant_webhook: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    tip: u64,
) -> Result<()> {
    if *webhook_watermark > tip {
        return Ok(());
    }
    let found = sweep_evm::discover_webhook_urls(client, cfg, *webhook_watermark, tip).await?;
    for (merchant, url) in found {
        merchant_webhook
            .lock()
            .await
            .insert(merchant_key_addr(merchant), url); // later events overwrite earlier ones
    }
    Ok(())
}

/// One multicall sweeps every distinct receiver that saw activity this batch — a single
/// tx, single nonce, regardless of how many merchants were touched. Each deposit is then
/// resolved receiver -> merchant -> that merchant's own webhook URL before delivery.
async fn process_deposits(
    client: &Arc<Client>,
    http: &reqwest::Client,
    merchant_webhook: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    deposits: Vec<config::Deposit>,
) -> Result<()> {
    if deposits.is_empty() {
        return Ok(());
    }

    match client.deref() {
        Client::Base {
            base_provider,
            base_config,
            base_registry,
        } => {
            let mut receivers: Vec<Address> = deposits
                .iter()
                .filter_map(|d| match d.receiver.parse::<Address>() {
                    Ok(a) => Some(a),
                    Err(e) => {
                        eprintln!(
                            "[{}] bad receiver address {}: {e}",
                            now_formatted(),
                            d.receiver
                        );
                        None
                    }
                })
                .collect();
            receivers.sort();
            receivers.dedup();
            let sweep_tx = match sweep_evm::multicall_sweep(base_provider.clone(), &receivers).await
            {
                Ok(tx) => tx,
                Err(e) => {
                    eprintln!("[{}] multicall sweep failed: {e:#}", now_formatted());
                    None
                }
            };

            for deposit in &deposits {
                let receiver: Address = match deposit.receiver.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let merchant = match base_registry.lock().await.get(&receiver) {
                    Some(m) => *m,
                    None => {
                        eprintln!(
                            "[{}] no known merchant for receiver {:?} — skipping webhook for tx {}",
                            now_formatted(),
                            receiver,
                            deposit.tx_hash
                        );
                        continue;
                    }
                };
                let merchant_webhook_guard = merchant_webhook.lock().await;
                let webhook_url = match merchant_webhook_guard.get(&merchant_key_addr(merchant)) {
                    Some(u) => u,
                    None => {
                        eprintln!(
                            "[{}] merchant {:?} has no registered webhook URL — skipping tx {}",
                            now_formatted(),
                            merchant,
                            deposit.tx_hash
                        );
                        continue;
                    }
                };

                println!(
                    "[{}] delivering webhook for tx {} -> {}",
                    now_formatted(),
                    deposit.tx_hash,
                    webhook_url
                );

                if let Err(e) = webhook::deliver_deposit(
                    http,
                    &Config::Evm(base_config.deref().clone()),
                    webhook_url,
                    deposit,
                    sweep_tx.as_deref(),
                    MAX_WEBHOOK_RETRIES,
                )
                .await
                {
                    eprintln!(
                        "[{}] webhook permanently failed for {}: {e:#}",
                        now_formatted(),
                        deposit.tx_hash
                    );
                }
            }
        }
        Client::Starknet {
            starknet_account,
            starknet_config,
            starknet_registry,
        } => {
            let mut receivers: Vec<Felt> = deposits
                .iter()
                .filter_map(|d| match d.receiver.parse::<Felt>() {
                    Ok(a) => Some(a),
                    Err(e) => {
                        eprintln!(
                            "[{}] bad receiver address {}: {e}",
                            now_formatted(),
                            d.receiver
                        );
                        None
                    }
                })
                .collect();
            receivers.sort();
            receivers.dedup();
            let sweep_tx =
                match sweep_starknet::multicall_sweep(starknet_account.clone(), &receivers).await {
                    Ok(tx) => tx,
                    Err(e) => {
                        eprintln!("[{}] multicall sweep failed: {e:#}", now_formatted());
                        None
                    }
                };

            for deposit in &deposits {
                let receiver: Felt = match deposit.receiver.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let merchant = match starknet_registry.lock().await.get(&receiver) {
                    Some(m) => *m,
                    None => {
                        eprintln!(
                            "[{}] no known merchant for receiver {:?} — skipping webhook for tx {}",
                            now_formatted(),
                            receiver,
                            deposit.tx_hash
                        );
                        continue;
                    }
                };

                let merchant_webhook_guard = merchant_webhook.lock().await;
                let webhook_url = match merchant_webhook_guard.get(&merchant_key_felt(merchant)) {
                    Some(u) => u,
                    None => {
                        eprintln!(
                            "[{}] merchant {:?} has no registered webhook URL — skipping tx {}",
                            now_formatted(),
                            merchant,
                            deposit.tx_hash
                        );
                        continue;
                    }
                };

                println!(
                    "[{}] delivering webhook for tx {} -> {}",
                    now_formatted(),
                    deposit.tx_hash,
                    webhook_url
                );

                if let Err(e) = webhook::deliver_deposit(
                    http,
                    &Config::Starknet(starknet_config.deref().clone()),
                    webhook_url,
                    deposit,
                    sweep_tx.as_deref(),
                    MAX_WEBHOOK_RETRIES,
                )
                .await
                {
                    eprintln!(
                        "[{}] webhook permanently failed for {}: {e:#}",
                        now_formatted(),
                        deposit.tx_hash
                    );
                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let base_config = Arc::new(EvmConfig::from_env()?);
    let starknet_config = Arc::new(StarknetConfig::from_env()?);

    let base_provider = sweep_evm::build_client(&base_config).await?;
    let starknet_account = sweep_starknet::build_starknet_account(&starknet_config)?;

    let base_registry = Arc::new(tokio::sync::Mutex::new(HashMap::<Address, Address>::new()));
    let starknet_registry = Arc::new(tokio::sync::Mutex::new(HashMap::<Felt, Felt>::new()));

    let base_client = Arc::new(Client::Base {
        base_provider: base_provider.clone(),
        base_config: base_config.clone(),
        base_registry: base_registry.clone(),
    });

    let starknet_client = Arc::new(Client::Starknet {
        starknet_account: starknet_account.clone(),
        starknet_config: starknet_config.clone(),
        starknet_registry: starknet_registry.clone(),
    });

    let http = reqwest::Client::new();
    let merchant_webhook = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    println!("sweeper starting —");
    println!(
        "chains={:?}",
        [
            base_config.chain_name.clone(),
            starknet_config.chain_name.clone()
        ]
        .to_vec()
    );

    println!(
        "webhook registry={:?}",
        base_config.webhook_registry_address
    );
    println!("base starting block={:?}", base_config.start_block);
    println!("starknet starting block={:?}", starknet_config.start_block);
    println!("base token={:?}", base_config.token_address);
    println!("base factory={:?}", base_config.factory_address);
    println!("starknet token={:?}", starknet_config.token_address);
    println!("starknet factory={:?}", starknet_config.factory_address);

    println!(
        "keeper address (the one ID merchants subscribe to — signs both sweep txs and webhooks):",
    );
    println!(
        " base-{:?}, starknet-{:?}",
        base_config.keeper_wallet.address(),
        starknet_config.keeper_address
    );

    // Clones for Starknet task before base task consumes outer handles
    let merchant_webhook_starknet = merchant_webhook.clone();
    let http_starknet = http.clone();

    let mut deposit_watermark = base_config.start_block;
    let mut base_watermark = base_config.registry_start_block;
    let mut base_webhook_watermark = base_config.webhook_registry_start_block;

    // Base execution loop
    let base_handle = tokio::spawn(async move {
        loop {
            let tip = match base_provider.get_block_number().await {
                Ok(b) => b.as_u64(),
                Err(e) => {
                    eprintln!("[{}] get_block_number failed: {e:#}", now_formatted());
                    sleep(base_config.poll_interval).await;
                    continue;
                }
            };

            if let Err(e) = refresh_registry(&base_client, &mut base_watermark, tip).await {
                eprintln!("[{}] registry refresh error: {e:#}", now_formatted());
            }

            if let Err(e) = refresh_webhooks(
                &base_provider,
                &base_config,
                &mut base_webhook_watermark,
                &merchant_webhook,
                tip,
            )
            .await
            {
                eprintln!(
                    "[{}] webhook registry refresh error: {e:#}",
                    now_formatted()
                );
            }

            if deposit_watermark <= tip && !base_registry.lock().await.is_empty() {
                println!(
                    "[{}] cycle start, deposit_watermark={} tip={} known_receivers={}",
                    now_formatted(),
                    deposit_watermark,
                    tip,
                    base_registry.lock().await.keys().count()
                );

                let receiver_keys: Vec<Address> =
                    base_registry.lock().await.keys().copied().collect();
                match sweep_evm::fetch_deposits_since_block(
                    &base_provider,
                    &base_config,
                    &receiver_keys,
                    deposit_watermark,
                    tip,
                )
                .await
                {
                    Ok(deposits) => {
                        if let Err(e) =
                            process_deposits(&base_client, &http, &merchant_webhook, deposits).await
                        {
                            eprintln!("[{}] deposit processing error: {e:#}", now_formatted());
                        }
                        deposit_watermark = tip + 1;
                    }
                    Err(e) => eprintln!("[{}] deposit fetch error: {e:#}", now_formatted()),
                }
            }

            sleep(base_config.poll_interval).await;
        }
    });

    let mut starknet_registry_watermark = starknet_config.registry_start_block;
    let mut starknet_deposit_watermark = starknet_config.start_block;

    // Starknet execution loop
    let starknet_handle = tokio::spawn(async move {
        loop {
            let tip = match starknet_account.provider().block_number().await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[{}] starknet block_number failed: {e:#}", now_formatted());
                    sleep(starknet_config.poll_interval).await;
                    continue;
                }
            };

            if let Err(e) =
                refresh_registry(&starknet_client, &mut starknet_registry_watermark, tip).await
            {
                eprintln!(
                    "[{}] starknet registry refresh error: {e:#}",
                    now_formatted()
                );
            }

            if starknet_deposit_watermark <= tip && !starknet_registry.lock().await.is_empty() {
                println!(
                    "[{}] starknet cycle start, deposit_watermark={} tip={} known_receivers={}",
                    now_formatted(),
                    starknet_deposit_watermark,
                    tip,
                    starknet_registry.lock().await.keys().count()
                );

                let receiver_keys: Vec<Felt> =
                    starknet_registry.lock().await.keys().copied().collect();

                match sweep_starknet::fetch_deposits_since_block(
                    &starknet_account,
                    &starknet_config,
                    &receiver_keys,
                    starknet_deposit_watermark,
                    tip,
                )
                .await
                {
                    Ok(deposits) => {
                        if let Err(e) = process_deposits(
                            &starknet_client,
                            &http_starknet,
                            &merchant_webhook_starknet,
                            deposits,
                        )
                        .await
                        {
                            eprintln!(
                                "[{}] starknet deposit processing error: {e:#}",
                                now_formatted()
                            );
                        }
                        starknet_deposit_watermark = tip + 1;
                    }
                    Err(e) => {
                        eprintln!("[{}] starknet deposit fetch error: {e:#}", now_formatted())
                    }
                }
            }

            sleep(starknet_config.poll_interval).await;
        }
    });

    tokio::try_join!(base_handle, starknet_handle)?;

    Ok(())
}

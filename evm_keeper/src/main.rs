mod config;
mod sweep;
mod webhook;

use anyhow::Result;
use config::Config;
use ethers::{providers::Middleware, signers::Signer, types::Address};
use std::{collections::HashMap, sync::Arc};
use tokio::time::sleep;

use crate::config::now_formatted;

const MAX_WEBHOOK_RETRIES: u32 = 5;

/// Pulls any newly-registered merchants into the in-memory receiver set, and tracks
/// which merchant owns each receiver so a deposit can be traced back to a webhook URL.
async fn refresh_registry(
    client: &Arc<sweep::SignerProvider>,
    cfg: &Config,
    registry_watermark: &mut u64,
    known: &mut Vec<Address>,
    receiver_to_merchant: &mut HashMap<Address, Address>,
    tip: u64,
) -> Result<()> {
    if *registry_watermark > tip {
        return Ok(());
    }
    let found = sweep::discover_merchants(client, cfg, *registry_watermark, tip).await?;
    for (merchant, receiver) in found {
        if !known.contains(&receiver) {
            known.push(receiver);
        }
        receiver_to_merchant.insert(receiver, merchant);
    }
    *registry_watermark = tip + 1;
    Ok(())
}

/// Pulls any new/updated webhook URL registrations. Each merchant's own server, not one
/// shared endpoint — this is what makes per-merchant delivery possible at all.
async fn refresh_webhooks(
    client: &Arc<sweep::SignerProvider>,
    cfg: &Config,
    webhook_watermark: &mut u64,
    merchant_webhook: &mut HashMap<Address, String>,
    tip: u64,
) -> Result<()> {
    if *webhook_watermark > tip {
        return Ok(());
    }
    let found = sweep::discover_webhook_urls(client, cfg, *webhook_watermark, tip).await?;
    for (merchant, url) in found {
        merchant_webhook.insert(merchant, url); // later events overwrite earlier ones
    }
    *webhook_watermark = tip + 1;
    Ok(())
}

/// One multicall sweeps every distinct receiver that saw activity this batch — a single
/// tx, single nonce, regardless of how many merchants were touched. Each deposit is then
/// resolved receiver -> merchant -> that merchant's own webhook URL before delivery.
async fn process_deposits(
    client: &Arc<sweep::SignerProvider>,
    http: &reqwest::Client,
    cfg: &Config,
    receiver_to_merchant: &HashMap<Address, Address>,
    merchant_webhook: &HashMap<Address, String>,
    deposits: Vec<config::Deposit>,
) -> Result<()> {
    if deposits.is_empty() {
        return Ok(());
    }

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

    let sweep_tx = match sweep::multicall_sweep(client.clone(), &receivers).await {
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

        let merchant = match receiver_to_merchant.get(&receiver) {
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

        let webhook_url = match merchant_webhook.get(&merchant) {
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
            cfg,
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

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cfg = Arc::new(Config::from_env()?);
    let client = sweep::build_client(&cfg).await?;
    let http = reqwest::Client::new();

    let mut deposit_watermark = cfg.start_block;
    let mut registry_watermark = cfg.registry_start_block;
    let mut webhook_watermark = cfg.webhook_registry_start_block;
    let mut known_receivers: Vec<Address> = Vec::new();
    let mut receiver_to_merchant: HashMap<Address, Address> = HashMap::new();
    let mut merchant_webhook: HashMap<Address, String> = HashMap::new();

    println!(
        "sweeper starting — chain={} token={:?} factory={:?} webhook_registry={:?} start_block={}",
        cfg.chain_name,
        cfg.token_address,
        cfg.factory_address,
        cfg.webhook_registry_address,
        cfg.start_block
    );
    println!(
        "keeper address (the one ID merchants subscribe to — signs both sweep txs and webhooks): {:?}",
        cfg.keeper_wallet.address()
    );

    loop {
        let tip = match client.get_block_number().await {
            Ok(b) => b.as_u64(),
            Err(e) => {
                eprintln!("[{}] get_block_number failed: {e:#}", now_formatted());
                sleep(cfg.poll_interval).await;
                continue;
            }
        };

        if let Err(e) = refresh_registry(
            &client,
            &cfg,
            &mut registry_watermark,
            &mut known_receivers,
            &mut receiver_to_merchant,
            tip,
        )
        .await
        {
            eprintln!("[{}] registry refresh error: {e:#}", now_formatted());
        }

        if let Err(e) = refresh_webhooks(
            &client,
            &cfg,
            &mut webhook_watermark,
            &mut merchant_webhook,
            tip,
        )
        .await
        {
            eprintln!(
                "[{}] webhook registry refresh error: {e:#}",
                now_formatted()
            );
        }

        if deposit_watermark <= tip && !known_receivers.is_empty() {
            println!(
                "[{}] cycle start, deposit_watermark={} tip={} known_receivers={}",
                now_formatted(),
                deposit_watermark,
                tip,
                known_receivers.len()
            );

            match sweep::fetch_deposits_since_block(
                &client,
                &cfg,
                &known_receivers,
                deposit_watermark,
                tip,
            )
            .await
            {
                Ok(deposits) => {
                    if let Err(e) = process_deposits(
                        &client,
                        &http,
                        &cfg,
                        &receiver_to_merchant,
                        &merchant_webhook,
                        deposits,
                    )
                    .await
                    {
                        eprintln!("[{}] deposit processing error: {e:#}", now_formatted());
                    }
                    deposit_watermark = tip + 1;
                }
                Err(e) => eprintln!("[{}] deposit fetch error: {e:#}", now_formatted()),
            }
        }

        sleep(cfg.poll_interval).await;
    }
}

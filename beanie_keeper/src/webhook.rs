use std::{cmp::min, time::Duration};

use anyhow::{Context, Result, anyhow};
use ethers::signers::Signer as EvmSignerTrait;
use serde::Serialize;
use starknet::{core::types::Felt, signers::Signer as StarknetSignerTrait};
use starknet_crypto::poseidon_hash_many;
use tokio::time::sleep;

use crate::config::{Config, Deposit, now_unix};

// ── Signing Logic ─────────────────────────────────────────────────────────────

async fn sign_payload(cfg: &Config, data: &[u8]) -> Result<String> {
    match cfg {
        Config::Evm(evm_cfg) => {
            let signature = evm_cfg
                .keeper_wallet
                .sign_message(data)
                .await
                .context("failed signing EIP-191 payload")?;
            Ok(format!("0x{}", hex::encode(signature.to_vec())))
        }
        Config::Starknet(starknet_cfg) => {
            let mut felts = Vec::new();
            for chunk in data.chunks(31) {
                felts.push(Felt::from_bytes_be_slice(chunk));
            }

            let message_hash = poseidon_hash_many(&felts);
            let signature = starknet_cfg
                .keeper_wallet
                .sign_hash(&message_hash)
                .await
                .context("failed signing Starknet payload")?;

            Ok(format!("{:#x}:{:#x}", signature.r, signature.s))
        }
    }
}

// ── Serialization Payload ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct DepositPayload<'a> {
    pub chain: &'a str,
    pub tx_hash: &'a str,
    pub from: &'a str,
    pub receiver: &'a str,
    pub amount_raw: &'a str,
    pub token: &'a str,
    pub block_number: u64,
    pub timestamp: i64,
}

#[derive(Serialize)]
struct NotificationBody<'a> {
    pub deposit: DepositPayload<'a>,
    pub sweep_tx: Option<&'a str>,
}

// ── Delivery Pipeline ─────────────────────────────────────────────────────────

async fn send_webhook_once(
    http: &reqwest::Client,
    cfg: &Config,
    webhook_url: &str,
    deposit: &Deposit,
    sweep_tx: Option<&str>,
) -> Result<(), anyhow::Error> {
    let timestamp = now_unix();
    let token_str = cfg.token_address_str();

    let body_struct = NotificationBody {
        deposit: DepositPayload {
            chain: cfg.chain_name(),
            tx_hash: &deposit.tx_hash,
            from: &deposit.from_address,
            receiver: &deposit.receiver,
            amount_raw: &deposit.amount_raw,
            token: &token_str,
            block_number: deposit.block_number,
            timestamp,
        },
        sweep_tx,
    };

    let body = serde_json::to_string(&body_struct).context("webhook body serialization failed")?;
    let signature = sign_payload(cfg, format!("{timestamp}.{body}").as_bytes()).await?;

    let resp = http
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .header("X-Signature-Scheme", cfg.signature_scheme())
        .header("X-Signature", signature)
        .header("X-Signer-Address", cfg.keeper_address_str())
        .header("X-Timestamp", timestamp.to_string())
        .header("Idempotency-Key", &deposit.tx_hash)
        .timeout(Duration::from_secs(3))
        .body(body)
        .send()
        .await
        .context("webhook HTTP request failed")?;

    let status = resp.status();

    if status.is_success() {
        Ok(())
    } else if status.is_client_error() {
        Err(anyhow!("TERMINAL_ERROR: HTTP {status} rejected payload"))
    } else {
        Err(anyhow!("HTTP status {status}"))
    }
}

pub async fn deliver_deposit(
    http: &reqwest::Client,
    cfg: &Config,
    webhook_url: &str,
    deposit: &Deposit,
    sweep_tx: Option<&str>,
    max_retries: u32,
) -> Result<()> {
    let mut delay = Duration::from_millis(100);
    let max_delay = Duration::from_secs(3);
    let mut attempts = 0;

    loop {
        attempts += 1;
        match send_webhook_once(http, cfg, webhook_url, deposit, sweep_tx).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if format!("{e:#}").contains("TERMINAL_ERROR") {
                    return Err(e);
                }
                if attempts >= max_retries {
                    return Err(anyhow!(
                        "Max retries reached for deposit {}",
                        deposit.tx_hash
                    ));
                }
            }
        }
        sleep(delay).await;
        delay = min(delay * 2, max_delay);
    }
}

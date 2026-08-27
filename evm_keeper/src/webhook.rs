use std::{cmp::min, time::Duration};

use anyhow::{Context, Result, anyhow};
use ethers::signers::Signer;
use serde::Serialize;
use tokio::time::sleep;

use crate::config::{Config, Deposit, now_unix};

/// Standard EIP-191 personal_sign — recoverable, so merchants don't need a separately
/// published pubkey. They recover the address from (signature, message) with any
/// standard EVM library and compare it to the keeper address they subscribed to.
async fn sign_eip191(cfg: &Config, data: &[u8]) -> Result<String> {
    let signature = cfg
        .keeper_wallet
        .sign_message(data)
        .await
        .context("failed signing webhook payload")?;
    Ok(format!("0x{}", hex::encode(signature.to_vec())))
}

#[derive(Serialize)]
struct DepositPayload<'a> {
    pub chain: &'a str,
    pub tx_hash: &'a str,
    pub from: &'a str,
    pub receiver: &'a str, // merchant's ChainXReceiver clone address
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

// ── Webhook Delivery ──────────────────────────────────────────────────────────

async fn send_webhook_once(
    http: &reqwest::Client,
    cfg: &Config,
    webhook_url: &str,
    deposit: &Deposit,
    sweep_tx: Option<&str>,
) -> Result<(), anyhow::Error> {
    let timestamp = now_unix();

    let body_struct = NotificationBody {
        deposit: DepositPayload {
            chain: &cfg.chain_name,
            tx_hash: &deposit.tx_hash,
            from: &deposit.from_address,
            receiver: &deposit.receiver,
            amount_raw: &deposit.amount_raw,
            token: &format!("{:?}", cfg.token_address),
            block_number: deposit.block_number,
            timestamp,
        },
        sweep_tx,
    };

    let body = serde_json::to_string(&body_struct).context("webhook body serialization failed")?;

    let signature = sign_eip191(cfg, format!("{timestamp}.{body}").as_bytes()).await?;

    let resp = http
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .header("X-Signature-Scheme", "eip191")
        .header("X-Signature", signature)
        .header(
            "X-Signer-Address",
            format!("{:?}", cfg.keeper_wallet.address()),
        )
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

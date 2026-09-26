//! Subsquid EVM Event Streaming & Utilities
//!
//! Replaces legacy node RPC WebSocket subscriptions with a Subsquid Portal
//! NDJSON stream (`POST /stream`), maintaining live push delivery to `transfer_workers.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address as AlloyAddress, Bytes as AlloyBytes};
use alloy::rpc::client::ClientBuilder;
use anyhow::{Context, Result};
use ethers::types::Address as EthersAddress;
use futures_util::StreamExt;
use log::warn;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::config::EvmConfig;
use crate::evm_indexer::portal;

#[derive(Debug, Clone, Copy)]
pub struct EvmTip {
    pub block_number: u64,
    pub base_fee_per_gas: Option<u64>,
    pub registry_activity: bool,
    pub webhook_activity: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubsquidStreamQuery {
    #[serde(rename = "type")]
    query_type: String, // "evm"
    from_block: u64,
    include_all_blocks: bool,
    logs: Vec<LogQuery>,
    fields: FieldsQuery,
}

#[derive(Serialize)]
struct LogQuery {
    address: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldsQuery {
    block: BlockFields,
    log: LogFields,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockFields {
    number: bool,
    base_fee_per_gas: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogFields {
    address: bool,
    topics: bool,
}

#[derive(Deserialize, Debug)]
struct SubsquidBlockChunk {
    header: SubsquidHeader,
    logs: Option<Vec<SubsquidLog>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SubsquidHeader {
    number: u64,
    #[serde(default, deserialize_with = "deserialize_hex_u64_opt")]
    base_fee_per_gas: Option<u64>,
}

fn deserialize_hex_u64_opt<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    match opt {
        Some(s) => {
            let clean = s.trim_start_matches("0x");
            u64::from_str_radix(clean, 16)
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
        None => Ok(None),
    }
}

#[derive(Deserialize, Debug)]
struct SubsquidLog {
    address: String,
}

/// Streams block tips and contract activity from Subsquid Portal in real time.
///
/// Takes the full `EvmConfig` (not just its `subsquid_portal_url`) so it
/// can look up *this chain's own* `PortalHandle` via `evm_indexer::portal`
/// — same client (with its `x-api-key` header) and same rate-limit bucket
/// the catch-up scans for this config use, isolated from any other EVM
/// chain's (e.g. Base vs. Arbitrum).
pub async fn run_evm_subscription(
    cfg: Arc<EvmConfig>,
    factory_address: AlloyAddress,
    webhook_registry_address: AlloyAddress,
    from_block: u64,
    tips_tx: mpsc::Sender<EvmTip>,
) {
    let mut current_block = from_block;
    let mut backoff = Duration::from_secs(1);
    let handle = portal(&cfg);

    loop {
        handle.limiter.until_ready().await;

        let block_before = current_block;
        let result = stream_subsquid_once(
            &handle.client,
            &cfg.subsquid_portal_url,
            factory_address,
            webhook_registry_address,
            &mut current_block,
            &tips_tx,
        )
        .await;

        match result {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                // Stream ended without delivering a block: we're at the head
                // (Portal answers 204 / empty body). Wait for the next block
                // instead of re-polling in a tight loop, which was burning
                // the shared 2 req/s Portal budget that the deposit scans
                // also need. If blocks *were* delivered, re-poll immediately.
                if current_block == block_before {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Err(e) => {
                warn!("stream disconnected: {e:#}. Reconnecting in {backoff:?}...");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn stream_subsquid_once(
    client: &reqwest::Client,
    portal_url: &str,
    factory_address: AlloyAddress,
    webhook_registry_address: AlloyAddress,
    current_block: &mut u64,
    tips_tx: &mpsc::Sender<EvmTip>,
) -> Result<()> {
    let query = SubsquidStreamQuery {
        query_type: "evm".to_string(),
        from_block: *current_block,
        include_all_blocks: true, // Needed to ensure blocks without matching logs are still returned
        logs: vec![LogQuery {
            address: vec![
                format!("{factory_address:#x}"),
                format!("{webhook_registry_address:#x}"),
            ],
        }],
        fields: FieldsQuery {
            block: BlockFields {
                number: true,
                base_fee_per_gas: true,
            },
            log: LogFields {
                address: true,
                topics: true,
            },
        },
    };

    let stream_endpoint = format!("{}/stream", portal_url.trim_end_matches('/'));
    let response = client
        .post(&stream_endpoint)
        .json(&query)
        .send()
        .await
        .context("failed initiating Subsquid portal stream")?;

    if !response.status().is_success() {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let req_id = response
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("-")
            .to_owned();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Subsquid stream HTTP {status} retry-after={retry_after:?} req_id={req_id} body={body}"
        );
    }

    // Convert HTTP response byte stream into an NDJSON line-by-line stream
    let bytes_stream = response
        .bytes_stream()
        .map(|res| res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
    let reader = StreamReader::new(bytes_stream);
    let mut lines = FramedRead::new(reader, LinesCodec::new());

    while let Some(line) = lines.next().await {
        let line = line.context("error reading NDJSON line from Subsquid stream")?;
        if line.trim().is_empty() {
            continue;
        }

        let chunk: SubsquidBlockChunk = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed decoding NDJSON line: {e}\nLine content: {line}");
                continue;
            }
        };

        let block_num = chunk.header.number;
        let mut registry_activity = false;
        let mut webhook_activity = false;

        if let Some(logs) = chunk.logs {
            for log in logs {
                if log
                    .address
                    .eq_ignore_ascii_case(&format!("{factory_address:#x}"))
                {
                    registry_activity = true;
                } else if log
                    .address
                    .eq_ignore_ascii_case(&format!("{webhook_registry_address:#x}"))
                {
                    webhook_activity = true;
                }
            }
        }

        let tip = EvmTip {
            block_number: block_num,
            base_fee_per_gas: chunk.header.base_fee_per_gas,
            registry_activity,
            webhook_activity,
        };

        *current_block = block_num + 1;

        if tips_tx.send(tip).await.is_err() {
            return Ok(()); // Worker shutdown
        }
    }

    Ok(())
}

/// Retained HTTP JSON-RPC batch utility for contract deployment checks
pub async fn batch_check_deployed(
    http_rpc_url: &str,
    receivers: &[EthersAddress],
) -> Result<HashMap<EthersAddress, bool>> {
    if receivers.is_empty() {
        return Ok(HashMap::new());
    }

    let url = http_rpc_url
        .parse()
        .context("invalid EVM HTTP RPC URL for batched existence check")?;
    let client = ClientBuilder::default().http(url);
    let mut batch = client.new_batch();

    let mut waiters = Vec::with_capacity(receivers.len());
    for &addr in receivers {
        let alloy_addr = AlloyAddress::from(addr.0);
        let waiter = batch
            .add_call::<_, AlloyBytes>("eth_getCode", &(alloy_addr, "latest"))
            .with_context(|| format!("failed queuing eth_getCode for {addr:?}"))?;
        waiters.push((addr, waiter));
    }

    batch
        .send()
        .await
        .context("batched eth_getCode request failed")?;

    let mut out = HashMap::with_capacity(waiters.len());
    for (addr, waiter) in waiters {
        let code = waiter
            .await
            .with_context(|| format!("batched eth_getCode response missing for {addr:?}"))?;
        out.insert(addr, !code.is_empty());
    }
    Ok(out)
}

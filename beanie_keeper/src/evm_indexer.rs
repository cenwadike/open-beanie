use anyhow::{Context, Result, anyhow};
use ethers::abi::{ParamType, Token, decode as abi_decode};
use ethers::types::Address;
use ethers::utils::keccak256;

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use log::{debug, info, warn};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Deposit, EvmConfig};
use crate::log_cache::{LogCache, chunk_ranges};

/// Distinct from the Starknet scan IDs so the two chains' checkpoints
/// never collide in the one shared `LogCache` both sides open.
pub const REGISTRY_WEBHOOK_SCAN_ID: &str = "evm:registry_webhook";
/// Bumped from "evm:deposits". Under the old flow that checkpoint advanced
/// per HTTP batch while the deposits found were only handed back at the END of
/// the scan, so a stopped/crashed first backfill left a checkpoint that had
/// moved past deposits nobody ever swept or notified. A new id makes the next
/// run rescan from `deposit_start_block` instead of skipping them.
pub const DEPOSITS_SCAN_ID: &str = "evm:deposits:v2";

/// Portal's own worker-boundary batching already bounds how much a single
/// HTTP response can cover, but there's no reason to invite an
/// unreasonably large one — mirrors the old `log_chunk_blocks` *intent*
/// (a sane upper bound per request) without the old *reason* for it
/// (working around an RPC provider's hard `eth_getLogs` range cap, which
/// doesn't apply to Portal). Portal may return less than this per batch
/// regardless; the pagination loop below doesn't care either way.
///
/// This is the *starting* and maximum window size, not a fixed one —
/// `stream_logs` shrinks below this adaptively when a window proves too
/// heavy to answer within the client timeout (see `portal()`'s doc comment),
/// which is exactly the failure mode a high-log-volume contract like
/// Base's native USDC hits: the registry scan's two low-traffic addresses
/// never need to shrink, but a Transfer-event scan over USDC can.
const MAX_BLOCKS_PER_REQUEST: u64 = 1_000;

/// Window for the deposit scan. The filter (USDC address + Transfer topic +
/// `topic2` in our receivers) matches very few logs, so tiny windows only
/// multiply request count: at 1_000 blocks/request the shared limiter
/// (2 req/s) plus the per-batch pause capped throughput near ~750 blocks/sec,
/// i.e. hours per ten million blocks. `stream_logs` still halves the window
/// down to `MIN_BLOCKS_PER_REQUEST` if Portal times out, and grows it back.
const DEPOSIT_MAX_BLOCKS_PER_REQUEST: u64 = 20_000;

/// Never shrink the window below this, even after repeated timeouts —
/// past this point a slow response is Portal/network trouble, not window
/// size, and shrinking further would just multiply request count for no
/// benefit.
const MIN_BLOCKS_PER_REQUEST: u64 = 1_000;

/// After this many consecutive successful batches at a shrunk window
/// size, try doubling back toward `MAX_BLOCKS_PER_REQUEST`. Conservative
/// (slow to grow, fast to shrink) on purpose — a query that was too heavy
/// once is likely to be too heavy again at the same block density.
const GROW_AFTER_CONSECUTIVE_SUCCESSES: u32 = 5;

/// Chunk size for registry discovery. The registry filter matches very few
/// logs, so Portal answers even huge ranges quickly. This is also the unit
/// cached in `LogCache` (`put_chunk`/`get_chunk`): boundaries are anchored at
/// the registry start block via `chunk_ranges`, so they're identical on every
/// run and a completed chunk is never fetched again.
const REGISTRY_CHUNK_BLOCKS: u64 = 200_000;

/// A chunk is only cached once its end is at least this far behind the head,
/// so a shallow reorg near the tip can't leave stale logs in the cache.
const REORG_SAFETY_BLOCKS: u64 = 128;

const MERCHANT_REGISTERED_SIG: &str = "MerchantRegistered(address,address)";
const RECEIVER_ANNOUNCED_SIG: &str = "ReceiverAnnounced(address,address,bytes32,bytes32)";
const WEBHOOK_URL_SET_SIG: &str = "WebhookUrlSet(address,string)";
const TRANSFER_SIG: &str = "Transfer(address,address,uint256)";

/// CCTP routing credentials as carried by `ReceiverAnnounced`: the chain-name
/// key and mint recipient exactly as the factory hashed them into the
/// receiver's salt. Both zero means same-chain settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvmRoute {
    pub chain: [u8; 32],
    pub recipient: [u8; 32],
}

/// One receiver the registry told us about.
#[derive(Clone, Copy, Debug)]
pub struct EvmReceiverRecord {
    pub merchant: Address,
    pub receiver: Address,
    /// `Some` for receivers learned from `ReceiverAnnounced`. `MerchantRegistered`
    /// doesn't carry a route, but a registered receiver is already deployed, so
    /// nothing downstream needs one for it.
    pub route: Option<EvmRoute>,
}

fn topic_hex(sig: &str) -> String {
    format!("0x{}", hex::encode(keccak256(sig)))
}

fn addr_topic_hex(a: Address) -> String {
    // eth_getLogs-style indexed-address-as-topic: 12 zero bytes + 20
    // address bytes, 0x-prefixed.
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(a.as_bytes());
    format!("0x{}", hex::encode(buf))
}

fn parse_topic_addr(topic_hex: &str) -> Option<Address> {
    let bytes = hex::decode(topic_hex.trim_start_matches("0x")).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(Address::from_slice(&bytes[12..32]))
}

/// A client + rate limiter for one Subsquid Portal deployment. One
/// `EvmConfig` (Base, Arbitrum, ...) gets exactly one `PortalHandle`,
/// looked up/created by `portal()` below and reused for every request
/// that config makes.
pub struct PortalHandle {
    pub client: reqwest::Client,
    pub limiter: DefaultDirectRateLimiter,
}

/// Keyed by `subsquid_portal_url`, not a single global `static`. Two EVM
/// chains talking to two different Portal deployments (Base, Arbitrum)
/// must not share one rate-limit bucket or one set of default headers —
/// each gets its own `PortalHandle` here, built once and reused for
/// every request `cfg` makes for the rest of the process's life.
static PORTAL_CLIENTS: LazyLock<Mutex<HashMap<String, Arc<PortalHandle>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns this config's `PortalHandle`, building it on first use.
///
/// The client carries `x-api-key: cfg.subsquid_portal_api_key` as a
/// default header on every request — see sqd.dev/developers'
/// "Authentication" section: paths and payloads are identical between
/// the public and an authenticated/dedicated Portal, so sending the key
/// is the entire difference between sharing the public pool and getting
/// this chain its own capacity.
///
/// The limiter quota mirrors what the shared public Portal documents (20
/// requests per 10 seconds / 2 req/sec sustained, full burst). A
/// dedicated portal may grant more than that, but Subsquid doesn't
/// publish a fixed authenticated number, so each bucket stays at this
/// conservative default until told otherwise — the win here is
/// isolation (Base's pace no longer throttles Arbitrum's and vice
/// versa), not a higher ceiling.
///
/// Every call site that used to reach for the old `portal_http_client()`
/// / `SQD_RATE_LIMITER` statics should use `portal(cfg).client` /
/// `portal(cfg).limiter` instead.
pub fn portal(cfg: &EvmConfig) -> Arc<PortalHandle> {
    let mut clients = PORTAL_CLIENTS
        .lock()
        .expect("Subsquid portal client registry mutex poisoned");

    if let Some(handle) = clients.get(&cfg.subsquid_portal_url) {
        return handle.clone();
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(&cfg.subsquid_portal_api_key)
            .expect("BASE_SUBSQUID_PORTAL_API_KEY is not a valid HTTP header value"),
    );

    // Timeout-bounded, same reasoning as the old shared client: a single
    // slow/stuck Portal response should surface as a retryable error, not
    // hang indefinitely (reqwest's default is no timeout at all).
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .default_headers(headers)
        .build()
        .expect("failed building Subsquid Portal HTTP client");

    let quota = Quota::with_period(Duration::from_millis(500))
        .unwrap()
        .allow_burst(NonZeroU32::new(20).unwrap());
    let limiter = RateLimiter::direct(quota);

    let handle = Arc::new(PortalHandle { client, limiter });
    clients.insert(cfg.subsquid_portal_url.clone(), handle.clone());
    handle
}

/// Renders a duration in seconds as a short human string for progress
/// logging (`"3m12s"`, `"1h04m"`), or `"unknown"` once the observed rate
/// is zero/non-finite (e.g. the very first batch, before any throughput
/// has been measured yet).
fn format_eta(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "unknown".to_string();
    }
    let secs = seconds.round() as u64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// Wire shapes — only the fields we actually request via `fields.*`.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PortalBlock {
    header: PortalHeader,
    #[serde(default)]
    logs: Vec<PortalLog>,
}

#[derive(Deserialize)]
struct PortalHeader {
    number: u64,
}

#[derive(Deserialize)]
struct PortalLog {
    #[allow(unused)]
    address: String,
    topics: Vec<String>,
    data: String,
    #[serde(rename = "transactionHash")]
    transaction_hash: String,
}

/// One `logs` filter object — mirrors the Portal request schema's
/// `logs: [{ address, topic0, topic1, topic2, topic3 }]`. Multiple
/// addresses/topics within one object are OR'd by Portal, same semantics
/// as `eth_getLogs`'s array-valued `address`/`topics` fields, which is
/// exactly what let the old `discover_registry_activity` combine the
/// factory and webhook-registry scans into one call.
#[derive(Clone, Default)]
struct LogFilter {
    address: Vec<String>,
    topic0: Vec<String>,
    topic2: Vec<String>,
}

impl LogFilter {
    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        if !self.address.is_empty() {
            obj.insert("address".into(), json!(self.address));
        }
        if !self.topic0.is_empty() {
            obj.insert("topic0".into(), json!(self.topic0));
        }
        if !self.topic2.is_empty() {
            obj.insert("topic2".into(), json!(self.topic2));
        }
        Value::Object(obj)
    }
}

#[derive(Debug, Deserialize)]
pub struct SqdErrorEnvelope {
    pub error: SqdErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct SqdErrorDetail {
    pub r#type: SqdErrorType,
    pub code: Option<String>,
    pub message: String,
    pub param: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqdErrorType {
    InvalidRequestError,
    RateLimitError,
    AvailabilityError,
    ApiError,
    #[serde(other)]
    Unknown,
}

pub async fn handle_portal_response(
    resp: reqwest::Response,
    scan_id: &str,
    default_backoff_secs: u64,
) -> Result<Option<String>> {
    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }

    if resp.status().is_success() {
        let body = resp.text().await?;
        return Ok(Some(body));
    }

    let header_req_id = resp
        .headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .map(String::from);

    let retry_after_header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    let sqd_err = serde_json::from_str::<SqdErrorEnvelope>(&text)
        .ok()
        .map(|e| e.error);

    let err_type = sqd_err.as_ref().map(|e| &e.r#type);
    let request_id = sqd_err
        .as_ref()
        .and_then(|e| e.request_id.clone())
        .or(header_req_id)
        .unwrap_or_else(|| "unknown".into());

    // Branch on explicit SQD type or fallback to HTTP status code
    match err_type {
        Some(SqdErrorType::RateLimitError) => {
            let wait_secs = retry_after_header.unwrap_or(1);
            warn!("Rate limit for {scan_id} (req_id: {request_id}). Continue in {wait_secs}s...");
            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            Err(anyhow!("RATE_LIMIT_RETRYABLE"))
        }

        Some(SqdErrorType::AvailabilityError) => {
            let wait_secs = retry_after_header.unwrap_or(default_backoff_secs);
            warn!(
                "Portal temporarily unavailable for {scan_id} (req_id: {request_id}). Retrying in {wait_secs}s..."
            );
            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            Err(anyhow!("AVAILABILITY_RETRYABLE"))
        }

        Some(SqdErrorType::InvalidRequestError) => Err(anyhow!(
            "Fatal SQD Invalid Request Error ({status}) for {scan_id}: {text}"
        )),

        _ => {
            // Fallback status code checks for transient failure status codes (429, 529, 502, 503)
            if status.as_u16() == 429 || status.as_u16() == 529 {
                let wait_secs = retry_after_header.unwrap_or(1);
                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                return Err(anyhow!("RATE_LIMIT_RETRYABLE"));
            } else if status.as_u16() == 502 || status.as_u16() == 503 {
                let wait_secs = retry_after_header.unwrap_or(default_backoff_secs);
                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                return Err(anyhow!("AVAILABILITY_RETRYABLE"));
            }

            Err(anyhow!(
                "Fatal SQD API Error ({status}, req_id: {request_id}) for {scan_id}: {text}"
            ))
        }
    }
}

/// Streams `from_block..=to_block` from Portal in documented batches,
/// calling `on_block` for every block line as it arrives and durably
/// advancing `cache`'s checkpoint under `scan_id` after every HTTP
/// response (not once at the end — see the module doc). `on_block` gets
/// every block Portal returns, including ones with an empty `logs` array
/// (Portal only omits blocks that match nothing when a filter excludes
/// them at the server, which is exactly what we want — no client-side
/// filtering needed beyond what `logs` filter objects already express).
///
/// Returns the last block number actually observed (which may be less
/// than `to_block` if Portal's dataset hasn't caught up to it yet, e.g.
/// when the caller asks for a `to_block` derived from `/metadata` a
/// moment before Portal itself ingests it — the caller should re-derive
/// "how far did we get" from `cache.get_checkpoint`, not assume this
/// return value equals `to_block`).
async fn stream_logs(
    client: &reqwest::Client,
    limiter: &DefaultDirectRateLimiter,
    portal_url: &str,
    scan_id: &str,
    cache: &LogCache,
    from_block: u64,
    to_block: u64,
    filters: &[LogFilter],
    max_window: u64,
    persist_checkpoint: bool,
    mut on_block: impl FnMut(&PortalBlock),
) -> Result<Option<u64>> {
    if from_block > to_block {
        return if persist_checkpoint {
            Ok(cache.get_checkpoint(scan_id)?)
        } else {
            Ok(None)
        };
    }

    let stream_url = format!("{}/stream", portal_url.trim_end_matches('/'));
    let mut cursor = from_block;
    let mut last_seen: Option<u64> = None;

    // --- Progress tracking -------------------------------------------------
    // Previously the only signal a caller had that anything was happening
    // was a rate-limit warning or the final "COMPLETED" line — nothing in
    // between, even across a scan running for an hour+. `blocks_done` tracks
    // actual block-range progress (not HTTP requests, since a retried
    // request doesn't advance anything); `scan_start`/`last_progress_log`
    // gate how often we print so a fast-completing scan isn't spammed while
    // a slow one still reports every few seconds.
    let total_range = to_block - from_block + 1;
    let scan_start = Instant::now();
    let mut last_progress_log = Instant::now();
    let progress_log_interval = Duration::from_secs(10);

    // Adaptive window: starts at the max and shrinks when a window proves
    // too heavy to answer within the client timeout (see
    // `portal()`'s doc comment for why that's the failure mode
    // this exists for — a high-log-volume contract like USDC on Base can
    // make a 50k-block window genuinely too much for Portal to answer
    // quickly, and retrying the *same* window size just walks into the
    // same wall again). Grows back cautiously after sustained success so a
    // scan that only had one heavy stretch doesn't stay small forever.
    let mut batch_size = max_window;
    let mut consecutive_successes: u32 = 0;

    // debug, not info: this fires on every call into `stream_logs`,
    // including a routine single-block live-tip scan every ~2s. The
    // meaningful signal (what was actually found) is logged by the
    // caller once the scan completes — this line is just "a scan started",
    // which is only interesting while actively troubleshooting.
    debug!("{scan_id}: scanning block {from_block} through {to_block} ({total_range} block(s))");

    while cursor <= to_block {
        let batch_ceiling = std::cmp::min(cursor + batch_size - 1, to_block);

        let body = json!({
            "type": "evm",
            "fromBlock": cursor,
            "toBlock": batch_ceiling,
            "fields": {
                "block": { "number": true },
                "log": {
                    "address": true,
                    "topics": true,
                    "data": true,
                    "transactionHash": true,
                }
            },
            "logs": filters.iter().map(LogFilter::to_json).collect::<Vec<_>>(),
        });

        limiter.until_ready().await;
        let resp = match client.post(&stream_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                // Previously any send() error — including a plain timeout —
                // bailed the whole scan via `?`, which the caller
                // (`run_evm_catchup`) swallows with `if let Ok(...)`,
                // meaning a single stuck request would silently abort the
                // entire catch-up with nothing logged. Treat it the same as
                // a rate-limit/availability error instead: retry, but with
                // a smaller window — a timeout at `batch_size` blocks means
                // *this* window was too heavy to answer in time, and
                // retrying the identical range at the identical size would
                // just time out again.
                consecutive_successes = 0;
                let old_batch_size = batch_size;
                batch_size = (batch_size / 2).max(MIN_BLOCKS_PER_REQUEST);
                warn!(
                    "Subsquid Portal request for {scan_id} (blocks {cursor}-{batch_ceiling}, \
                     {old_batch_size} block window) timed out or failed to connect: {e:#}. \
                     Shrinking window to {batch_size} block(s) and retrying in 5s..."
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Subsquid Portal request failed for {scan_id}"));
            }
        };

        // Process response via SQD Specification handler
        let text = match handle_portal_response(resp, scan_id, 5).await {
            Ok(Some(body)) => body,
            Ok(None) => break, // Received 204 No Content -> No blocks in range yet
            Err(e) => {
                let err_str = e.to_string();
                if err_str == "RATE_LIMIT_RETRYABLE" || err_str == "AVAILABILITY_RETRYABLE" {
                    continue; // Retry the same range again after backoff sleep
                }
                return Err(e); // Fatal error (InvalidRequestError, ApiError, or unparsed)
            }
        };

        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            break;
        }

        let mut batch_last: Option<u64> = None;
        for line in &lines {
            let block: PortalBlock = serde_json::from_str(line).with_context(|| {
                format!("failed decoding Portal NDJSON line for {scan_id}: {line}")
            })?;
            on_block(&block);
            batch_last = Some(block.header.number);
        }

        let batch_last = match batch_last {
            Some(n) => n,
            None => break,
        };

        // Durable checkpoint advance — per HTTP response, exactly the
        // granularity the Portal docs' "Stream continuation" section
        // warns is necessary. A crash on the next line resumes from here,
        // not from `from_block`.
        if persist_checkpoint {
            cache.set_checkpoint(scan_id, batch_last)?;
        }
        last_seen = Some(batch_last);
        cursor = batch_last + 1;

        let reached_end = batch_last >= to_block;

        if batch_size < max_window {
            consecutive_successes += 1;
            if consecutive_successes >= GROW_AFTER_CONSECUTIVE_SUCCESSES {
                let old_batch_size = batch_size;
                batch_size = (batch_size * 2).min(max_window);
                consecutive_successes = 0;
                info!(
                    "{scan_id}: {GROW_AFTER_CONSECUTIVE_SUCCESSES} consecutive successful \
                     batches at {old_batch_size} block(s) — growing window back to {batch_size}"
                );
            }
        }

        // Periodic progress log: cursor position, % of range covered,
        // observed throughput, and a rough ETA. Throttled to once per
        // `progress_log_interval` (plus always on the final batch) so a
        // fast scan doesn't get spammed while a slow one stays visible.
        if reached_end || last_progress_log.elapsed() >= progress_log_interval {
            let blocks_done = batch_last.saturating_sub(from_block) + 1;
            let elapsed_secs = scan_start.elapsed().as_secs_f64();
            let blocks_per_sec = if elapsed_secs > 0.0 {
                blocks_done as f64 / elapsed_secs
            } else {
                0.0
            };
            let pct = blocks_done as f64 / total_range as f64 * 100.0;
            let remaining = to_block.saturating_sub(batch_last);
            let eta = if blocks_per_sec > 0.0 {
                format_eta(remaining as f64 / blocks_per_sec)
            } else {
                "unknown".to_string()
            };
            // debug, not info: throughput/ETA progress is only useful
            // while watching a long first-time backfill; on the steady-
            // state live path (1-block ranges, every tip) this was the
            // single largest source of log volume for zero new signal.
            debug!(
                "{scan_id}: block {batch_last} of {to_block} ({pct:.1}%), \
                 ~{blocks_per_sec:.0} blocks/sec, {remaining} block(s) remaining, \
                 window {batch_size}, ETA {eta}"
            );
            last_progress_log = Instant::now();
        }

        if reached_end {
            break;
        }

        // `limiter` already paces requests; this is only a short
        // courtesy pause (it used to be a flat 1s after every batch, even the
        // last, which alone capped a 1_000-block scan near 1_000 blocks/sec).
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(last_seen.or_else(|| {
        if persist_checkpoint {
            cache.get_checkpoint(scan_id).ok().flatten()
        } else {
            None
        }
    }))
}

/// Fetches current chain head directly from SQD Portal /head endpoint.
pub async fn current_head(cfg: &EvmConfig) -> Result<u64> {
    let base_url = cfg.subsquid_portal_url.trim_end_matches('/');
    let client = &portal(cfg).client;

    // 1. First try the official dataset /head endpoint
    let head_url = format!("{base_url}/head");
    if let Ok(resp) = client.get(&head_url).send().await {
        if resp.status().is_success() {
            if let Ok(text) = resp.text().await {
                // Response is a plain JSON number or object with height
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(num) = val
                        .as_u64()
                        .or_else(|| val.get("height").and_then(|h| h.as_u64()))
                    {
                        return Ok(num);
                    }
                }
            }
        }
    }

    // 2. Fallback: Query a 1-block range stream and read `x-sqd-head-number` header
    let stream_url = format!("{base_url}/stream");
    let body = serde_json::json!({
        "type": "evm",
        "fromBlock": 0,
        "toBlock": 0,
        "fields": {
            "block": { "number": true }
        }
    });

    let resp = client
        .post(&stream_url)
        .json(&body)
        .send()
        .await
        .context("SQD Portal stream request failed for head query")?;

    if let Some(head_hdr) = resp.headers().get("x-sqd-head-number") {
        if let Ok(head_str) = head_hdr.to_str() {
            if let Ok(head) = head_str.parse::<u64>() {
                return Ok(head);
            }
        }
    }

    Err(anyhow!("Could not fetch block height from SQD Portal"))
}

/// Combined merchant-registry + webhook-registry discovery. Same contract
/// as the RPC-era `discover_registry_activity` it replaces: no
/// `from_block` parameter — resumes from `LogCache::get_checkpoint`,
/// falling back to `min(registry_start_block, webhook_registry_start_block)`
/// on first run. Returns whatever was found up through wherever the
/// checkpoint actually landed (which may be less than `to_block` if
/// Portal rate-limited us or its dataset hasn't caught up yet) — the
/// checkpoint on disk is always the honest source of truth for "how far
/// did this actually get," never this function's return value alone.
pub async fn discover_registry_activity(
    cfg: &EvmConfig,
    cache: &LogCache,
    to_block: u64,
) -> Result<(Vec<EvmReceiverRecord>, Vec<(Address, String)>)> {
    // debug, not info: this is called from `process_evm_tip` on nearly
    // every qualifying live tip, not just at startup. The overwhelming
    // majority of calls find nothing new — see the conditional level below.
    debug!("Catching up on Beanie EVM Registry started, this may take a while");
    let from_block = cache
        .get_checkpoint(REGISTRY_WEBHOOK_SCAN_ID)?
        .map(|b| b + 1)
        .unwrap_or_else(|| {
            std::cmp::min(cfg.registry_start_block, cfg.webhook_registry_start_block)
        });

    if from_block > to_block {
        return Ok((Vec::new(), Vec::new()));
    }

    let (merchants, webhooks, _) = discover_registry_range(
        cfg,
        cache,
        from_block,
        to_block,
        MAX_BLOCKS_PER_REQUEST,
        true,
    )
    .await?;

    // A real registration/webhook-URL event is genuinely rare and worth
    // seeing at info level; "checked, found nothing" is the steady-state
    // outcome of nearly every call and belongs at debug so it doesn't
    // drown out the lines that actually matter.
    if merchants.is_empty() && webhooks.is_empty() {
        debug!(
            "Catching up on Beanie Registry: COMPLETED (0 merchant(s), 0 webhook(s) found, through block {to_block})"
        );
    } else {
        info!(
            "Catching up on Beanie Registry: COMPLETED ({} merchant(s), {} webhook(s) found, through block {to_block})",
            merchants.len(),
            webhooks.len()
        );
    }
    Ok((merchants, webhooks))
}

/// The COMPLETE registry state as of `scanned_to`, independent of any saved
/// checkpoint.
pub struct RegistrySnapshot {
    pub merchants: Vec<EvmReceiverRecord>,
    pub webhooks: Vec<(Address, String)>,
    pub scanned_to: u64,
}

/// One cached registry chunk. Plain hex strings (not ethers types) so the
/// cache stays chain-library agnostic, as `log_cache.rs` intends.
#[derive(Serialize, Deserialize)]
struct RegistryChunk {
    merchants: Vec<CachedReceiver>,
    /// (merchant, url), in block order — later entries win
    webhooks: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct CachedReceiver {
    merchant: String,
    receiver: String,
    /// 0x-hex bytes32, present only for `ReceiverAnnounced` rows.
    #[serde(default)]
    cctp_chain: Option<String>,
    #[serde(default)]
    cctp_recipient: Option<String>,
}

fn bytes32_hex(b: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(b))
}

fn parse_bytes32(s: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

impl RegistryChunk {
    fn from_found(merchants: &[EvmReceiverRecord], webhooks: &[(Address, String)]) -> Self {
        Self {
            merchants: merchants
                .iter()
                .map(|rec| CachedReceiver {
                    merchant: format!("{:?}", rec.merchant),
                    receiver: format!("{:?}", rec.receiver),
                    cctp_chain: rec.route.map(|r| bytes32_hex(&r.chain)),
                    cctp_recipient: rec.route.map(|r| bytes32_hex(&r.recipient)),
                })
                .collect(),
            webhooks: webhooks
                .iter()
                .map(|(m, u)| (format!("{m:?}"), u.clone()))
                .collect(),
        }
    }

    fn into_found(self) -> (Vec<EvmReceiverRecord>, Vec<(Address, String)>) {
        let merchants = self
            .merchants
            .iter()
            .filter_map(|c| {
                let route = match (&c.cctp_chain, &c.cctp_recipient) {
                    (Some(chain), Some(recipient)) => Some(EvmRoute {
                        chain: parse_bytes32(chain)?,
                        recipient: parse_bytes32(recipient)?,
                    }),
                    _ => None,
                };
                Some(EvmReceiverRecord {
                    merchant: c.merchant.parse::<Address>().ok()?,
                    receiver: c.receiver.parse::<Address>().ok()?,
                    route,
                })
            })
            .collect();
        let webhooks = self
            .webhooks
            .into_iter()
            .filter_map(|(m, u)| Some((m.parse::<Address>().ok()?, u)))
            .collect();
        (merchants, webhooks)
    }
}

/// Returns the COMPLETE merchant/receiver/webhook set as of `scanned_to`.
///
/// Why this exists: `discover_registry_activity` resumes from the cache
/// checkpoint, so after any restart it returns only what was registered
/// *since the last run* (usually nothing). The in-memory receiver map — which
/// the deposit scan's `topic2` filter is built from — must hold every
/// receiver, not just new ones. The checkpoint says how far we scanned, but
/// not what we found; the found results live in `LogCache`'s chunk store.
///
/// The range is split with `chunk_ranges` into fixed `REGISTRY_CHUNK_BLOCKS`
/// chunks anchored at the registry start block. Each *settled* full chunk is
/// read from the cache if present (no network), otherwise fetched from Portal
/// and `put_chunk`ed immediately. So the first run pays for full history once
/// (and a crash mid-way keeps every finished chunk), and later runs only
/// fetch chunks that weren't complete yet — in practice the partial tail
/// chunk near the head, which is never cached.
///
/// The scan id includes the contract addresses, so cached chunks from a
/// different deployment sharing the same cache path are never reused.
pub async fn rebuild_registry_state(
    cfg: &EvmConfig,
    cache: &LogCache,
    to_block: u64,
) -> Result<RegistrySnapshot> {
    let from_block = std::cmp::min(cfg.registry_start_block, cfg.webhook_registry_start_block);
    let scan_id = format!(
        "evm:registry_chunks:{:?}:{:?}",
        cfg.factory_address, cfg.webhook_registry_address
    );

    info!("Rebuilding EVM registry state: blocks {from_block}..={to_block}");

    let mut merchants: Vec<EvmReceiverRecord> = Vec::new();
    let mut webhooks: Vec<(Address, String)> = Vec::new();
    let mut scanned_to: Option<u64> = None;
    let (mut cache_hits, mut fetched) = (0u32, 0u32);
    let total_chunks = chunk_ranges(from_block, to_block, REGISTRY_CHUNK_BLOCKS).count();

    for (idx, (chunk_start, chunk_end)) in
        chunk_ranges(from_block, to_block, REGISTRY_CHUNK_BLOCKS).enumerate()
    {
        let is_full = chunk_end - chunk_start + 1 == REGISTRY_CHUNK_BLOCKS;
        let settled = is_full && chunk_end + REORG_SAFETY_BLOCKS <= to_block;

        if settled {
            match cache.get_chunk::<RegistryChunk>(&scan_id, chunk_start, chunk_end) {
                Ok(Some(chunk)) => {
                    let (m, w) = chunk.into_found();
                    merchants.extend(m);
                    webhooks.extend(w);
                    scanned_to = Some(chunk_end);
                    cache_hits += 1;
                    continue;
                }
                Ok(None) => {}
                Err(e) => warn!(
                    "unreadable cached registry chunk {chunk_start}..={chunk_end}: {e:#} — refetching"
                ),
            }
        }

        info!(
            "Registry chunk {}/{total_chunks} (blocks {chunk_start}..={chunk_end}): not cached, fetching from Portal",
            idx + 1
        );
        let (m, w, last_seen) = discover_registry_range(
            cfg,
            cache,
            chunk_start,
            chunk_end,
            REGISTRY_CHUNK_BLOCKS,
            false,
        )
        .await?;

        // Portal hasn't ingested any of this chunk yet — stop here with
        // whatever earlier chunks gave us.
        let Some(last) = last_seen else { break };

        // Only a chunk Portal fully covered is safe to cache.
        if settled && last >= chunk_end {
            if let Err(e) = cache.put_chunk(
                &scan_id,
                chunk_start,
                chunk_end,
                &RegistryChunk::from_found(&m, &w),
            ) {
                warn!("failed caching registry chunk {chunk_start}..={chunk_end}: {e:#}");
            }
        }

        merchants.extend(m);
        webhooks.extend(w);
        scanned_to = Some(last);
        fetched += 1;

        if last < chunk_end {
            break; // Portal is behind the head we asked for
        }
    }

    let scanned_to = scanned_to.ok_or_else(|| {
        anyhow!("registry rebuild made no progress (Portal returned no blocks up to {to_block})")
    })?;

    // Keep the live path's checkpoint in step (forward only), so
    // `process_evm_tip` only scans what comes after this point.
    let existing = cache.get_checkpoint(REGISTRY_WEBHOOK_SCAN_ID)?;
    if existing.map_or(true, |cp| cp < scanned_to) {
        cache.set_checkpoint(REGISTRY_WEBHOOK_SCAN_ID, scanned_to)?;
    }

    info!(
        "Registry rebuild COMPLETE: {} merchant/receiver pair(s), {} webhook(s), through block {scanned_to} ({cache_hits} chunk(s) from cache, {fetched} fetched)",
        merchants.len(),
        webhooks.len()
    );
    Ok(RegistrySnapshot {
        merchants,
        webhooks,
        scanned_to,
    })
}

/// Shared scanner behind both the checkpoint-resuming and full-rebuild
/// paths. Returns (merchants, webhooks, last block Portal reported).
async fn discover_registry_range(
    cfg: &EvmConfig,
    cache: &LogCache,
    from_block: u64,
    to_block: u64,
    max_window: u64,
    persist_checkpoint: bool,
) -> Result<(Vec<EvmReceiverRecord>, Vec<(Address, String)>, Option<u64>)> {
    let merchant_reg_topic = topic_hex(MERCHANT_REGISTERED_SIG);
    let receiver_announced_topic = topic_hex(RECEIVER_ANNOUNCED_SIG);
    let webhook_set_topic = topic_hex(WEBHOOK_URL_SET_SIG);

    let filter = LogFilter {
        address: vec![
            format!("{:?}", cfg.factory_address).to_lowercase(),
            format!("{:?}", cfg.webhook_registry_address).to_lowercase(),
        ],
        topic0: vec![
            merchant_reg_topic.clone(),
            receiver_announced_topic.clone(),
            webhook_set_topic.clone(),
        ],
        topic2: Vec::new(),
    };

    let mut merchants = Vec::new();
    let mut webhooks = Vec::new();

    let handle = portal(cfg);
    let last_seen = stream_logs(
        &handle.client,
        &handle.limiter,
        &cfg.subsquid_portal_url,
        REGISTRY_WEBHOOK_SCAN_ID,
        cache,
        from_block,
        to_block,
        std::slice::from_ref(&filter),
        max_window,
        persist_checkpoint,
        |block| {
            for log in &block.logs {
                if log.topics.is_empty() {
                    continue;
                }
                let topic0 = log.topics[0].to_lowercase();
                let data_bytes = hex::decode(log.data.trim_start_matches("0x")).unwrap_or_default();

                if topic0 == merchant_reg_topic {
                    if log.topics.len() < 2 || data_bytes.len() < 32 {
                        continue;
                    }
                    let Some(merchant) = parse_topic_addr(&log.topics[1]) else {
                        continue;
                    };
                    let receiver = Address::from_slice(&data_bytes[12..32]);
                    merchants.push(EvmReceiverRecord {
                        merchant,
                        receiver,
                        route: None,
                    });
                } else if topic0 == receiver_announced_topic {
                    // data = cctpMintChain | cctpMintRecipient, 32 bytes each
                    if log.topics.len() < 3 || data_bytes.len() < 64 {
                        continue;
                    }
                    let (Some(merchant), Some(receiver)) = (
                        parse_topic_addr(&log.topics[1]),
                        parse_topic_addr(&log.topics[2]),
                    ) else {
                        continue;
                    };
                    let mut chain = [0u8; 32];
                    let mut recipient = [0u8; 32];
                    chain.copy_from_slice(&data_bytes[0..32]);
                    recipient.copy_from_slice(&data_bytes[32..64]);
                    merchants.push(EvmReceiverRecord {
                        merchant,
                        receiver,
                        route: Some(EvmRoute { chain, recipient }),
                    });
                } else if topic0 == webhook_set_topic {
                    if log.topics.len() < 2 {
                        continue;
                    }
                    let Some(merchant) = parse_topic_addr(&log.topics[1]) else {
                        continue;
                    };
                    match abi_decode(&[ParamType::String], &data_bytes) {
                        Ok(mut tokens) => {
                            if let Token::String(s) = tokens.remove(0) {
                                webhooks.push((merchant, s));
                            }
                        }
                        Err(_) => continue,
                    }
                }
            }
        },
    )
    .await
    .with_context(|| "evm registry/webhook discovery via Subsquid Portal failed")?;

    Ok((merchants, webhooks, last_seen))
}

/// Deposit discovery for a known receiver set. Same contract as the
/// RPC-era `fetch_deposits_since_block`: `receivers` must be the complete
/// set known as of `to_block` (the caller runs registry discovery first,
/// same ordering requirement as before — see `discover_registry_activity`'s
/// doc and the Phase 0 note in `transfer_workers.rs`).
///
/// The receiver set is folded into `topic2` (the indexed `to` param) as
/// an OR list, the same trick `eth_getLogs`'s array-valued `topics` did
/// before — Portal's docs confirm multiple values within one filter
/// object are OR'd. This is *why* Phase 2 can just re-ask Portal every
/// tick with the current receiver set instead of needing a
/// subscription-that-stays-in-sync-with-a-growing-list the way a
/// websocket log subscription would have (see the deleted half of
/// `evm_ws.rs` for that rejected approach) — a stateless request can
/// always just include the current, complete list.
pub async fn fetch_deposits_since_block(
    cfg: &EvmConfig,
    cache: &LogCache,
    receivers: &[Address],
    to_block: u64,
) -> Result<Vec<Deposit>> {
    if receivers.is_empty() {
        // This used to return silently *after* logging "started", which is
        // exactly what made an empty receiver set look like a stalled scan.
        // Still worth keeping visible at debug (not silent) for that same
        // reason, but not info: with no receivers this fires every tip,
        // forever, until the first merchant registers.
        debug!("Beanie EVM deposit scan skipped: no receivers known");
        return Ok(Vec::new());
    }
    // debug, not info: `fetch_deposits_since_block` is called unconditionally
    // on every tip (no activity flag gates it — see transfer_workers.rs),
    // so on a quiet chain this fires roughly once per block, forever. The
    // "COMPLETED" line below is where the real signal (a deposit found)
    // shows up, and only there is it worth info level.
    debug!(
        "Catching up on Beanie EVM Deposits started ({} receiver(s)), this may take a while",
        receivers.len()
    );

    let from_block = cache
        .get_checkpoint(DEPOSITS_SCAN_ID)?
        .map(|b| b + 1)
        .unwrap_or(cfg.deposit_start_block);

    if from_block > to_block {
        return Ok(Vec::new());
    }

    let transfer_topic = topic_hex(TRANSFER_SIG);
    let filter = LogFilter {
        address: vec![format!("{:?}", cfg.token_address).to_lowercase()],
        topic0: vec![transfer_topic.clone()],
        topic2: receivers.iter().map(|&a| addr_topic_hex(a)).collect(),
    };

    let mut deposits = Vec::new();
    let handle = portal(cfg);

    // persist_checkpoint = false: the deposits found are only returned to the
    // caller when the whole scan finishes, so the checkpoint must not run
    // ahead of them. It is written once, below, after the scan completes.
    let last_seen = stream_logs(
        &handle.client,
        &handle.limiter,
        &cfg.subsquid_portal_url,
        DEPOSITS_SCAN_ID,
        cache,
        from_block,
        to_block,
        std::slice::from_ref(&filter),
        DEPOSIT_MAX_BLOCKS_PER_REQUEST,
        false,
        |block| {
            for log in &block.logs {
                if log.topics.len() < 3 || log.topics[0].to_lowercase() != transfer_topic {
                    continue;
                }
                let (Some(from), Some(to)) = (
                    parse_topic_addr(&log.topics[1]),
                    parse_topic_addr(&log.topics[2]),
                ) else {
                    continue;
                };
                let data_bytes = hex::decode(log.data.trim_start_matches("0x")).unwrap_or_default();
                let amount_raw = if data_bytes.len() >= 32 {
                    ethers::types::U256::from_big_endian(&data_bytes[..32]).to_string()
                } else {
                    "0".to_string()
                };
                deposits.push(Deposit {
                    tx_hash: log.transaction_hash.clone(),
                    from_address: format!("{from:?}"),
                    receiver: format!("{to:?}"),
                    amount_raw,
                    block_number: block.header.number,
                });
            }
        },
    )
    .await
    .with_context(|| "evm deposit discovery via Subsquid Portal failed")?;

    if let Some(scanned_to) = last_seen {
        cache.set_checkpoint(DEPOSITS_SCAN_ID, scanned_to)?;
    }

    deposits.sort_by_key(|d| d.block_number);
    // This used to say "STARTED" at the exact point deposits scanning
    // *finished* — a copy/paste leftover that made a completed scan look
    // like it had barely begun in the log, part of why this looked like a
    // silent hang.
    info!(
        "Catching up on Beanie EVM Deposits: COMPLETED ({} deposit(s) found, through block {to_block})",
        deposits.len()
    );
    Ok(deposits)
}

/// Phase 1 entry point: startup catch-up to current head. Runs registry
/// discovery first, then deposit discovery over the now-known receiver
/// set — same ordering requirement `act_on_evm_deposits`'s caller already
/// documents (a deposit can't be attributed to a receiver that hasn't
/// been folded into `EvmState.merchant_map` yet).
///
/// Unlike the old `run_evm_backfill`, this is not optional / gated on an
/// API key being configured — `subsquid_portal_url` is required
/// (`EvmConfig::from_env` already enforces that), so this always runs.
pub struct EvmCatchupSummary {
    pub merchants: Vec<EvmReceiverRecord>,
    pub webhooks: Vec<(Address, String)>,
    pub deposits: Vec<Deposit>,
    pub caught_up_to_block: Option<u64>,
}

pub async fn run_evm_catchup(cfg: &EvmConfig, cache: &LogCache) -> Result<EvmCatchupSummary> {
    let head = current_head(cfg)
        .await
        .context("failed fetching current head from Subsquid Portal")?;

    // Always rebuild the FULL registry state here — never just the delta
    // since the saved checkpoint. The deposit filter is built from this set.
    let snapshot = rebuild_registry_state(cfg, cache, head).await?;

    let mut receivers: Vec<Address> = snapshot.merchants.iter().map(|rec| rec.receiver).collect();
    receivers.sort();
    receivers.dedup();
    info!(
        "EVM catch-up: {} receiver(s) known through block {}",
        receivers.len(),
        snapshot.scanned_to
    );

    // Deposits are only scanned up to the block the registry is known
    // through, so a receiver can never be missing from the filter for a
    // block range we then mark as scanned.
    let deposits = fetch_deposits_since_block(cfg, cache, &receivers, snapshot.scanned_to).await?;

    cache
        .flush()
        .context("failed flushing log cache after evm catch-up")?;

    Ok(EvmCatchupSummary {
        merchants: snapshot.merchants,
        webhooks: snapshot.webhooks,
        deposits,
        caught_up_to_block: Some(snapshot.scanned_to),
    })
}

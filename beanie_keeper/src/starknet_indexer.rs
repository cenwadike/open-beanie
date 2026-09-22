//! Starknet event discovery, sourced via the standard `starknet_getEvents`
//! JSON-RPC method (currently pointed at Starkscan's rpc-beta proxy, but
//! works against any Starknet RPC node that implements the method).
//!
//! Replaces the Apibara DNA gRPC stream: Apibara requires either paying for
//! their hosted service or running your own indexer node. `getEvents` is
//! served directly by RPC nodes, so this module now polls a paginated
//! request/response API instead of subscribing to a push stream. That
//! changes the checkpointing model (see `discover_merchants` and
//! `fetch_deposits_since_block` below) but the public contract — "what
//! merchants/deposits appeared between the checkpoint and to_block" — is
//! unchanged, so callers (`run_starknet_catchup`, etc.) don't need to change.

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde::Deserialize;
use serde_json::{Value, json};
use starknet::core::types::Felt;
use starknet::core::utils::get_selector_from_name;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::config::{Deposit, StarknetConfig};
use crate::log_cache::LogCache;

/// CCTP routing credentials as carried by `ReceiverAnnounced`, in the exact
/// felts the factory hashed into the receiver's deploy salt (chain as a short
/// string, recipient as the u256's low/high halves). All zero = same-chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StarknetRoute {
    pub chain: Felt,
    pub recipient_low: Felt,
    pub recipient_high: Felt,
}

/// One receiver the registry told us about.
#[derive(Clone, Copy, Debug)]
pub struct StarknetReceiverRecord {
    pub merchant: Felt,
    pub receiver: Felt,
    /// `Some` for receivers learned from `ReceiverAnnounced`. `MerchantRegistered`
    /// doesn't carry a route, but a registered receiver is already deployed.
    pub route: Option<StarknetRoute>,
}

pub const STARKNET_REGISTRY_SCAN_ID: &str = "starknet:registry";
pub const STARKNET_DEPOSITS_SCAN_ID: &str = "starknet:deposits";

/// Events per page. Kept conservative since the Starkscan RPC proxy this
/// currently points at is still tagged `rpc-beta` — raise once you've
/// confirmed larger chunk sizes don't get throttled or truncated.
const CHUNK_SIZE: u32 = 1_000;

/// Maximum block range allowed in a single `starknet_getEvents` query span
/// to prevent proxy RPC errors (-32602) on large backfill gaps.
const MAX_BLOCK_RANGE: u64 = 1_000;

/// Retry a 429 (or an equivalent application-level saturation signal) this
/// many times before giving up on a single page fetch.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;

/// Starkscan's JSON-RPC error code for "gateway locally saturated" — an
/// overload signal, not a real query error. Retried with the same backoff
/// as an HTTP 429 rather than failing the whole scan on the spot.
const STARKSCAN_GATEWAY_SATURATED: i64 = -32005;

/// One `reqwest::Client`, built once and reused for every `starknet_getEvents`
/// call this process makes.
///
/// Previously `fetch_all_events` built a fresh `reqwest::Client` on every
/// invocation, i.e. on every live tip and every backfill chunk — each one
/// its own TCP/TLS handshake, none of them reusing a pooled connection to
/// the Starkscan proxy. Against an already rate-limit-sensitive `rpc-beta`
/// gateway, that connection churn made saturation and dropped connections
/// worse, not just wasteful. Following the same "one shared instance is
/// the correct pattern" reasoning `LogCache::open`'s doc comment lays out,
/// this is opened once and handed out by reference from then on.
fn shared_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .tcp_keepalive(Duration::from_secs(15))
            // Same gap as the Subsquid Portal client in evm_indexer.rs had:
            // no timeout meant a stuck Starkscan `rpc-beta` response could
            // hang a page fetch indefinitely with nothing logged. 30s is
            // generous for one `starknet_getEvents` page; past that it's
            // now retried like any other transient failure below, not left
            // to hang silently.
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed building shared Starknet HTTP client")
    })
}

/// Minimum spacing enforced between every outgoing `starknet_getEvents`
/// request this process makes — successes and retries alike.
///
/// — tighten or loosen it once you've confirmed the real per-key budget.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1100);

/// Tracks when the last request to Starkscan was *sent* (not when it
/// finished), so back-to-back calls — across `discover_merchants`,
/// `fetch_deposits_since_block`, and every retry inside either — are all
/// paced against the same clock instead of pacing per-caller.
fn request_pacer() -> &'static tokio::sync::Mutex<Instant> {
    static PACER: OnceLock<tokio::sync::Mutex<Instant>> = OnceLock::new();
    PACER.get_or_init(|| tokio::sync::Mutex::new(Instant::now() - MIN_REQUEST_INTERVAL))
}

/// Blocks until at least `MIN_REQUEST_INTERVAL` has passed since the last
/// request this process sent to Starkscan, then reserves this slot.
async fn wait_for_request_slot() {
    let mut last_sent = request_pacer().lock().await;
    let elapsed = last_sent.elapsed();
    if elapsed < MIN_REQUEST_INTERVAL {
        tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
    }
    *last_sent = Instant::now();
}

#[derive(Debug, Clone, Copy)]
pub struct StarknetTip {
    pub block_number: u64,
}

struct DecodedEvent {
    selector: Felt,
    keys: Vec<Felt>,
    data: Vec<Felt>,
    transaction_hash: String,
    block_number: u64,
}

fn transfer_selector() -> Result<Felt> {
    get_selector_from_name("Transfer").context("failed computing Transfer event selector")
}

fn merchant_registered_selector() -> Result<Felt> {
    get_selector_from_name("MerchantRegistered")
        .context("failed computing MerchantRegistered event selector")
}

fn receiver_announced_selector() -> Result<Felt> {
    get_selector_from_name("ReceiverAnnounced")
        .context("failed computing ReceiverAnnounced event selector")
}

fn parse_felt(s: &str) -> Result<Felt> {
    Felt::from_hex(s).with_context(|| format!("RPC returned a non-felt value: {s}"))
}

// ---- starknet_getEvents wire types -----------------------------------

#[derive(Deserialize)]
struct RpcEnvelope {
    result: Option<EventsPage>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct EventsPage {
    events: Vec<RawEvent>,
    continuation_token: Option<String>,
}

#[derive(Deserialize)]
struct RawEvent {
    from_address: String,
    keys: Vec<String>,
    data: Vec<String>,
    block_number: u64,
    transaction_hash: String,
}

/// Outcome of a single, no-retry attempt at one `starknet_getEvents` page.
enum PageAttempt {
    Success(EventsPage),
    /// A transient, worth-retrying condition: HTTP 429, Starkscan's
    /// "gateway locally saturated" RPC code, or an envelope with neither
    /// `result` nor `error` populated (also observed under gateway
    /// overload — see the match on `envelope.result` below). `retry_after`
    /// carries the server's own suggested wait when a `Retry-After` header
    /// was present, so the caller can prefer it over a guessed backoff.
    Retryable {
        reason: String,
        retry_after: Option<Duration>,
    },
}

/// One attempt at one page, and nothing else — no retry counting, no
/// sleeping. Mirrors `stream_subsquid_once` in `evm_ws.rs`: that function's
/// only job is to try the Subsquid connection once and report what
/// happened (`Result<()>`, ended by any error); all reconnect/backoff
/// logic lives solely in its caller, `run_evm_subscription`. `get_events_page`
/// below is that caller here — the sole place that loops, sleeps, and logs.
async fn fetch_events_page_once(
    client: &reqwest::Client,
    cfg: &StarknetConfig,
    body: &Value,
) -> Result<PageAttempt> {
    let mut request = client.post(&cfg.starknet_events_rpc_url);
    if let Some(api_key) = cfg.starknet_events_api_key.as_deref() {
        request = request.header("X-Starkscan-Api-Key", api_key);
    }
    let response = match request.json(body).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() || e.is_connect() => {
            // Previously this `?`'d straight out of the function on any
            // send() error, including a plain timeout — which the caller
            // chain (`fetch_all_events` -> `discover_merchants` /
            // `fetch_deposits_since_block` -> `run_starknet_catchup`) never
            // logs on failure, since `run_starknet_worker` just does
            // `if let Ok(summary) = ...`. A single stuck request could
            // silently abort the entire catch-up. Treat it as retryable
            // instead, same bucket as a 429 or gateway-saturated response.
            return Ok(PageAttempt::Retryable {
                reason: format!("starknet_getEvents request timed out or failed to connect ({e})"),
                retry_after: None,
            });
        }
        Err(e) => return Err(e).context("starknet_getEvents request failed"),
    };

    if response.status().as_u16() == 429 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Ok(PageAttempt::Retryable {
            reason: "rate-limited (HTTP 429)".to_string(),
            retry_after,
        });
    }

    let raw: Value = response
        .json()
        .await
        .context("starknet_getEvents response was not valid JSON")?;
    let envelope: RpcEnvelope = serde_json::from_value(raw)
        .context("starknet_getEvents response did not match the expected envelope")?;

    if let Some(err) = envelope.error {
        if err.code == STARKSCAN_GATEWAY_SATURATED {
            return Ok(PageAttempt::Retryable {
                reason: format!("rate-limited (RPC {}: {})", err.code, err.message),
                retry_after: None,
            });
        }
        bail!("starknet_getEvents RPC error {}: {}", err.code, err.message);
    }

    match envelope.result {
        Some(page) => Ok(PageAttempt::Success(page)),
        // Observed under gateway overload: a 200 with neither "result" nor
        // "error" populated. Treated as the same kind of transient
        // saturation signal as the other two cases above.
        None => Ok(PageAttempt::Retryable {
            reason: "returned neither a result nor an error".to_string(),
            retry_after: None,
        }),
    }
}

/// One page of `starknet_getEvents`, retried with backoff. The sole owner
/// of the retry loop, the sleep, and the log line for every failure kind
/// `fetch_events_page_once` can report — previously each kind had its own
/// near-identical "attempt += 1; log; sleep; continue" block inline, easy
/// to have drift out of sync; now there's exactly one. `keys` follows the
/// JSON-RPC spec: a list of OR-groups, one per key position, AND'd
/// together — an empty inner list means "any value at this position."
async fn get_events_page(
    client: &reqwest::Client,
    cfg: &StarknetConfig,
    address: Felt,
    keys: &[Vec<Felt>],
    from_block: u64,
    to_block: u64,
    continuation_token: Option<&str>,
) -> Result<EventsPage> {
    let keys_json: Vec<Vec<String>> = keys
        .iter()
        .map(|group| group.iter().map(|f| format!("{f:#x}")).collect())
        .collect();

    let mut filter = json!({
        "from_block": {"block_number": from_block},
        "to_block": {"block_number": to_block},
        "address": format!("{address:#x}"),
        "chunk_size": CHUNK_SIZE,
    });
    if !keys_json.is_empty() {
        filter["keys"] = json!(keys_json);
    }
    if let Some(tok) = continuation_token {
        filter["continuation_token"] = json!(tok);
    }

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "starknet_getEvents",
        "params": [filter],
    });

    let mut attempt = 0;
    loop {
        // Enforced before every attempt, not just after a failure — see
        // MIN_REQUEST_INTERVAL's doc comment for why backoff alone wasn't
        // enough.
        wait_for_request_slot().await;

        match fetch_events_page_once(client, cfg, &body).await? {
            PageAttempt::Success(page) => return Ok(page),
            PageAttempt::Retryable {
                reason,
                retry_after,
            } => {
                attempt += 1;
                if attempt > MAX_RATE_LIMIT_RETRIES {
                    bail!("starknet_getEvents {reason} after {attempt} attempts");
                }
                // Prefer the server's own stated wait time over a guessed
                // exponential backoff when it gave us one.
                let backoff =
                    retry_after.unwrap_or_else(|| Duration::from_millis(250 * 2u64.pow(attempt)));
                warn!("{reason}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Drains every page for a filter across `[from_block, to_block]`.
async fn fetch_all_events(
    cfg: &StarknetConfig,
    address: Felt,
    keys: &[Vec<Felt>],
    from_block: u64,
    to_block: u64,
) -> Result<Vec<DecodedEvent>> {
    let client = shared_http_client();
    let mut out = Vec::new();
    let mut token: Option<String> = None;

    loop {
        let page = get_events_page(
            client,
            cfg,
            address,
            keys,
            from_block,
            to_block,
            token.as_deref(),
        )
        .await?;

        for evt in page.events {
            let Some(selector_str) = evt.keys.first() else {
                continue;
            };
            let selector = parse_felt(selector_str)?;
            let keys: Vec<Felt> = evt
                .keys
                .iter()
                .map(|k| parse_felt(k))
                .collect::<Result<_>>()?;
            let data: Vec<Felt> = evt
                .data
                .iter()
                .map(|d| parse_felt(d))
                .collect::<Result<_>>()?;
            let _ = &evt.from_address; // already implied by the address filter
            out.push(DecodedEvent {
                selector,
                keys,
                data,
                transaction_hash: evt.transaction_hash,
                block_number: evt.block_number,
            });
        }

        match page.continuation_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }

    Ok(out)
}

/// Renders a duration in seconds as a short human string (`"3m12s"`,
/// `"1h04m"`), or `"unknown"` once the observed rate is zero/non-finite —
/// same helper shape as `evm_indexer::format_eta`, duplicated rather than
/// shared across modules since this one deliberately stays chain-agnostic
/// (see `log_cache.rs`'s module doc for the same reasoning applied there).
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

/// Logs cursor position, % of range covered, observed throughput and a
/// rough ETA for a chunked backfill loop — throttled by the caller so a
/// fast scan (the common case here, given `MAX_BLOCK_RANGE` chunks are
/// small) isn't spammed while a slow one (a long-idle process catching up
/// a large gap) stays visible instead of looking hung.
fn log_backfill_progress(
    scan_id: &str,
    from_block: u64,
    to_block: u64,
    current_to: u64,
    scan_start: Instant,
) {
    let total_range = to_block.saturating_sub(from_block) + 1;
    let blocks_done = current_to.saturating_sub(from_block) + 1;
    let elapsed_secs = scan_start.elapsed().as_secs_f64();
    let blocks_per_sec = if elapsed_secs > 0.0 {
        blocks_done as f64 / elapsed_secs
    } else {
        0.0
    };
    let pct = blocks_done as f64 / total_range as f64 * 100.0;
    let remaining = to_block.saturating_sub(current_to);
    let eta = if blocks_per_sec > 0.0 {
        format_eta(remaining as f64 / blocks_per_sec)
    } else {
        "unknown".to_string()
    };
    info!(
        "{scan_id}: block {current_to} of {to_block} ({pct:.1}%), \
         ~{blocks_per_sec:.0} blocks/sec, {remaining} block(s) remaining, ETA {eta}"
    );
}

pub struct StarknetCatchupSummary {
    pub merchants: Vec<StarknetReceiverRecord>,
    pub deposits: Vec<Deposit>,
    pub caught_up_to_block: Option<u64>,
}

/// Bounded, checkpointed registry discovery. Processes range in chunks defined by MAX_BLOCK_RANGE
/// to avoid Starknet RPC invalid parameter errors on large range gaps.
pub async fn discover_merchants(
    cfg: &StarknetConfig,
    cache: &LogCache,
    to_block: u64,
) -> Result<Vec<StarknetReceiverRecord>> {
    info!("Catching up on Beanie Starknet Registry started, this may take a while");
    let mut current_from = cache
        .get_checkpoint(STARKNET_REGISTRY_SCAN_ID)?
        .map(|b| b + 1)
        .unwrap_or(cfg.registry_start_block);

    if current_from > to_block {
        info!(
            "Beanie Starknet Registry already caught up through block {to_block}; nothing new to scan"
        );
        return Ok(Vec::new());
    }

    let merchant_registered = merchant_registered_selector()?;
    let receiver_announced = receiver_announced_selector()?;
    let mut all_merchants = Vec::new();
    let mut total_matching_events = 0usize;

    let scan_start = Instant::now();
    let backfill_from = current_from;
    let mut last_progress_log = Instant::now();
    let progress_log_interval = Duration::from_secs(10);

    while current_from <= to_block {
        let current_to = (current_from + MAX_BLOCK_RANGE - 1).min(to_block);

        let events = fetch_all_events(
            cfg,
            cfg.factory_address,
            &[vec![merchant_registered, receiver_announced]],
            current_from,
            current_to,
        )
        .await
        .context("failed fetching Starknet registry events")?;

        // Neither `ReceiverAnnounced { merchant, receiver, cctp_mint_chain,
        // cctp_mint_recipient }` nor `MerchantRegistered { merchant, receiver }`
        // in merchant_factory.cairo annotates any field `#[key]`, so every field
        // lands in the event's `data` array — `keys` is always just `[selector]`.
        // Read `data`, not `keys`. The two variants have different arities (5
        // felts vs. 2, the u256 recipient being two),
        // so they're parsed on separate branches rather than one shared
        // `(m, r)` extraction. A shape mismatch is warned about instead of
        // silently dropped — that silence is exactly what let this bug run
        // for an entire backfill history without ever surfacing.
        for evt in &events {
            if evt.selector == merchant_registered {
                total_matching_events += 1;
                // data = [merchant, receiver]
                match (evt.data.first(), evt.data.get(1)) {
                    (Some(&m), Some(&r)) => all_merchants.push(StarknetReceiverRecord {
                        merchant: m,
                        receiver: r,
                        route: None,
                    }),
                    _ => warn!(
                        "MerchantRegistered event at block {} (tx {}) had unexpected data shape (len={}), skipping",
                        evt.block_number,
                        evt.transaction_hash,
                        evt.data.len()
                    ),
                }
            } else if evt.selector == receiver_announced {
                total_matching_events += 1;
                // data = [merchant, receiver, cctp_mint_chain,
                //         recipient_low, recipient_high]
                match (
                    evt.data.first(),
                    evt.data.get(1),
                    evt.data.get(2),
                    evt.data.get(3),
                    evt.data.get(4),
                ) {
                    (Some(&m), Some(&r), Some(&chain), Some(&low), Some(&high)) => all_merchants
                        .push(StarknetReceiverRecord {
                            merchant: m,
                            receiver: r,
                            route: Some(StarknetRoute {
                                chain,
                                recipient_low: low,
                                recipient_high: high,
                            }),
                        }),
                    _ => warn!(
                        "ReceiverAnnounced event at block {} (tx {}) had unexpected data shape (len={}), skipping",
                        evt.block_number,
                        evt.transaction_hash,
                        evt.data.len()
                    ),
                }
            }
        }

        cache.set_checkpoint(STARKNET_REGISTRY_SCAN_ID, current_to)?;
        cache
            .flush()
            .context("failed flushing log cache after starknet registry discovery")?;

        let reached_end = current_to >= to_block;
        if reached_end || last_progress_log.elapsed() >= progress_log_interval {
            log_backfill_progress(
                STARKNET_REGISTRY_SCAN_ID,
                backfill_from,
                to_block,
                current_to,
                scan_start,
            );
            last_progress_log = Instant::now();
        }

        current_from = current_to + 1;
    }

    info!(
        "Catching up on Beanie Starknet Registry complete ({} merchant(s) found from {} matching event(s), through block {to_block})",
        all_merchants.len(),
        total_matching_events
    );

    Ok(all_merchants)
}

/// Same role as evm_subsquid::run_evm_catchup: one-shot startup catch-up
/// to "now," registry first (deposits need the receiver set it produces).
pub async fn run_starknet_catchup(
    cfg: &StarknetConfig,
    cache: &LogCache,
) -> Result<StarknetCatchupSummary> {
    let head = current_head(cfg)
        .await
        .context("failed fetching current Starknet head")?;
    let merchants = discover_merchants(cfg, cache, head).await?;
    let receivers: Vec<Felt> = merchants.iter().map(|rec| rec.receiver).collect();
    let deposits = fetch_deposits_since_block(cfg, cache, &receivers, head).await?;
    Ok(StarknetCatchupSummary {
        merchants,
        deposits,
        caught_up_to_block: Some(head),
    })
}

/// Plain JSON-RPC block number against cfg.rpc_url — unchanged from before,
/// this doesn't depend on the events source.
async fn current_head(cfg: &StarknetConfig) -> Result<u64> {
    use starknet::providers::{JsonRpcClient, Provider, jsonrpc::HttpTransport};
    let provider = JsonRpcClient::new(HttpTransport::new(url::Url::parse(&cfg.rpc_url)?));
    provider
        .block_number()
        .await
        .context("failed fetching Starknet block number")
}

pub async fn fetch_deposits_since_block(
    cfg: &StarknetConfig,
    cache: &LogCache,
    receivers: &[Felt],
    to_block: u64,
) -> Result<Vec<Deposit>> {
    info!("Catching up on Beanie Starknet deposits started, this may take a while");
    if receivers.is_empty() {
        // Falls through here whenever discover_merchants found nothing to
        // scan deposits for (including "found zero, ever" — see its own
        // completion log for the count). Previously this returned
        // silently: "started" would print with no matching "complete",
        // making a normal empty-receiver-set result look like a hang.
        info!("Beanie Starknet deposit scan skipped: no merchants/receivers discovered yet");
        return Ok(Vec::new());
    }

    let mut current_from = cache
        .get_checkpoint(STARKNET_DEPOSITS_SCAN_ID)?
        .map(|b| b + 1)
        .unwrap_or(cfg.deposit_start_block);

    if current_from > to_block {
        info!(
            "Beanie Starknet deposits already caught up through block {to_block}; nothing new to scan"
        );
        return Ok(Vec::new());
    }

    let transfer = transfer_selector()?;
    let mut all_deposits = Vec::new();

    let scan_start = Instant::now();
    let backfill_from = current_from;
    let mut last_progress_log = Instant::now();
    let progress_log_interval = Duration::from_secs(10);

    while current_from <= to_block {
        let current_to = (current_from + MAX_BLOCK_RANGE - 1).min(to_block);

        let events = fetch_all_events(
            cfg,
            cfg.token_address,
            &[vec![transfer], vec![], receivers.to_vec()],
            current_from,
            current_to,
        )
        .await
        .context("failed fetching Starknet deposit events")?;

        for evt in &events {
            if evt.selector != transfer {
                continue;
            }
            let (Some(&from), Some(&to)) = (evt.keys.get(1), evt.keys.get(2)) else {
                continue;
            };
            if !receivers.contains(&to) {
                continue;
            }
            let amount_raw = if evt.data.len() >= 2 {
                u256_to_string(evt.data[0], evt.data[1])
            } else {
                "0".to_string()
            };
            all_deposits.push(Deposit {
                tx_hash: evt.transaction_hash.clone(),
                from_address: format!("{from:#x}"),
                receiver: format!("{to:#x}"),
                amount_raw,
                block_number: evt.block_number,
            });
        }

        cache.set_checkpoint(STARKNET_DEPOSITS_SCAN_ID, current_to)?;
        cache
            .flush()
            .context("failed flushing log cache after starknet deposit discovery")?;

        let reached_end = current_to >= to_block;
        if reached_end || last_progress_log.elapsed() >= progress_log_interval {
            log_backfill_progress(
                STARKNET_DEPOSITS_SCAN_ID,
                backfill_from,
                to_block,
                current_to,
                scan_start,
            );
            last_progress_log = Instant::now();
        }

        current_from = current_to + 1;
    }

    all_deposits.sort_by_key(|d| d.block_number);
    info!(
        "Catching up on Beanie Starknet Deposits complete ({} deposit(s) found, through block {to_block})",
        all_deposits.len()
    );
    Ok(all_deposits)
}

fn u256_to_string(low: Felt, high: Felt) -> String {
    let low_u128: u128 = low.try_into().unwrap_or(0);
    let high_u128: u128 = high.try_into().unwrap_or(0);

    // Bug fix: this was `low_u128.to_string();` — a statement, not a
    // `return` — so this branch never actually returned and every amount
    // fell through to the hex-concatenation path below regardless of
    // `high_u128`. For a genuine zero-amount transfer (low = high = 0),
    // that fallthrough formatted 64 hex zero characters and then
    // `trim_start_matches('0')` stripped all of them, producing an EMPTY
    // STRING for `amount_raw` instead of "0" — a real landmine for
    // whatever consumes `Deposit.amount_raw` downstream (webhook payloads,
    // DB inserts, etc).
    if high_u128 == 0 {
        return low_u128.to_string();
    }

    // Note this remaining path returns a bare lowercase hex string (no
    // "0x" prefix) while the branch above returns decimal — an
    // inconsistent radix depending on magnitude. For USDC-scale deposits
    // (6 decimals) `high_u128` should never realistically be nonzero, so
    // this is unlikely to bite in practice, but it's still a real
    // inconsistency if some future token or a corrupted event ever hits
    // it. Flagging rather than silently "fixing" the radix too, since
    // changing this path's output format is a behavior change downstream
    // consumers may already depend on — worth a deliberate decision, not
    // a drive-by change bundled with the dead-code fix.
    format!("{high_u128:032x}{low_u128:032x}")
        .trim_start_matches('0')
        .to_string()
}

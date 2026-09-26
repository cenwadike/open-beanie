//! src/solana_indexer.rs
//!
//! Solana registry + deposit discovery, backed directly by Subsquid Portal
//! (https://portal.sqd.dev/datasets/solana-mainnet) — same vendor, same
//! typed-struct + rate-limiter pattern as evm_indexer.rs. This module IS
//! the vendor integration, same as evm_indexer.rs is for EVM.
//!
//! WHY ANNOUNCED IS A TRACKED STATE, NOT JUST REGISTERED
//! -------------------------------------------------------
//! `MerchantAnnounced` discloses `receiver`/`receiver_token_account` to
//! depositors before `receiver_config` exists on-chain. `register_merchant`
//! is a separate, permissionless broadcast of a pre-signed tx that can
//! happen long after the first deposit lands (JIT registration). If
//! discovery only tracked `Registered` receivers, any deposit that arrives
//! in that window is invisible until registration catches up — so every
//! receiver is tracked from `Announced` onward; only *sweeping* is gated on
//! `Registered`.
//!
//! WHY AN ANNOUNCE MUST BE VALIDATED BEFORE IT'S TRUSTED
//! -------------------------------------------------------
//! `announce_merchant` does not decode or check `reg_tx` on-chain — it just
//! pins whatever bytes it's given. Nothing stops an announce claiming
//! `merchant = X` from actually carrying a `register_merchant` call for a
//! *different* merchant, or for PDAs that don't match `receiver` at all.
//! `validate_announce` below re-derives every PDA/ATA from `receiver` and
//! decodes the embedded `register_merchant` instruction to confirm its
//! arguments and accounts agree with what the event claims, before the
//! receiver is folded into `merchant_map`.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use log::{debug, info, warn};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use crate::config::{Deposit, SolanaConfig};
use crate::log_cache::LogCache;

pub const REGISTRY_SCAN_ID: &str = "solana:registry";
pub const DEPOSITS_SCAN_ID: &str = "solana:deposits";

const CONFIG_SEED: &[u8] = b"config";
const PENDING_SEED: &[u8] = b"pending";
const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const MAX_SLOTS_PER_REQUEST: u64 = 50_000;

/// Same shape as evm_indexer.rs's `impl FnMut(...) -> Pin<Box<dyn Future...>>`
/// pattern, just named so it's a single line at every use site instead of a
/// multi-line generic that's easy to get wrong.
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

// ── Our own decoded wire shapes (not Subsquid's — see PortalBlock below) ────

/// One Anchor `emit!` event already extracted from a transaction's Portal
/// logs — the `Program data: <base64>` line base64-decoded, the 8-byte
/// Anchor discriminator stripped, `name` set from whichever discriminator
/// matched. Slot is carried through for checkpointing.
#[derive(Clone, Debug)]
pub struct AnchorEvent {
    pub name: String,  // "MerchantAnnounced" | "MerchantRegistered"
    pub data: Vec<u8>, // borsh body, discriminator already stripped
    pub slot: u64,
}

/// One SPL Token `Transfer`/`TransferChecked` instruction matching our
/// destination filter.
#[derive(Clone, Debug)]
pub struct TokenTransferEvent {
    pub source: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey, // signer that authorized the transfer — our "from"
    pub amount: u64,
    pub slot: u64,
}

// ── Registry state ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverStatus {
    Announced,
    Registered,
}

#[derive(Clone, Debug)]
pub struct SolanaReceiverRecord {
    pub merchant: Pubkey,
    pub receiver: Pubkey,
    pub receiver_token_account: Pubkey,
    pub receiver_config: Pubkey,
    pub status: ReceiverStatus,
    /// Present only while `Announced`. Cleared once `Registered` fires.
    pub reg_tx: Option<Vec<u8>>,
}

// ── Raw event bodies (fixed-size — plain byte-offset slicing) ───────────────

struct RawMerchantAnnounced {
    merchant: Pubkey,
    receiver: Pubkey,
    receiver_token_account: Pubkey,
    receiver_config: Pubkey,
    cctp_burn_staging_account: Pubkey,
    pending_registration: Pubkey,
    cctp_mint_chain: [u8; 32],
    cctp_mint_recipient: [u8; 32],
}

fn read_pubkey(buf: &[u8], off: usize) -> Result<Pubkey> {
    buf.get(off..off + 32)
        .map(|bytes| Pubkey::new_from_array(bytes.try_into().unwrap()))
        .context("event body too short")
}

fn decode_merchant_announced(data: &[u8]) -> Result<RawMerchantAnnounced> {
    if data.len() != 256 {
        bail!(
            "MerchantAnnounced body is {} bytes, expected 256",
            data.len()
        );
    }
    Ok(RawMerchantAnnounced {
        merchant: read_pubkey(data, 0)?,
        receiver: read_pubkey(data, 32)?,
        receiver_token_account: read_pubkey(data, 64)?,
        receiver_config: read_pubkey(data, 96)?,
        cctp_burn_staging_account: read_pubkey(data, 128)?,
        pending_registration: read_pubkey(data, 160)?,
        cctp_mint_chain: data[192..224].try_into().unwrap(),
        cctp_mint_recipient: data[224..256].try_into().unwrap(),
    })
}

pub struct RawMerchantRegistered {
    pub merchant: Pubkey,
    pub receiver: Pubkey,
    pub receiver_config: Pubkey,
    pub receiver_token_account: Pubkey,
}

pub fn decode_merchant_registered(data: &[u8]) -> Result<RawMerchantRegistered> {
    if data.len() < 128 {
        bail!(
            "MerchantRegistered body is {} bytes, expected >=128",
            data.len()
        );
    }
    Ok(RawMerchantRegistered {
        merchant: read_pubkey(data, 0)?,
        receiver: read_pubkey(data, 32)?,
        receiver_config: read_pubkey(data, 64)?,
        receiver_token_account: read_pubkey(data, 96)?,
    })
}

/// Namespace is "global" for instruction discriminators, "event" for
/// Anchor `emit!` discriminators.
fn anchor_discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let hash = solana_sdk::hash::hash(format!("{namespace}:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.to_bytes()[..8]);
    out
}

// ── Announce validation ─────────────────────────────────────────────────────

#[allow(unused)]
mod register_merchant_accounts {
    pub const PAYER: usize = 0;
    pub const RECEIVER: usize = 1;
    pub const FACTORY_CONFIG: usize = 2;
    pub const RECEIVER_TOKEN_ACCOUNT: usize = 3;
    pub const MERCHANT_TOKEN_ACCOUNT: usize = 4;
    pub const RECEIVER_CONFIG: usize = 5;
    pub const CCTP_BURN_STAGING_ACCOUNT: usize = 6;
    pub const MERCHANT_REGISTRY: usize = 7;
    pub const PENDING_REGISTRATION: usize = 8;
}

fn validate_announce(
    ev: &RawMerchantAnnounced,
    reg_tx: &[u8],
    program_id: &Pubkey,
    mint: &Pubkey,
) -> Result<()> {
    let (expect_config, _) =
        Pubkey::find_program_address(&[CONFIG_SEED, ev.receiver.as_ref()], program_id);
    let (expect_pending, _) =
        Pubkey::find_program_address(&[PENDING_SEED, ev.receiver.as_ref()], program_id);
    let expect_receiver_ta = get_associated_token_address(&ev.receiver, mint);
    let expect_staging_ta = get_associated_token_address(&expect_config, mint);

    if expect_config != ev.receiver_config {
        bail!("announce: receiver_config does not derive from receiver");
    }
    if expect_pending != ev.pending_registration {
        bail!("announce: pending_registration does not derive from receiver");
    }
    if expect_receiver_ta != ev.receiver_token_account {
        bail!("announce: receiver_token_account is not ATA(receiver, mint)");
    }
    if expect_staging_ta != ev.cctp_burn_staging_account {
        bail!("announce: cctp_burn_staging_account is not ATA(receiver_config, mint)");
    }

    let tx: Transaction =
        bincode::deserialize(reg_tx).context("reg_tx does not decode as a legacy Transaction")?;

    let register_disc = anchor_discriminator("global", "register_merchant");
    let ix = tx
        .message
        .instructions
        .iter()
        .find(|ix| {
            let program_key = tx.message.account_keys[ix.program_id_index as usize];
            program_key == *program_id && ix.data.len() >= 8 && ix.data[..8] == register_disc
        })
        .context("reg_tx contains no register_merchant instruction for this program")?;

    if ix.data.len() != 8 + 32 + 32 + 32 {
        bail!(
            "register_merchant instruction data is {} bytes, expected 104",
            ix.data.len()
        );
    }
    let arg_merchant = Pubkey::try_from(&ix.data[8..40]).unwrap();
    let arg_chain: [u8; 32] = ix.data[40..72].try_into().unwrap();
    let arg_recipient: [u8; 32] = ix.data[72..104].try_into().unwrap();

    if arg_merchant != ev.merchant {
        bail!("reg_tx registers a different merchant than the announce claims");
    }
    if arg_chain != ev.cctp_mint_chain || arg_recipient != ev.cctp_mint_recipient {
        bail!("reg_tx route does not match the announced route");
    }

    let acc = |idx: usize| -> Result<Pubkey> {
        let account_idx = *ix
            .accounts
            .get(idx)
            .context("register_merchant instruction missing an expected account")?;
        Ok(tx.message.account_keys[account_idx as usize])
    };
    if acc(register_merchant_accounts::RECEIVER)? != ev.receiver {
        bail!("reg_tx's receiver account does not match the announce");
    }
    if acc(register_merchant_accounts::RECEIVER_TOKEN_ACCOUNT)? != ev.receiver_token_account {
        bail!("reg_tx's receiver_token_account does not match the announce");
    }
    if acc(register_merchant_accounts::RECEIVER_CONFIG)? != ev.receiver_config {
        bail!("reg_tx's receiver_config does not match the announce");
    }
    if acc(register_merchant_accounts::CCTP_BURN_STAGING_ACCOUNT)? != ev.cctp_burn_staging_account {
        bail!("reg_tx's cctp_burn_staging_account does not match the announce");
    }
    if acc(register_merchant_accounts::PENDING_REGISTRATION)? != ev.pending_registration {
        bail!("reg_tx's pending_registration does not match the announce");
    }

    Ok(())
}

// ── Registry discovery ──────────────────────────────────────────────────────

fn merge_events(map: &mut HashMap<Pubkey, SolanaReceiverRecord>, events: &[AnchorEvent]) {
    for ev in events {
        if ev.name != "MerchantRegistered" {
            continue; // MerchantAnnounced handled in attach_reg_tx_and_merge
        }
        let Ok(raw) = decode_merchant_registered(&ev.data) else {
            log::warn!("undecodable MerchantRegistered at slot {}", ev.slot);
            continue;
        };
        let entry = map.entry(raw.receiver).or_insert(SolanaReceiverRecord {
            merchant: raw.merchant,
            receiver: raw.receiver,
            receiver_token_account: raw.receiver_token_account,
            receiver_config: raw.receiver_config,
            status: ReceiverStatus::Registered,
            reg_tx: None,
        });
        entry.status = ReceiverStatus::Registered;
        entry.reg_tx = None;
    }
}

/// `MerchantAnnounced` needs the pinned `PendingRegistration.reg_tx` blob to
/// validate against — the event itself doesn't carry it. `fetch_reg_tx` is
/// an ordinary on-chain account read (via solana_keeper's RPC client, not
/// Portal — Portal doesn't serve current account state).
pub async fn attach_reg_tx_and_merge(
    map: &mut HashMap<Pubkey, SolanaReceiverRecord>,
    events: &[AnchorEvent],
    program_id: &Pubkey,
    mint: &Pubkey,
    mut fetch_reg_tx: impl FnMut(Pubkey) -> BoxFuture<Result<Vec<u8>>>,
) {
    for ev in events {
        if ev.name != "MerchantAnnounced" {
            continue;
        }
        let Ok(raw) = decode_merchant_announced(&ev.data) else {
            log::warn!("undecodable MerchantAnnounced at slot {}", ev.slot);
            continue;
        };
        if matches!(map.get(&raw.receiver), Some(r) if r.status == ReceiverStatus::Registered) {
            continue;
        }
        let reg_tx = match fetch_reg_tx(raw.pending_registration).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!(
                    "could not fetch pending_registration for announced receiver {}: {e:#} — skipping until next scan",
                    raw.receiver
                );
                continue;
            }
        };
        if let Err(e) = validate_announce(&raw, &reg_tx, program_id, mint) {
            log::warn!(
                "rejecting invalid/squatted announce for receiver {}: {e:#}",
                raw.receiver
            );
            continue;
        }
        map.insert(
            raw.receiver,
            SolanaReceiverRecord {
                merchant: raw.merchant,
                receiver: raw.receiver,
                receiver_token_account: raw.receiver_token_account,
                receiver_config: raw.receiver_config,
                status: ReceiverStatus::Announced,
                reg_tx: Some(reg_tx),
            },
        );
    }
}

pub struct SolanaCatchupSummary {
    pub receivers: Vec<SolanaReceiverRecord>,
    pub deposits: Vec<Deposit>,
    pub caught_up_to_slot: Option<u64>,
}

pub async fn run_solana_catchup(
    cfg: &SolanaConfig,
    cache: &LogCache,
    fetch_reg_tx: impl FnMut(Pubkey) -> BoxFuture<Result<Vec<u8>>>,
) -> Result<SolanaCatchupSummary> {
    let head = current_slot(cfg)
        .await
        .context("failed fetching Solana head slot from Subsquid Portal")?;
    let from_slot = cache
        .get_checkpoint(REGISTRY_SCAN_ID)?
        .map(|s| s + 1)
        .unwrap_or(cfg.registry_start_slot);

    debug!("Catching up on Beanie Solana Registry started, this may take a while");

    let (events, last_seen) = fetch_program_events(cfg, cache, from_slot, head).await?;

    let mut map: HashMap<Pubkey, SolanaReceiverRecord> = HashMap::new();
    merge_events(&mut map, &events);
    attach_reg_tx_and_merge(&mut map, &events, &cfg.program_id, &cfg.mint, fetch_reg_tx).await;

    if let Some(seen) = last_seen {
        cache.set_checkpoint(REGISTRY_SCAN_ID, seen)?;
    }

    let receivers: Vec<SolanaReceiverRecord> = map.into_values().collect();
    let receiver_tas: Vec<Pubkey> = receivers.iter().map(|r| r.receiver_token_account).collect();

    info!(
        "Catching up on Beanie Solana Registry: COMPLETED ({} receiver(s) found, through slot {head})",
        receivers.len()
    );

    let deposits = fetch_deposits_since_slot(cfg, cache, &receiver_tas, head).await?;

    cache
        .flush()
        .context("failed flushing log cache after solana catch-up")?;

    Ok(SolanaCatchupSummary {
        receivers,
        deposits,
        caught_up_to_slot: Some(head),
    })
}

/// `receiver_token_accounts` must be the Announced ∪ Registered set — a
/// deposit into an announced-but-unregistered receiver still needs to be
/// seen (see module doc).
pub async fn fetch_deposits_since_slot(
    cfg: &SolanaConfig,
    cache: &LogCache,
    receiver_token_accounts: &[Pubkey],
    to_slot: u64,
) -> Result<Vec<Deposit>> {
    if receiver_token_accounts.is_empty() {
        debug!("Beanie Solana deposit scan skipped: no receivers known");
        return Ok(Vec::new());
    }
    let from_slot = cache
        .get_checkpoint(DEPOSITS_SCAN_ID)?
        .map(|s| s + 1)
        .unwrap_or(cfg.deposit_start_slot);
    if from_slot > to_slot {
        return Ok(Vec::new());
    }

    debug!(
        "Catching up on Beanie Solana Deposits started ({} receiver(s)), this may take a while",
        receiver_token_accounts.len()
    );

    let (transfers, last_seen) =
        fetch_token_transfers(cfg, cache, receiver_token_accounts, from_slot, to_slot).await?;

    let deposits: Vec<Deposit> = transfers
        .into_iter()
        .map(|t| Deposit {
            tx_hash: String::new(), // add "transaction": {"signatures": true} to fields if needed
            from_address: t.authority.to_string(),
            receiver: t.destination.to_string(),
            amount_raw: t.amount.to_string(),
            block_number: t.slot,
        })
        .collect();

    if let Some(seen) = last_seen {
        cache.set_checkpoint(DEPOSITS_SCAN_ID, seen)?;
    }

    info!(
        "Catching up on Beanie Solana Deposits: COMPLETED ({} deposit(s) found, through slot {to_slot})",
        deposits.len()
    );
    Ok(deposits)
}

// ── Subsquid Portal — the actual vendor integration ─────────────────────────
//
// Same typed-struct + rate-limiter pattern as evm_indexer.rs. "type":
// "solana" and slot-shaped fields instead of "type": "evm".

pub static SQD_SOLANA_RATE_LIMITER: LazyLock<DefaultDirectRateLimiter> = LazyLock::new(|| {
    let quota = Quota::with_period(Duration::from_millis(500))
        .unwrap()
        .allow_burst(NonZeroU32::new(20).unwrap());
    RateLimiter::direct(quota)
});

/// One shared, timeout-bounded, `x-api-key`-carrying HTTP client for
/// every Subsquid Portal request the Solana side makes — catch-up
/// (`current_slot`, `stream_solana` below) and live tips
/// (`solana_ws::run_solana_subscription`) alike, so `solana_ws.rs` no
/// longer has to build its own bare, unauthenticated client per call.
///
/// Only one `SolanaConfig` exists per process today, so a single
/// `OnceLock` (built from whichever `cfg` first calls this) is enough —
/// unlike the EVM side's `evm_indexer::portal`, which is keyed per
/// config because Base and Arbitrum are two distinct deployments.
pub(crate) fn portal_http_client(cfg: &SolanaConfig) -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(60));
        if let Some(key) = &cfg.subsquid_portal_api_key {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::from_str(key)
                    .expect("SOLANA_SUBSQUID_PORTAL_API_KEY is not a valid HTTP header value"),
            );
            builder = builder.default_headers(headers);
        }
        builder
            .build()
            .expect("failed building shared Subsquid Portal HTTP client (Solana)")
    })
}

#[derive(Deserialize)]
struct PortalBlock {
    header: PortalHeader,
    #[serde(default)]
    logs: Vec<PortalLog>,
    #[serde(default)]
    instructions: Vec<PortalInstruction>,
}

#[derive(Deserialize)]
struct PortalHeader {
    number: u64,
}

#[derive(Deserialize)]
struct PortalLog {
    #[allow(unused)]
    #[serde(rename = "programId")]
    program_id: String,
    #[allow(unused)]
    kind: String,
    message: String,
}

#[derive(Deserialize)]
struct PortalInstruction {
    accounts: Vec<String>,
    data: String,
}

/// Same "/head then fall back to a 1-block stream + response header"
/// pattern as evm_indexer.rs's `current_head`.
pub async fn current_slot(cfg: &SolanaConfig) -> Result<u64> {
    let base_url = cfg.subsquid_portal_url.trim_end_matches('/');
    let client = portal_http_client(cfg);

    let head_url = format!("{base_url}/head");
    if let Ok(resp) = client.get(&head_url).send().await {
        if resp.status().is_success() {
            if let Ok(text) = resp.text().await {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(num) = val
                        .as_u64()
                        .or_else(|| val.get("height").and_then(|h| h.as_u64()))
                        .or_else(|| val.get("number").and_then(|h| h.as_u64()))
                    {
                        return Ok(num);
                    }
                }
            }
        }
    }

    let stream_url = format!("{base_url}/stream");
    let body = serde_json::json!({
        "type": "solana",
        "fromBlock": 0,
        "toBlock": 0,
        "fields": { "block": { "number": true } }
    });
    let resp = client
        .post(&stream_url)
        .json(&body)
        .send()
        .await
        .context("Subsquid Portal Solana stream request failed for head query")?;

    if let Some(head_hdr) = resp.headers().get("x-sqd-head-number") {
        if let Ok(head_str) = head_hdr.to_str() {
            if let Ok(head) = head_str.parse::<u64>() {
                return Ok(head);
            }
        }
    }

    Err(anyhow!(
        "Could not fetch current slot from Subsquid Portal (Solana)"
    ))
}

/// Streams `from_slot..=to_slot` from Portal, calling `on_block` for every
/// block line as it arrives. Returns the last slot actually observed.
async fn stream_solana(
    cfg: &SolanaConfig,
    from_slot: u64,
    to_slot: u64,
    extra_fields: serde_json::Value,
    mut on_block: impl FnMut(&PortalBlock),
) -> Result<Option<u64>> {
    if from_slot > to_slot {
        return Ok(None);
    }

    let client = portal_http_client(cfg);
    let stream_url = format!("{}/stream", cfg.subsquid_portal_url.trim_end_matches('/'));
    let mut cursor = from_slot;
    let mut last_seen: Option<u64> = None;

    while cursor <= to_slot {
        let ceiling = std::cmp::min(cursor + MAX_SLOTS_PER_REQUEST - 1, to_slot);

        let mut body = serde_json::json!({
            "type": "solana",
            "fromBlock": cursor,
            "toBlock": ceiling,
        });
        if let (serde_json::Value::Object(extra), serde_json::Value::Object(b)) =
            (extra_fields.clone(), &mut body)
        {
            b.extend(extra);
        }

        SQD_SOLANA_RATE_LIMITER.until_ready().await;
        let resp = client
            .post(&stream_url)
            .json(&body)
            .send()
            .await
            .context("Subsquid Portal Solana stream request failed")?;

        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            break;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                warn!("Subsquid Portal Solana rate-limited, retrying in 1s...");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            bail!("Subsquid Portal Solana stream HTTP {status}: {text}");
        }

        let text = resp
            .text()
            .await
            .context("failed reading Portal response body")?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            break;
        }

        let mut batch_last = None;
        for line in &lines {
            let block: PortalBlock = serde_json::from_str(line)
                .with_context(|| format!("failed decoding Portal Solana NDJSON line: {line}"))?;
            batch_last = Some(block.header.number);
            on_block(&block);
        }

        let Some(batch_last) = batch_last else { break };
        last_seen = Some(batch_last);
        cursor = batch_last + 1;

        if batch_last >= to_slot {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(last_seen)
}

/// MerchantAnnounced/MerchantRegistered are Anchor `emit!` lines — `kind:
/// "data"` log rows ("Program data: <base64>"). Filtered by emitting
/// program_id; discriminator match happens client-side.
pub async fn fetch_program_events(
    cfg: &SolanaConfig,
    _cache: &LogCache,
    from_slot: u64,
    to_slot: u64,
) -> Result<(Vec<AnchorEvent>, Option<u64>)> {
    let announced_disc = anchor_discriminator("event", "MerchantAnnounced");
    let registered_disc = anchor_discriminator("event", "MerchantRegistered");
    let program_str = cfg.program_id.to_string();

    let extra = serde_json::json!({
        "fields": {
            "block": { "number": true },
            "log": { "programId": true, "kind": true, "message": true }
        },
        "logs": [{ "programId": [program_str], "kind": ["data"] }]
    });

    let mut events = Vec::new();
    let last_seen = stream_solana(cfg, from_slot, to_slot, extra, |block| {
        let slot = block.header.number;
        for log in &block.logs {
            let Some(b64) = log.message.strip_prefix("Program data: ") else {
                continue;
            };
            let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                warn!("undecodable base64 in Program data log: {}", log.message);
                continue;
            };
            if raw.len() < 8 {
                continue;
            }
            let (disc, data) = raw.split_at(8);
            let name = if disc == announced_disc {
                "MerchantAnnounced"
            } else if disc == registered_disc {
                "MerchantRegistered"
            } else {
                continue;
            };
            events.push(AnchorEvent {
                name: name.to_string(),
                data: data.to_vec(),
                slot,
            });
        }
    })
    .await
    .with_context(|| "solana registry discovery via Subsquid Portal failed")?;

    Ok((events, last_seen))
}

/// `a1` filters by the account at instruction-account position 1 — Portal
/// documents this for instruction filters. SPL Transfer/TransferChecked
/// account order is [source, destination, authority, ...], so `a1` =
/// destination.
async fn fetch_token_transfers(
    cfg: &SolanaConfig,
    _cache: &LogCache,
    destinations: &[Pubkey],
    from_slot: u64,
    to_slot: u64,
) -> Result<(Vec<TokenTransferEvent>, Option<u64>)> {
    if destinations.is_empty() {
        return Ok((Vec::new(), None));
    }
    let dest_strs: Vec<String> = destinations.iter().map(|p| p.to_string()).collect();

    let extra = serde_json::json!({
        "fields": {
            "block": { "number": true },
            "instruction": { "accounts": true, "data": true, "d1": true }
        },
        "instructions": [
            { "programId": [SPL_TOKEN_PROGRAM], "d1": ["0x03"], "a1": dest_strs.clone() }, // Transfer
            { "programId": [SPL_TOKEN_PROGRAM], "d1": ["0x0c"], "a1": dest_strs }          // TransferChecked
        ]
    });

    let mut transfers = Vec::new();
    let last_seen = stream_solana(cfg, from_slot, to_slot, extra, |block| {
        let slot = block.header.number;
        for ix in &block.instructions {
            let get_pubkey = |i: usize| -> Option<Pubkey> {
                ix.accounts.get(i).and_then(|s| Pubkey::from_str(s).ok())
            };
            let (Some(source), Some(destination), Some(authority)) =
                (get_pubkey(0), get_pubkey(1), get_pubkey(2))
            else {
                continue;
            };
            let clean = ix.data.trim_start_matches("0x");
            let Ok(raw) = (0..clean.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&clean[i..i + 2], 16))
                .collect::<std::result::Result<Vec<u8>, _>>()
            else {
                continue;
            };
            if raw.len() < 9 {
                continue;
            }
            let amount = u64::from_le_bytes(raw[1..9].try_into().unwrap());
            transfers.push(TokenTransferEvent {
                source,
                destination,
                authority,
                amount,
                slot,
            });
        }
    })
    .await
    .with_context(|| "solana deposit discovery via Subsquid Portal failed")?;

    Ok((transfers, last_seen))
}

//! src/solana_indexer.rs
//!
//! Solana registry + deposit discovery.
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
//! Anchor's seed constraints mean a forged `reg_tx` can never corrupt
//! on-chain state (`register_merchant` simply fails at broadcast time if the
//! accounts don't derive correctly), but an unvalidated announce can still
//! poison OUR state: a deposit could get attributed to the wrong merchant's
//! webhook, or we could waste the deposit scan watching a bogus ATA forever.
//! `validate_announce` below re-derives every PDA/ATA from `receiver` and
//! decodes the embedded `register_merchant` instruction to confirm its
//! arguments and accounts agree with what the event claims, before the
//! receiver is folded into `merchant_map`.
//!
//! TRANSPORT IS DELIBERATELY ABSTRACTED
//! -------------------------------------------------------
//! This module has no idea which gRPC vendor is behind it — same posture as
//! `evm_indexer.rs` not knowing Subsquid Portal's wire format leaks past its
//! `PortalBlock`/`PortalLog` structs. `SolanaEventSource` is the seam: it
//! takes a program-owned account/log stream and a Token-Program transfer
//! stream and returns them in the vendor-agnostic shapes below. The actual
//! vendor client (Bitquery CoreCast or equivalent archive-gRPC) implements
//! this trait in a separate module once picked; nothing here changes when
//! it is.

use anyhow::{Context, Result, bail};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashMap;

use crate::config::{Deposit, SolanaConfig};
use crate::log_cache::LogCache;

pub const REGISTRY_SCAN_ID: &str = "solana:registry";
pub const DEPOSITS_SCAN_ID: &str = "solana:deposits";

const CONFIG_SEED: &[u8] = b"config";
const PENDING_SEED: &[u8] = b"pending";

// ── Vendor-agnostic wire shapes ─────────────────────────────────────────────

/// One Anchor `emit!` event already extracted from a transaction's logs —
/// `sol_log_data` line base64-decoded, the 8-byte Anchor discriminator
/// stripped, `name` set from whichever discriminator matched. Slot and
/// signature are carried through for checkpointing / dedup.
#[derive(Clone, Debug)]
pub struct AnchorEvent {
    pub name: String,  // "MerchantAnnounced" | "MerchantRegistered"
    pub data: Vec<u8>, // borsh body, discriminator already stripped
    pub slot: u64,
    pub signature: String,
}

/// One SPL Token `Transfer`/`TransferChecked` instruction matching our
/// destination filter.
#[derive(Clone, Debug)]
pub struct TokenTransferEvent {
    pub source: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey, // signer that authorized the transfer — our "from"
    pub amount: u64,
    pub signature: String,
    pub slot: u64,
}

#[async_trait::async_trait]
pub trait SolanaEventSource: Send + Sync {
    /// All `MerchantAnnounced`/`MerchantRegistered` events logged by
    /// `program_id` in `(from_slot, to_slot]`. Vendor implements pagination.
    async fn fetch_program_events(
        &self,
        program_id: &Pubkey,
        from_slot: u64,
        to_slot: u64,
    ) -> Result<(Vec<AnchorEvent>, u64 /* last_slot_seen */)>;

    /// All token transfers into any of `destinations` in `(from_slot, to_slot]`.
    async fn fetch_token_transfers(
        &self,
        mint: &Pubkey,
        destinations: &[Pubkey],
        from_slot: u64,
        to_slot: u64,
    ) -> Result<(Vec<TokenTransferEvent>, u64)>;

    /// Current chain head, in slots.
    async fn current_slot(&self) -> Result<u64>;
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
    /// Present only while `Announced`. Cleared once `Registered` fires —
    /// nothing downstream needs it after that, and there's no reason to
    /// keep a stale broadcastable tx sitting in memory.
    pub reg_tx: Option<Vec<u8>>,
}

// ── Raw event bodies (fixed-size borsh — no Vec/String fields, so plain
// byte-offset slicing is simpler and less dependency-fragile than pulling
// in a full borsh derive here) ───────────────────────────────────────────────

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
    // 6 pubkeys (32B each) + 2 [u8;32] = 256 bytes, fixed.
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

struct RawMerchantRegistered {
    merchant: Pubkey,
    receiver: Pubkey,
    receiver_config: Pubkey,
    receiver_token_account: Pubkey,
}

fn decode_merchant_registered(data: &[u8]) -> Result<RawMerchantRegistered> {
    // 4 pubkeys + 2 [u8;32] = 192 bytes; we only need the first 4 fields.
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

fn anchor_discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let hash = solana_sdk::hash::hash(format!("{namespace}:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.to_bytes()[..8]);
    out
}

// ── Announce validation ─────────────────────────────────────────────────────

/// Account order Anchor compiles `register_merchant`'s instruction into,
/// matching `RegisterMerchant`'s field order in lib.rs exactly. If the
/// on-chain IDL ever reorders that struct this must move with it — there is
/// no way to derive this positionally at runtime, it has to track the
/// source of truth by hand.
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

/// Re-derives every PDA/ATA from `receiver` alone and confirms the announce
/// event's claimed addresses agree, then decodes the embedded
/// `register_merchant` instruction inside `reg_tx` and confirms ITS
/// arguments and accounts agree too. Only once both checks pass is a
/// receiver safe to fold into `merchant_map`.
fn validate_announce(
    ev: &RawMerchantAnnounced,
    reg_tx: &[u8],
    program_id: &Pubkey,
    mint: &Pubkey,
) -> Result<()> {
    // 1. Re-derive from `receiver`, compare to what the event claims.
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

    // 2. Decode the pinned reg_tx and confirm it actually registers THIS
    // merchant/receiver/route, not a different one.
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

/// Merges a batch of program events into `map`, validating every
/// `MerchantAnnounced` before it's trusted and never downgrading a
/// `Registered` receiver back to `Announced`.
fn merge_events(
    map: &mut HashMap<Pubkey, SolanaReceiverRecord>,
    events: &[AnchorEvent],
    program_id: &Pubkey,
    mint: &Pubkey,
) {
    for ev in events {
        match ev.name.as_str() {
            "MerchantAnnounced" => {
                let Ok(raw) = decode_merchant_announced(&ev.data) else {
                    log::warn!(
                        "undecodable MerchantAnnounced at slot {} (sig {})",
                        ev.slot,
                        ev.signature
                    );
                    continue;
                };
                // reg_tx itself isn't in the event — the caller (registry
                // scan) must separately fetch the pending PDA's contents
                // (or the vendor may surface it inline; see fetch loop
                // below) before validation can run. This function assumes
                // it's already been attached — see `attach_reg_tx_and_merge`.
                let _ = raw; // placeholder: real merge happens in
                // attach_reg_tx_and_merge below, which has the
                // blob in hand.
            }
            "MerchantRegistered" => {
                let Ok(raw) = decode_merchant_registered(&ev.data) else {
                    log::warn!(
                        "undecodable MerchantRegistered at slot {} (sig {})",
                        ev.slot,
                        ev.signature
                    );
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
                entry.reg_tx = None; // no longer needed once registered
            }
            other => {
                log::debug!("ignoring unrecognized program event: {other}");
            }
        }
    }
    let _ = (program_id, mint); // used by the caller's announce path
}

/// `MerchantAnnounced` needs the pinned `PendingRegistration.reg_tx` blob to
/// validate against — the event itself doesn't carry it. `fetch_reg_tx` is
/// the vendor-specific "read this account's data" call (an ordinary account
/// read, not an event — any RPC or gRPC accounts-snapshot works).
pub async fn attach_reg_tx_and_merge<F, Fut>(
    map: &mut HashMap<Pubkey, SolanaReceiverRecord>,
    events: &[AnchorEvent],
    program_id: &Pubkey,
    mint: &Pubkey,
    mut fetch_reg_tx: F,
) where
    F: FnMut(Pubkey /* pending_registration PDA */) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    for ev in events {
        if ev.name != "MerchantAnnounced" {
            continue;
        }
        let Ok(raw) = decode_merchant_announced(&ev.data) else {
            log::warn!(
                "undecodable MerchantAnnounced at slot {} (sig {})",
                ev.slot,
                ev.signature
            );
            continue;
        };
        // Registered already wins over Announced — never downgrade.
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
    source: &dyn SolanaEventSource,
    fetch_reg_tx: impl FnMut(
        Pubkey,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send>,
    >,
) -> Result<SolanaCatchupSummary> {
    let head = source
        .current_slot()
        .await
        .context("failed fetching Solana head slot")?;
    let from_slot = cache
        .get_checkpoint(REGISTRY_SCAN_ID)?
        .map(|s| s + 1)
        .unwrap_or(cfg.registry_start_slot);

    let (events, last_seen) = source
        .fetch_program_events(&cfg.program_id, from_slot, head)
        .await
        .context("solana registry discovery failed")?;

    let mut map: HashMap<Pubkey, SolanaReceiverRecord> = HashMap::new();
    merge_events(&mut map, &events, &cfg.program_id, &cfg.mint); // MerchantRegistered
    attach_reg_tx_and_merge(&mut map, &events, &cfg.program_id, &cfg.mint, fetch_reg_tx).await; // MerchantAnnounced

    cache.set_checkpoint(REGISTRY_SCAN_ID, last_seen)?;

    let receivers: Vec<SolanaReceiverRecord> = map.into_values().collect();
    let receiver_tas: Vec<Pubkey> = receivers.iter().map(|r| r.receiver_token_account).collect();

    let deposits = fetch_deposits_since_slot(cfg, cache, source, &receiver_tas, head).await?;

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
/// seen (see module doc). Checkpoint written once, after the full scan
/// returns — same reasoning as `evm:deposits:v2`.
pub async fn fetch_deposits_since_slot(
    cfg: &SolanaConfig,
    cache: &LogCache,
    source: &dyn SolanaEventSource,
    receiver_token_accounts: &[Pubkey],
    to_slot: u64,
) -> Result<Vec<Deposit>> {
    if receiver_token_accounts.is_empty() {
        return Ok(Vec::new());
    }
    let from_slot = cache
        .get_checkpoint(DEPOSITS_SCAN_ID)?
        .map(|s| s + 1)
        .unwrap_or(cfg.deposit_start_slot);
    if from_slot > to_slot {
        return Ok(Vec::new());
    }

    let (transfers, last_seen) = source
        .fetch_token_transfers(&cfg.mint, receiver_token_accounts, from_slot, to_slot)
        .await
        .context("solana deposit discovery failed")?;

    let deposits: Vec<Deposit> = transfers
        .into_iter()
        .map(|t| Deposit {
            tx_hash: t.signature,
            from_address: t.authority.to_string(),
            receiver: t.destination.to_string(),
            amount_raw: t.amount.to_string(),
            block_number: t.slot,
        })
        .collect();

    cache.set_checkpoint(DEPOSITS_SCAN_ID, last_seen)?;
    Ok(deposits)
}

//! src/solana_keeper.rs
//!
//! Helius RPC for state reads + tx submission. Event discovery never comes
//! through here — see solana_indexer.rs / solana_ws.rs — mirrors
//! `evm_rpc_url` being state-reads-and-sends-only in `EvmConfig`.

use anchor_lang::solana_program::system_instruction;
use anchor_lang::system_program;
use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::nonce_utils;
use solana_client::rpc_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction as LegacyTransaction;
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{SolanaConfig, now_formatted};
use crate::solana_indexer::{SolanaReceiverRecord, SolanaRoute};

/// Conservative same-chain batch size. Each `sweep` (same-chain) touches 9
/// accounts (caller, caller_ta, receiver_ta, factory_config, treasury_ta,
/// receiver_config, merchant_ta, token_program, system_program) minus the
/// ones shared across every instruction in the tx (factory_config,
/// treasury_ta, token_program, system_program, caller, caller_ta = 6
/// shared), so each additional sweep only adds ~3 unique accounts. Six
/// sweeps/tx stays comfortably under both the 1232-byte packet limit and
/// the account-count ceiling without needing an Address Lookup Table.
/// Raise this once an ALT is wired up for the shared accounts.
pub const MAX_SAME_CHAIN_SWEEPS_PER_TX: usize = 6;

/// Fixed on-chain size of a durable nonce account (`NONCE_ACCOUNT_LENGTH` in
/// `@solana/web3.js`, `solana_nonce::state::State::size()` in newer split
/// SDK crates). This is a protocol constant, not something that varies by
/// SDK version, so hardcoding it avoids chasing which crate re-exports the
/// `State` type in your particular `solana-sdk` version.
const NONCE_ACCOUNT_LENGTH: usize = 80;

pub fn build_client(cfg: &SolanaConfig) -> Arc<RpcClient> {
    Arc::new(RpcClient::new_with_commitment(
        cfg.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ))
}

fn anchor_discriminator(name: &str) -> [u8; 8] {
    let hash = solana_sdk::hash::hash(format!("global:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.to_bytes()[..8]);
    out
}

// ── PDA seeds ───────────────────────────────────────────────────────────────
// Mirrors FACTORY_SEED/CONFIG_SEED/PENDING_SEED/REGISTRY_SEED and the exact
// seed ordering used by `deriveConfig`/`derivePending`/`deriveRegistry` in
// tests/solana_beanie.ts.

const FACTORY_SEED: &[u8] = b"factory";
const CONFIG_SEED: &[u8] = b"config";
const PENDING_SEED: &[u8] = b"pending";
const REGISTRY_SEED: &[u8] = b"registry";

fn derive_factory_config(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[FACTORY_SEED], program_id).0
}

pub fn derive_receiver_config(
    program_id: &Pubkey,
    merchant: &Pubkey,
    receiver: &Pubkey,
    chain: &[u8; 32],
    recipient: &[u8; 32],
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            CONFIG_SEED,
            merchant.as_ref(),
            receiver.as_ref(),
            chain,
            recipient,
        ],
        program_id,
    )
    .0
}

pub fn derive_pending_registration(
    program_id: &Pubkey,
    merchant: &Pubkey,
    receiver: &Pubkey,
    chain: &[u8; 32],
    recipient: &[u8; 32],
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            PENDING_SEED,
            merchant.as_ref(),
            receiver.as_ref(),
            chain,
            recipient,
        ],
        program_id,
    )
    .0
}

fn derive_merchant_registry(program_id: &Pubkey, merchant: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[REGISTRY_SEED, merchant.as_ref()], program_id).0
}

// ── CCTP (Circle Cross-Chain Transfer Protocol) PDAs ────────────────────────
// These belong to Circle's TokenMessengerMinterV2 / MessageTransmitterV2
// programs, not ours. Verified against Circle's real, deployed V2 source
// (circlefin/solana-cctp-contracts, programs/v2/token-messenger-minter-v2/
// src/token_messenger_v2/instructions/deposit_for_burn.rs) — every account
// this program's `sweep` cross-chain branch passes maps 1:1, in the same
// order, to that instruction's `DepositForBurnContext`. The one seed not
// independently verifiable that way is `remote_token_messenger` (Circle's
// own struct has no `seeds =` constraint on it — the caller/SDK is trusted
// to supply the right address); that one still rests on Circle's official
// V2 quickstart sample rather than an on-chain-checkable constraint.
const CCTP_SENDER_AUTHORITY_SEED: &[u8] = b"sender_authority";
const CCTP_DENYLIST_SEED: &[u8] = b"denylist_account";
const CCTP_MESSAGE_TRANSMITTER_SEED: &[u8] = b"message_transmitter";
const CCTP_TOKEN_MESSENGER_SEED: &[u8] = b"token_messenger";
const CCTP_REMOTE_TOKEN_MESSENGER_SEED: &[u8] = b"remote_token_messenger";
const CCTP_TOKEN_MINTER_SEED: &[u8] = b"token_minter";
const CCTP_LOCAL_TOKEN_SEED: &[u8] = b"local_token";
const CCTP_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";

// Same constants as lib.rs's CCTP_TOKEN_MESSENGER_MINTER_V2 /
// CCTP_MESSAGE_TRANSMITTER_V2 — duplicated here rather than imported since
// this crate doesn't depend on the on-chain program crate.
const CCTP_TOKEN_MESSENGER_MINTER_V2: &str = "CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe";
const CCTP_MESSAGE_TRANSMITTER_V2: &str = "CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC";

fn cctp_token_messenger_minter_id() -> Pubkey {
    Pubkey::from_str(CCTP_TOKEN_MESSENGER_MINTER_V2).expect("valid pubkey literal")
}
fn cctp_message_transmitter_id() -> Pubkey {
    Pubkey::from_str(CCTP_MESSAGE_TRANSMITTER_V2).expect("valid pubkey literal")
}

fn derive_cctp_sender_authority_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_SENDER_AUTHORITY_SEED],
        &cctp_token_messenger_minter_id(),
    )
    .0
}

/// Seeded off whichever pubkey signs as CCTP's `owner` for the burn — in
/// this program that's `receiver_config` (see lib.rs's `sweep`:
/// `AccountMeta::new_readonly(receiver_config.key(), true)` is passed as
/// `owner`), NOT the keeper wallet. Confirmed against Circle's real V2
/// source: `seeds = [b"denylist_account", owner.key()]`.
fn derive_cctp_denylist_account(owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_DENYLIST_SEED, owner.as_ref()],
        &cctp_token_messenger_minter_id(),
    )
    .0
}
fn derive_cctp_message_transmitter() -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_MESSAGE_TRANSMITTER_SEED],
        &cctp_message_transmitter_id(),
    )
    .0
}
fn derive_cctp_token_messenger() -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_TOKEN_MESSENGER_SEED],
        &cctp_token_messenger_minter_id(),
    )
    .0
}
/// Domain is encoded as its base-10 STRING representation for this seed in
/// CCTP V2 (V1 used raw domain bytes instead) — per Circle's V2 quickstart.
/// Not independently checkable against an on-chain `seeds =` constraint,
/// since Circle's own `remote_token_messenger` account has none; worth a
/// devnet dry run before trusting on mainnet.
fn derive_cctp_remote_token_messenger(domain: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            CCTP_REMOTE_TOKEN_MESSENGER_SEED,
            domain.to_string().as_bytes(),
        ],
        &cctp_token_messenger_minter_id(),
    )
    .0
}
fn derive_cctp_token_minter() -> Pubkey {
    Pubkey::find_program_address(&[CCTP_TOKEN_MINTER_SEED], &cctp_token_messenger_minter_id()).0
}
fn derive_cctp_local_token(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_LOCAL_TOKEN_SEED, mint.as_ref()],
        &cctp_token_messenger_minter_id(),
    )
    .0
}
fn derive_cctp_event_authority() -> Pubkey {
    Pubkey::find_program_address(
        &[CCTP_EVENT_AUTHORITY_SEED],
        &cctp_token_messenger_minter_id(),
    )
    .0
}

/// `ReceiverConfig.cctp_domain_id` is written once, at `register_merchant`
/// time, by the SAME `validate_route` the on-chain program itself uses (see
/// lib.rs) — reading it back here avoids re-implementing that chain-name ->
/// domain-id mapping (and factory_config's per-chain domain fields) a
/// second time, off-chain, where it could drift from the on-chain version.
/// Byte layout mirrors `ReceiverConfig` exactly: 8-byte Anchor
/// discriminator, then receiver/merchant/mint/merchant_token_account/
/// cctp_mint_chain/cctp_mint_recipient (32 bytes each, six fields), then
/// cctp_domain_id as a little-endian u32.
async fn fetch_cctp_domain_id(rpc: &RpcClient, receiver_config: &Pubkey) -> Result<u32> {
    let account = rpc
        .get_account(receiver_config)
        .await
        .context("receiver_config account not found on-chain")?;
    const DOMAIN_OFFSET: usize = 8 + 32 * 6;
    let bytes = account
        .data
        .get(DOMAIN_OFFSET..DOMAIN_OFFSET + 4)
        .context("receiver_config account too short to contain cctp_domain_id")?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Route straight from the on-chain `ReceiverConfig` — the same account the
/// program's own `sweep` uses to decide same-chain vs cross-chain, so it can
/// never disagree with it. Used only when the indexer has no route for a
/// receiver (e.g. `Registered` with no announce seen).
/// Layout: 8-byte discriminator, receiver/merchant/mint/merchant_token_account
/// (32 bytes each), then cctp_mint_chain and cctp_mint_recipient.
pub async fn fetch_receiver_route(
    rpc: &RpcClient,
    receiver_config: &Pubkey,
) -> Result<SolanaRoute> {
    let account = rpc
        .get_account(receiver_config)
        .await
        .context("receiver_config account not found on-chain")?;
    const ROUTE_OFFSET: usize = 8 + 32 * 4;
    let b = account
        .data
        .get(ROUTE_OFFSET..ROUTE_OFFSET + 64)
        .context("receiver_config account too short to contain route")?;
    Ok(SolanaRoute {
        chain: b[..32].try_into().unwrap(),
        recipient: b[32..].try_into().unwrap(),
    })
}

/// `treasury_token_account` from the on-chain `FactoryConfig`
/// (8-byte discriminator, mint, then treasury_token_account).
pub async fn fetch_treasury_token_account(
    rpc: &RpcClient,
    factory_config: &Pubkey,
) -> Result<Pubkey> {
    let account = rpc
        .get_account(factory_config)
        .await
        .context("factory_config account not found on-chain")?;
    let b = account
        .data
        .get(8 + 32..8 + 64)
        .context("factory_config account too short to contain treasury_token_account")?;
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

// ── Borsh-shaped instruction data (hand-encoded, no extra `borsh` dep) ──────
// Anchor encodes args in declaration order with no extra framing beyond the
// 8-byte discriminator: fixed-size types ([u8; 32], Pubkey) are written raw,
// `Vec<u8>` gets a 4-byte little-endian length prefix. This matches
// `registerMerchant(merchant, chain, recipient)` and `announceMerchant(
// merchant, chain, recipient, receiver, reg_tx)`'s call sites in the test.

fn encode_register_merchant_args(
    merchant: &Pubkey,
    chain: &[u8; 32],
    recipient: &[u8; 32],
) -> Vec<u8> {
    let mut data = anchor_discriminator("register_merchant").to_vec();
    data.extend_from_slice(merchant.as_ref());
    data.extend_from_slice(chain);
    data.extend_from_slice(recipient);
    data
}

fn encode_announce_merchant_args(
    merchant: &Pubkey,
    chain: &[u8; 32],
    recipient: &[u8; 32],
    receiver: &Pubkey,
    reg_tx: &[u8],
) -> Vec<u8> {
    let mut data = anchor_discriminator("announce_merchant").to_vec();
    data.extend_from_slice(merchant.as_ref());
    data.extend_from_slice(chain);
    data.extend_from_slice(recipient);
    data.extend_from_slice(receiver.as_ref());
    data.extend_from_slice(&(reg_tx.len() as u32).to_le_bytes());
    data.extend_from_slice(reg_tx);
    data
}

/// Polls `getSlot` until at least `n` slots have passed. Durable nonces
/// can't be advanced in the same slot they (or their last advance) landed
/// in — same rule the test works around with `waitSlots(2)`.
async fn wait_slots(rpc: &RpcClient, n: u64) -> Result<()> {
    let start = rpc.get_slot().await.context("get_slot failed")?;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let cur = rpc.get_slot().await.context("get_slot failed")?;
        if cur >= start + n {
            return Ok(());
        }
    }
}

// ── Balance filter ──────────────────────────────────────────────────────────

/// Batched SPL token balance check, same role as EVM's Multicall3 `balanceOf`
/// batch / Starknet's `batch_check_nonzero_balance` — one RPC round trip
/// (`getMultipleAccounts`) covers every candidate.
pub async fn batch_check_nonzero_balance(
    rpc: &RpcClient,
    receiver_token_accounts: &[Pubkey],
) -> Result<HashSet<Pubkey>> {
    if receiver_token_accounts.is_empty() {
        return Ok(HashSet::new());
    }

    let mut nonzero = HashSet::with_capacity(receiver_token_accounts.len());
    // getMultipleAccounts caps at 100 pubkeys/request.
    for chunk in receiver_token_accounts.chunks(100) {
        let accounts = rpc
            .get_multiple_accounts(chunk)
            .await
            .context("getMultipleAccounts failed for receiver token accounts")?;
        for (&pubkey, acct) in chunk.iter().zip(accounts.iter()) {
            match acct {
                Some(acct) => match spl_token::state::Account::unpack(&acct.data) {
                    Ok(unpacked) => {
                        if unpacked.amount != 0 {
                            nonzero.insert(pubkey);
                        }
                    }
                    Err(e) => {
                        // Can't tell if it's really zero — don't guess, same
                        // posture as the EVM multicall's revert handling.
                        log::error!(
                            "failed unpacking token account {pubkey}: {e} — treating as unknown, not zero"
                        );
                        nonzero.insert(pubkey);
                    }
                },
                None => {} // account doesn't exist yet — definitionally zero
            }
        }
    }
    Ok(nonzero)
}

// ── JIT registration: prepare + announce ────────────────────────────────────
//
// Mirrors `prepare()` / the `registerIx` half of `signRegTx()` / `announce()`
// in tests/solana_beanie.ts, run entirely by the keeper instead of a test
// harness. `receiver` is a throwaway `Keypair` the CALLER generates right
// before invoking `prepare_registration` and never touches again afterward
// (see create_workers.rs) — its private key exists only long enough to sign
// the one `register_merchant` instruction below, then it's dropped. Nothing
// here persists it, sends it anywhere, or expects it back later.

/// Builds the durable-nonce account and the ATAs `register_merchant` needs
/// (keeper-paid, keeper-signed, sent immediately — this is `tx1` in the
/// test), then builds and signs the actual `register_merchant` transaction
/// against that nonce with (keeper, receiver) and returns it serialized,
/// WITHOUT sending it. The caller is responsible for pinning these bytes via
/// `announce_merchant` and then submitting them via
/// `broadcast_pending_registration`.
///
/// `merchant_token_account` is the merchant's own USDC ATA — derived here,
/// not supplied by anyone, and created idempotently in case it doesn't exist
/// yet. This is the field earlier drafts wrongly expected the client to
/// hand-pick; a merchant only ever has one natural token account per mint.
pub async fn prepare_registration(
    rpc: &RpcClient,
    keeper: &Keypair,
    receiver: &Keypair,
    program_id: Pubkey,
    merchant: Pubkey,
    chain: [u8; 32],
    recipient: [u8; 32],
    usdc_mint: Pubkey,
) -> Result<Vec<u8>> {
    let receiver_pk = receiver.pubkey();
    let factory_config = derive_factory_config(&program_id);
    let receiver_config =
        derive_receiver_config(&program_id, &merchant, &receiver_pk, &chain, &recipient);
    let pending_registration =
        derive_pending_registration(&program_id, &merchant, &receiver_pk, &chain, &recipient);
    let merchant_registry = derive_merchant_registry(&program_id, &merchant);

    let receiver_ta = get_associated_token_address(&receiver_pk, &usdc_mint);
    // Owner is a PDA (off-curve) — the derivation formula doesn't care;
    // `allowOwnerOffCurve` in the JS SDK is a client-side assertion only.
    let staging_ta = get_associated_token_address(&receiver_config, &usdc_mint);
    let merchant_ta = get_associated_token_address(&merchant, &usdc_mint);

    // --- tx1: durable nonce account + the three ATAs. Keeper-paid,
    //     keeper-signed (+ the fresh nonce keypair, required to sign its own
    //     `create_account`). Sent and confirmed immediately.
    let nonce_kp = Keypair::new();
    let nonce_rent = rpc
        .get_minimum_balance_for_rent_exemption(NONCE_ACCOUNT_LENGTH)
        .await
        .context("get_minimum_balance_for_rent_exemption failed")?;

    let mut setup_ixs = system_instruction::create_nonce_account(
        &keeper.pubkey(),
        &nonce_kp.pubkey(),
        &keeper.pubkey(), // nonce authority
        nonce_rent,
    );
    setup_ixs.push(create_associated_token_account_idempotent(
        &keeper.pubkey(),
        &receiver_pk,
        &usdc_mint,
        &spl_token::ID,
    ));
    setup_ixs.push(create_associated_token_account_idempotent(
        &keeper.pubkey(),
        &receiver_config,
        &usdc_mint,
        &spl_token::ID,
    ));
    setup_ixs.push(create_associated_token_account_idempotent(
        &keeper.pubkey(),
        &merchant,
        &usdc_mint,
        &spl_token::ID,
    ));

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .context("failed fetching blockhash for nonce/ATA setup tx")?;
    let setup_tx = LegacyTransaction::new_signed_with_payer(
        &setup_ixs,
        Some(&keeper.pubkey()),
        &[keeper, &nonce_kp],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&setup_tx)
        .await
        .context("nonce/ATA setup tx failed")?;

    // A durable nonce can't be advanced in the slot it (or its last advance)
    // landed in.
    wait_slots(rpc, 2).await?;

    let nonce_account = rpc
        .get_account(&nonce_kp.pubkey())
        .await
        .context("failed fetching nonce account after setup")?;
    let nonce_data = nonce_utils::data_from_account(&nonce_account)
        .context("nonce account did not decode as an initialized durable nonce")?;
    let durable_nonce_hash = nonce_data.blockhash();

    // --- tx2: [AdvanceNonce, register_merchant], signed by (keeper,
    //     receiver) against the durable nonce. This is the ONLY place
    //     `receiver`'s private key is ever used. Not sent here — returned as
    //     bytes for the caller to pin via announce_merchant, then submit via
    //     broadcast_pending_registration.
    let advance_ix =
        system_instruction::advance_nonce_account(&nonce_kp.pubkey(), &keeper.pubkey());

    let register_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(keeper.pubkey(), true),      // payer
            AccountMeta::new_readonly(receiver_pk, true), // receiver (approve authority)
            AccountMeta::new_readonly(factory_config, false),
            AccountMeta::new(receiver_ta, false),
            AccountMeta::new_readonly(merchant_ta, false),
            AccountMeta::new(receiver_config, false),
            AccountMeta::new_readonly(staging_ta, false),
            AccountMeta::new(merchant_registry, false),
            AccountMeta::new(pending_registration, false), // closed here, rent -> payer
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: encode_register_merchant_args(&merchant, &chain, &recipient),
    };

    let reg_tx = LegacyTransaction::new_signed_with_payer(
        &[advance_ix, register_ix],
        Some(&keeper.pubkey()),
        &[keeper, receiver],
        durable_nonce_hash,
    );

    bincode::serialize(&reg_tx).context("failed serializing signed register_merchant tx")
}

/// `announce_merchant(merchant, chain, recipient, receiver, reg_tx)` —
/// keeper-signed only, per the program's `AnnounceMerchant` account context
/// (fee payer + the two PDAs it touches, nothing else). Pins `reg_tx`
/// on-chain at `pending_registration` for later broadcast.
pub async fn announce_merchant(
    rpc: &RpcClient,
    keeper: &Keypair,
    program_id: Pubkey,
    merchant: Pubkey,
    chain: [u8; 32],
    recipient: [u8; 32],
    receiver: Pubkey,
    reg_tx: &[u8],
) -> Result<String> {
    let factory_config = derive_factory_config(&program_id);
    let pending_registration =
        derive_pending_registration(&program_id, &merchant, &receiver, &chain, &recipient);

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(keeper.pubkey(), true), // payer
            AccountMeta::new_readonly(factory_config, false),
            AccountMeta::new(pending_registration, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: encode_announce_merchant_args(&merchant, &chain, &recipient, &receiver, reg_tx),
    };

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .context("failed fetching blockhash for announce_merchant")?;
    let tx = LegacyTransaction::new_signed_with_payer(
        &[ix],
        Some(&keeper.pubkey()),
        &[keeper],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .context("announce_merchant failed")?;
    Ok(sig.to_string())
}

/// Optional cleanup: reclaims the durable nonce's rent once a registration
/// has been announced and broadcast, mirroring NC-HAPPY-1/NC-HAPPY-2's
/// "close after use" pattern. Safe to call best-effort — a failure here
/// (already closed, still needed, etc.) shouldn't fail the registration
/// that already succeeded.
pub async fn close_nonce_account(
    rpc: &RpcClient,
    keeper: &Keypair,
    nonce_pubkey: &Pubkey,
) -> Result<String> {
    let account = rpc
        .get_account(nonce_pubkey)
        .await
        .context("nonce account not found")?;
    let ix = system_instruction::withdraw_nonce_account(
        nonce_pubkey,
        &keeper.pubkey(),
        &keeper.pubkey(),
        account.lamports,
    );
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .context("failed fetching blockhash for nonce close")?;
    let tx = LegacyTransaction::new_signed_with_payer(
        &[ix],
        Some(&keeper.pubkey()),
        &[keeper],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .context("nonce close failed")?;
    Ok(sig.to_string())
}

// ── JIT registration broadcast ──────────────────────────────────────────────

/// Broadcasts a pinned `reg_tx` UNCHANGED — it is already fully signed
/// (payer + receiver, over a durable nonce, per `announce_merchant`'s
/// contract). The keeper is a pure relayer here: it does not construct,
/// sign, or modify anything. This is the one place in the whole keeper
/// (across all three chains) where a "registration" isn't built by us.
pub async fn broadcast_pending_registration(rpc: &RpcClient, reg_tx: &[u8]) -> Result<String> {
    let tx: LegacyTransaction =
        bincode::deserialize(reg_tx).context("stored reg_tx does not decode as a Transaction")?;
    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .context("register_merchant broadcast failed")?;
    Ok(sig.to_string())
}

// ── Sweep instruction builders ──────────────────────────────────────────────

struct SameChainSweepAccounts {
    caller: Pubkey,
    caller_token_account: Pubkey,
    receiver_token_account: Pubkey,
    factory_config: Pubkey,
    treasury_token_account: Pubkey,
    receiver_config: Pubkey,
    merchant_token_account: Pubkey,
}

fn build_same_chain_sweep_ix(program_id: &Pubkey, a: &SameChainSweepAccounts) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(a.caller, true),
            AccountMeta::new(a.caller_token_account, false),
            AccountMeta::new(a.receiver_token_account, false),
            AccountMeta::new_readonly(a.factory_config, false),
            AccountMeta::new(a.treasury_token_account, false),
            AccountMeta::new_readonly(a.receiver_config, false),
            AccountMeta::new(a.merchant_token_account, false),
            // Option<Account> fields the client leaves as "None" for a
            // same-chain sweep — Anchor's None sentinel for an
            // Option<Account<'info, T>> in the accounts list is the
            // program id itself (see solana_beanie.ts's `programId` usage
            // for `merchantTokenAccount`/omitted CCTP accounts).
            AccountMeta::new_readonly(*program_id, false), // cctp_burn_staging_account: None
            AccountMeta::new_readonly(*program_id, false), // event_rent_payer: None
            AccountMeta::new_readonly(*program_id, false), // message_sent_event_data: None
            AccountMeta::new_readonly(*program_id, false), // burn_token_mint: None
            AccountMeta::new_readonly(*program_id, false), // cctp_sender_authority_pda: None
            AccountMeta::new_readonly(*program_id, false), // cctp_denylist_account: None
            AccountMeta::new_readonly(*program_id, false), // cctp_message_transmitter: None
            AccountMeta::new_readonly(*program_id, false), // cctp_token_messenger: None
            AccountMeta::new_readonly(*program_id, false), // cctp_remote_token_messenger: None
            AccountMeta::new_readonly(*program_id, false), // cctp_token_minter: None
            AccountMeta::new_readonly(*program_id, false), // cctp_local_token: None
            AccountMeta::new_readonly(*program_id, false), // cctp_event_authority: None
            AccountMeta::new_readonly(*program_id, false), // cctp_message_transmitter_program: None
            AccountMeta::new_readonly(*program_id, false), // cctp_token_messenger_minter_program: None
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: anchor_discriminator("sweep").to_vec(), // sweep() takes no args
    }
}

/// Same-chain sweeps only — `Registered` receivers whose `cctp_mint_chain`
/// is zeroed. Cross-chain (CCTP) sweeps are NOT batched here: each one adds
/// two ephemeral co-signers (`event_rent_payer`, `message_sent_event_data`)
/// and ~14 extra accounts, which eats the per-tx budget fast enough that
/// batching them together buys little. See `sweep_cross_chain_receiver` for
/// the one-sweep-per-tx path.
pub async fn multicall_sweep_same_chain(
    rpc: &RpcClient,
    keeper: &Keypair,
    cfg: &SolanaConfig,
    registered: &[(
        SolanaReceiverRecord,
        Pubkey, /* merchant_token_account */
    )],
) -> Result<Option<String>> {
    if registered.is_empty() {
        return Ok(None);
    }

    let mut sent = Vec::new();
    for batch in registered.chunks(MAX_SAME_CHAIN_SWEEPS_PER_TX) {
        let instructions: Vec<Instruction> = batch
            .iter()
            .map(|(rec, merchant_ta)| {
                build_same_chain_sweep_ix(
                    &cfg.program_id,
                    &SameChainSweepAccounts {
                        caller: keeper.pubkey(),
                        caller_token_account: cfg.caller_token_account,
                        receiver_token_account: rec.receiver_token_account,
                        factory_config: cfg.factory_config,
                        treasury_token_account: cfg.treasury_token_account,
                        receiver_config: rec.receiver_config,
                        merchant_token_account: *merchant_ta,
                    },
                )
            })
            .collect();

        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .context("failed fetching blockhash for sweep tx")?;
        let tx = LegacyTransaction::new_signed_with_payer(
            &instructions,
            Some(&keeper.pubkey()),
            &[keeper],
            blockhash,
        );

        // Pass the transaction itself: a serialized Vec<u8> does not
        // implement the RPC crate's SerializableTransaction trait.
        match rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => {
                log::info!(
                    "[{}] solana multicall swept {} receiver(s) -> tx {}",
                    now_formatted(),
                    batch.len(),
                    sig
                );
                sent.push(sig.to_string());
            }
            Err(e) => {
                log::error!(
                    "solana sweep batch failed ({} receivers): {e:#}",
                    batch.len()
                );
            }
        }
    }

    Ok(sent.into_iter().last())
}

// ── Cross-chain sweep (CCTP) ─────────────────────────────────────────────────

struct CrossChainSweepAccounts {
    caller: Pubkey,
    caller_token_account: Pubkey,
    receiver_token_account: Pubkey,
    factory_config: Pubkey,
    treasury_token_account: Pubkey,
    receiver_config: Pubkey,
    cctp_burn_staging_account: Pubkey,
    event_rent_payer: Pubkey,
    message_sent_event_data: Pubkey,
    burn_token_mint: Pubkey,
    cctp_sender_authority_pda: Pubkey,
    cctp_denylist_account: Pubkey,
    cctp_message_transmitter: Pubkey,
    cctp_token_messenger: Pubkey,
    cctp_remote_token_messenger: Pubkey,
    cctp_token_minter: Pubkey,
    cctp_local_token: Pubkey,
    cctp_event_authority: Pubkey,
}

/// Account order MUST exactly match `Sweep<'info>` in lib.rs — Anchor
/// deserializes accounts positionally, not by name. `merchant_token_account`
/// is left at the "None" sentinel (`program_id`), same convention
/// `build_same_chain_sweep_ix` uses for ITS unused (CCTP) slots — a
/// cross-chain sweep never reads `merchant_token_account` on-chain (lib.rs's
/// `sweep` only touches it inside the `!cross_chain` branch). This account
/// list, and the mut/signer flags on each entry, were verified directly
/// against Circle's real, deployed V2 `deposit_for_burn` instruction
/// (circlefin/solana-cctp-contracts) — see the doc comment on the CCTP PDA
/// derivation functions above.
fn build_cross_chain_sweep_ix(program_id: &Pubkey, a: &CrossChainSweepAccounts) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(a.caller, true),
            AccountMeta::new(a.caller_token_account, false),
            AccountMeta::new(a.receiver_token_account, false),
            AccountMeta::new_readonly(a.factory_config, false),
            AccountMeta::new(a.treasury_token_account, false),
            AccountMeta::new_readonly(a.receiver_config, false),
            AccountMeta::new_readonly(*program_id, false), // merchant_token_account: None
            AccountMeta::new(a.cctp_burn_staging_account, false),
            AccountMeta::new(a.event_rent_payer, true),
            AccountMeta::new(a.message_sent_event_data, true),
            AccountMeta::new(a.burn_token_mint, false),
            AccountMeta::new_readonly(a.cctp_sender_authority_pda, false),
            AccountMeta::new_readonly(a.cctp_denylist_account, false),
            AccountMeta::new(a.cctp_message_transmitter, false),
            AccountMeta::new_readonly(a.cctp_token_messenger, false),
            AccountMeta::new_readonly(a.cctp_remote_token_messenger, false),
            AccountMeta::new_readonly(a.cctp_token_minter, false),
            AccountMeta::new(a.cctp_local_token, false),
            AccountMeta::new_readonly(a.cctp_event_authority, false),
            AccountMeta::new_readonly(cctp_message_transmitter_id(), false),
            AccountMeta::new_readonly(cctp_token_messenger_minter_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: anchor_discriminator("sweep").to_vec(), // same no-arg data as same-chain
    }
}

/// One sweep per transaction, deliberately not batched — a cross-chain
/// sweep adds two ephemeral co-signers (`event_rent_payer`,
/// `message_sent_event_data`) plus ~14 extra accounts on top of what a
/// same-chain sweep needs, so several together would blow the 1232-byte
/// packet / account-count ceiling `multicall_sweep_same_chain` is already
/// sized against.
///
/// `message_sent_event_data` is a fresh, single-use keypair generated here
/// and used only to co-sign this one transaction — Circle's
/// MessageTransmitterV2 program initializes it via CPI inside
/// `deposit_for_burn`, the same "ephemeral account, signs once, never
/// touched again" shape `prepare_registration`'s nonce keypair already
/// uses elsewhere in this file. `event_rent_payer` is the keeper itself,
/// since the keeper already pays for and signs the whole transaction.
pub async fn sweep_cross_chain_receiver(
    rpc: &RpcClient,
    keeper: &Keypair,
    cfg: &SolanaConfig,
    rec: &SolanaReceiverRecord,
) -> Result<String> {
    let domain = fetch_cctp_domain_id(rpc, &rec.receiver_config).await?;
    let event_data_kp = Keypair::new();

    let accounts = CrossChainSweepAccounts {
        caller: keeper.pubkey(),
        caller_token_account: cfg.caller_token_account,
        receiver_token_account: rec.receiver_token_account,
        factory_config: cfg.factory_config,
        treasury_token_account: cfg.treasury_token_account,
        receiver_config: rec.receiver_config,
        cctp_burn_staging_account: get_associated_token_address(&rec.receiver_config, &cfg.mint),
        event_rent_payer: keeper.pubkey(),
        message_sent_event_data: event_data_kp.pubkey(),
        burn_token_mint: cfg.mint,
        cctp_sender_authority_pda: derive_cctp_sender_authority_pda(),
        cctp_denylist_account: derive_cctp_denylist_account(&rec.receiver_config),
        cctp_message_transmitter: derive_cctp_message_transmitter(),
        cctp_token_messenger: derive_cctp_token_messenger(),
        cctp_remote_token_messenger: derive_cctp_remote_token_messenger(domain),
        cctp_token_minter: derive_cctp_token_minter(),
        cctp_local_token: derive_cctp_local_token(&cfg.mint),
        cctp_event_authority: derive_cctp_event_authority(),
    };

    let ix = build_cross_chain_sweep_ix(&cfg.program_id, &accounts);

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .context("failed fetching blockhash for cross-chain sweep tx")?;
    let tx = LegacyTransaction::new_signed_with_payer(
        &[ix],
        Some(&keeper.pubkey()),
        &[keeper, &event_data_kp],
        blockhash,
    );

    let sig = rpc
        .send_and_confirm_transaction(&tx)
        .await
        .context("cross-chain sweep failed")?;
    log::info!(
        "[{}] solana cross-chain sweep for {} -> tx {} (domain {domain})",
        now_formatted(),
        rec.receiver,
        sig
    );
    Ok(sig.to_string())
}

/// Sweeps `Registered` receivers, picking the instruction per receiver:
/// zero route -> batched same-chain sweep, non-zero route -> one CCTP sweep
/// per tx. A route missing from the indexer is read from chain; if that
/// fails the receiver is skipped, never guessed (a wrong guess reverts, and
/// would take a whole same-chain batch down with it).
/// Returns the last tx signature that landed.
pub async fn sweep_registered(
    rpc: &RpcClient,
    keeper: &Keypair,
    cfg: &SolanaConfig,
    registered: &[(
        SolanaReceiverRecord,
        Pubkey, /* merchant_token_account */
    )],
) -> Option<String> {
    let mut same_chain = Vec::new();
    let mut last = None;

    for (rec, merchant_ta) in registered {
        let route = match rec.route {
            Some(r) => r,
            None => match fetch_receiver_route(rpc, &rec.receiver_config).await {
                Ok(r) => r,
                Err(e) => {
                    log::error!("skipping sweep for {}: route unknown: {e:#}", rec.receiver);
                    continue;
                }
            },
        };

        if route.chain == [0u8; 32] {
            same_chain.push((rec.clone(), *merchant_ta));
        } else {
            match sweep_cross_chain_receiver(rpc, keeper, cfg, rec).await {
                Ok(sig) => last = Some(sig),
                Err(e) => log::error!("cross-chain sweep failed for {}: {e:#}", rec.receiver),
            }
        }
    }

    let same = multicall_sweep_same_chain(rpc, keeper, cfg, &same_chain)
        .await
        .unwrap_or_else(|e| {
            log::error!("same-chain sweep failed: {e:#}");
            None
        });
    same.or(last)
}

//! src/solana_keeper.rs
//!
//! Helius RPC for state reads + tx submission. Event discovery never comes
//! through here — see solana_indexer.rs / solana_ws.rs — mirrors
//! `evm_rpc_url` being state-reads-and-sends-only in `EvmConfig`.

use anchor_lang::system_program;
use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction as LegacyTransaction;
use std::collections::HashSet;
use std::sync::Arc;

use crate::config::{SolanaConfig, now_formatted};
use crate::solana_indexer::SolanaReceiverRecord;

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
/// and ~11 extra accounts, which eats the per-tx budget fast enough that
/// batching them together buys little. See `sweep_cross_chain_receiver` for
/// the one-sweep-per-tx path — not implemented in this draft; wire up once
/// real CCTP CPI accounts (sender authority PDA, message transmitter, etc.)
/// are sourced, same TODO the Anchor test suite itself leaves `.skip`ped.
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

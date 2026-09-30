use std::sync::Arc;

use ethers::types::Address;
use ethers::utils::keccak256;
use log::{info, warn};
use solana_sdk::signature::Signer;
use starknet::accounts::Account;
use starknet::core::types::{Call, Felt};
use starknet::core::utils::get_selector_from_name;
use tokio::sync::mpsc;

use crate::models::{
    AnnounceTask, Chain, ReceiverFactory, derive_felt_from_foreign_address,
    derive_pubkey_from_foreign_address,
};
use crate::payment_workers::StarknetAccount;

// ---------------------------------------------------------------------------
// CCTP route encoding. Arbitrum and Monad both slot into the same EVM
// branch as Base/Ethereum since it's the same ABI, just a different
// deployment (and, correctly, its own client) — passed in via
// `evm_targets` below, keyed by chain.
// ---------------------------------------------------------------------------

pub(crate) fn same_chain(a: &Chain, b: &Chain) -> bool {
    matches!(
        (a, b),
        (Chain::Base, Chain::Base)
            | (Chain::Ethereum, Chain::Ethereum)
            | (Chain::Arbitrum, Chain::Arbitrum)
            | (Chain::Monad, Chain::Monad)
            | (Chain::Starknet, Chain::Starknet)
            | (Chain::Solana, Chain::Solana)
    )
}

pub(crate) fn chain_key(chain: &Chain) -> Option<&'static str> {
    match chain {
        Chain::Base => Some("BASE"),
        Chain::Ethereum => Some("ETHEREUM"),
        Chain::Arbitrum => Some("ARBITRUM"),
        Chain::Monad => Some("MONAD"),
        Chain::Starknet => Some("STARKNET"),
        Chain::Solana => Some("SOLANA"),
    }
}

pub(crate) fn recipient_bytes32(target: &Chain, recipient: &str) -> Option<[u8; 32]> {
    match target {
        Chain::Base | Chain::Ethereum | Chain::Arbitrum | Chain::Monad => {
            let addr: Address = recipient.parse().ok()?;
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(addr.as_bytes());
            Some(out)
        }
        Chain::Starknet => Some(Felt::from_hex(recipient).ok()?.to_bytes_be()),
        Chain::Solana => {
            let pk: solana_sdk::pubkey::Pubkey = recipient.parse().ok()?;
            Some(pk.to_bytes())
        }
    }
}

fn evm_route(source: &Chain, target: &Chain, recipient: &str) -> Option<([u8; 32], [u8; 32])> {
    if same_chain(source, target) {
        return Some(([0u8; 32], [0u8; 32]));
    }
    let name = chain_key(target)?;
    let mut chain = [0u8; 32];
    chain[..name.len()].copy_from_slice(name.as_bytes());
    Some((chain, recipient_bytes32(target, recipient)?))
}

fn starknet_route(source: &Chain, target: &Chain, recipient: &str) -> Option<[Felt; 3]> {
    if same_chain(source, target) {
        return Some([Felt::ZERO; 3]);
    }
    let name = chain_key(target)?;
    let chain = Felt::from_bytes_be_slice(name.as_bytes());
    let r = recipient_bytes32(target, recipient)?;
    let high = u128::from_be_bytes(r[..16].try_into().ok()?);
    let low = u128::from_be_bytes(r[16..].try_into().ok()?);
    Some([chain, Felt::from(low), Felt::from(high)])
}

/// Solana's route encoding: 32-byte chain-name buffer + 32-byte recipient,
/// exactly what `announce_merchant`/`register_merchant` take per
/// `chainNameSeed()` and `recipient_bytes32`-equivalent in the test. Solana's
/// own `SAME_CHAIN_DOMAIN_SENTINEL` handling is internal to the program (it's
/// applied to the CCTP *domain*, not this chain/recipient pair), so the
/// same-chain zero-buffer convention below matches `ZERO_32` in the test.
pub(crate) fn solana_route(
    source: &Chain,
    target: &Chain,
    recipient: &str,
) -> Option<([u8; 32], [u8; 32])> {
    if same_chain(source, target) {
        return Some(([0u8; 32], [0u8; 32]));
    }
    let name = chain_key(target)?;
    let mut chain = [0u8; 32];
    chain[..name.len()].copy_from_slice(name.as_bytes());
    Some((chain, recipient_bytes32(target, recipient)?))
}

/// Background worker: receives AnnounceTask and performs the on-chain
/// announce for whichever chain it targets.
///
/// For EVM chains and Starknet this is a single call, same as before.
///
/// For Solana this reproduces `prepare()` + `announce()` +
/// `broadcastStored()` from `tests/solana_beanie.ts` end to end, entirely
/// server-side:
///   1. Generate a fresh, throwaway `receiver` keypair — never persisted,
///      never sent anywhere, dropped at the end of this function's scope.
///   2. Build & send `tx1`: create the durable nonce account (authority =
///      keeper), the receiver's ATA, and the staging ATA — keeper-paid,
///      keeper-signed only.
///   3. Build the `[AdvanceNonce, register_merchant]` tx against that nonce
///      and sign it with (keeper, receiver). This is the one and only
///      operation that ever touches the receiver's private key.
///   4. Submit `announce_merchant(merchant, chain, recipient, receiver,
///      reg_tx_bytes)` — keeper-signed only, per the program's
///      `AnnounceMerchant` account context (fee payer only, no receiver
///      signature needed here).
///   5. Immediately broadcast the just-announced blob, same as
///      `broadcastStored()` — no reason to wait for a second, separate
///      keeper pass since the keeper already holds the bytes in memory.
///
/// NOTE: steps 2-5 below are written against the same account/instruction
/// shapes the test uses (`registerMerchant`, `announceMerchant`, the
/// `factory`/`config`/`pending`/`registry` PDA seeds), but wired through
/// whatever Rust client you use to talk to the `sol` program — an
/// Anchor-generated Rust client, `anchor-client`, or hand-built
/// `Instruction`s. Plug that in where marked; I don't have
/// `beanie_keeper::solana_keeper`'s contents so I can't give you the exact
/// call signatures for e.g. `build_client`, the program's Rust IDL type, or
/// how you're currently deriving PDAs on the keeper side.
pub async fn run_announce_worker(
    // One (signer client, factory address) pair per EVM-family chain — a
    // single shared `evm_client` here was wrong: Base and Arbitrum are
    // different RPC endpoints/signers even though they share the same ABI.
    // `Chain::Ethereum` intentionally has no separate client yet (none is
    // built in main.rs); callers should point it at the same entry as Base
    // until a real Ethereum client exists.
    evm_targets: std::collections::HashMap<
        Chain,
        (Arc<beanie_keeper::evm_keeper::SignerProvider>, Address),
    >,
    starknet_account: Arc<StarknetAccount>,
    // Solana equivalents of `evm_client`/`starknet_account` — an RPC client
    // plus the keeper's Solana keypair (or a wrapper around both, matching
    // whatever `solana_keeper::build_client` already returns elsewhere).
    solana_rpc: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    solana_keeper: Arc<solana_sdk::signature::Keypair>,
    starknet_factory_addr: Felt,
    solana_program_id: solana_sdk::pubkey::Pubkey,
    usdc_mint: solana_sdk::pubkey::Pubkey,
    mut rx: mpsc::Receiver<AnnounceTask>,
) {
    info!("Announce worker starting");

    while let Some(task) = rx.recv().await {
        match task.chain {
            Chain::Base | Chain::Ethereum | Chain::Arbitrum | Chain::Monad => {
                let merchant: Address = task.merchant_address.parse().unwrap_or_else(|_| {
                    let hash = keccak256(task.merchant_address.as_bytes());
                    Address::from_slice(&hash[12..32])
                });

                let Some((chain_client, factory_addr)) = evm_targets.get(&task.chain).cloned()
                else {
                    warn!(
                        "announce worker: no client/factory configured for {:?}",
                        task.chain
                    );
                    continue;
                };

                let Some((cctp_chain, cctp_recipient)) =
                    evm_route(&task.chain, &task.target_chain, &task.target_recipient)
                else {
                    warn!(
                        "announce worker: unusable settlement route {:?} -> {:?} ({})",
                        task.chain, task.target_chain, task.target_recipient
                    );
                    continue;
                };

                let factory = ReceiverFactory::new(factory_addr, chain_client);
                let call = factory.announce_receiver(merchant, cctp_chain, cctp_recipient);

                match call.send().await {
                    Ok(pending) => match pending.await {
                        Ok(Some(receipt)) => println!(
                            "announceReceiver ok chain={:?} merchant={:?} tx={:#x}",
                            task.chain, merchant, receipt.transaction_hash
                        ),
                        Ok(None) => warn!(
                            "announceReceiver dropped chain={:?} merchant={:?}",
                            task.chain, merchant
                        ),
                        Err(e) => warn!(
                            "announceReceiver confirmation failed chain={:?} merchant={:?}: {e}",
                            task.chain, merchant
                        ),
                    },
                    Err(e) => warn!(
                        "announceReceiver send failed chain={:?} merchant={:?}: {e}",
                        task.chain, merchant
                    ),
                }
            }

            Chain::Starknet => {
                let merchant = Felt::from_hex(&task.merchant_address)
                    .unwrap_or_else(|_| derive_felt_from_foreign_address(&task.merchant_address));

                let selector = match get_selector_from_name("announce_receiver") {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("announce worker: selector announce_receiver: {e}");
                        continue;
                    }
                };

                let Some([cctp_chain, recipient_low, recipient_high]) =
                    starknet_route(&task.chain, &task.target_chain, &task.target_recipient)
                else {
                    warn!(
                        "announce worker: unusable settlement route {:?} -> {:?} ({})",
                        task.chain, task.target_chain, task.target_recipient
                    );
                    continue;
                };

                let call = Call {
                    to: starknet_factory_addr,
                    selector,
                    calldata: vec![merchant, cctp_chain, recipient_low, recipient_high],
                };

                match starknet_account.execute_v3(vec![call]).send().await {
                    Ok(pending) => println!(
                        "announce_receiver ok merchant={:#x} tx={:#x}",
                        merchant, pending.transaction_hash
                    ),
                    Err(e) => warn!("announce_receiver failed for {:#x}: {e}", merchant),
                }
            }

            Chain::Solana => {
                // Native base58 pubkey when address was announced on Solana
                // directly; derived otherwise (address came from a
                // different chain's format as a CCTP leg) — mirrors the
                // EVM keccak256 fallback and the Starknet
                // derive_felt_from_foreign_address fallback above. Derived
                // pubkeys don't need to be valid curve points; Pubkey is
                // just 32 bytes used for PDA seeding.
                let merchant = task
                    .merchant_address
                    .parse::<solana_sdk::pubkey::Pubkey>()
                    .unwrap_or_else(|_| derive_pubkey_from_foreign_address(&task.merchant_address));

                let Some((chain_buf, recipient_buf)) =
                    solana_route(&task.chain, &task.target_chain, &task.target_recipient)
                else {
                    warn!(
                        "announce worker: unusable settlement route {:?} -> {:?} ({})",
                        task.chain, task.target_chain, task.target_recipient
                    );
                    continue;
                };

                // --- 1. Throwaway receiver keypair. Lives only for this
                //     iteration of the loop; nothing persists it, nothing
                //     sends it anywhere, nothing needs it again after step 3.
                let receiver_kp = solana_sdk::signature::Keypair::new();
                let receiver = receiver_kp.pubkey();

                // --- 2 & 3. TODO: wire to your actual Solana keeper helpers.
                // This is the part that needs `solana_keeper`'s real API
                // (PDA derivation, ATA creation, durable-nonce setup) to
                // compile — mirroring `prepare()` in the test 1:1:
                //   a. derive config_pda/pending_pda/registry_pda from
                //      (program_id, "config"/"pending"/"registry" seeds,
                //      merchant, receiver, chain_buf, recipient_buf)
                //   b. create + fund a durable nonce account (keeper-signed)
                //   c. create receiver's ATA + the config PDA's staging ATA
                //      (keeper-signed)
                //   d. build `register_merchant(merchant, chain_buf,
                //      recipient_buf)` ix against the accounts above, put it
                //      in a tx behind `AdvanceNonce`, sign with
                //      (solana_keeper, receiver_kp) — this is the *only*
                //      place `receiver_kp` is ever used
                //   e. serialize that signed tx to bytes — this is what the
                //      old code wrongly expected the client to hand over as
                //      `solana_reg_tx_hex`
                let reg_tx_bytes: Vec<u8> =
                    match beanie_keeper::solana_keeper::prepare_registration(
                        &solana_rpc,
                        &solana_keeper,
                        &receiver_kp,
                        solana_program_id,
                        merchant,
                        chain_buf,
                        recipient_buf,
                        usdc_mint,
                    )
                    .await
                    {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!(
                                "announce worker: solana prepare_registration failed for merchant {}: {e}",
                                merchant
                            );
                            continue;
                        }
                    };
                // `receiver_kp` is not touched again after this point —
                // matches "the key is unreachable after return" in the test.

                // --- 4. `announce_merchant(merchant, chain_buf, recipient_buf,
                //     receiver, reg_tx_bytes)` — keeper-signed only.
                let announce_sig = match beanie_keeper::solana_keeper::announce_merchant(
                    &solana_rpc,
                    &solana_keeper,
                    solana_program_id,
                    merchant,
                    chain_buf,
                    recipient_buf,
                    receiver,
                    &reg_tx_bytes,
                )
                .await
                {
                    Ok(sig) => sig,
                    Err(e) => {
                        warn!(
                            "announce worker: solana announce_merchant failed for merchant {}: {e}",
                            merchant
                        );
                        continue;
                    }
                };
                println!(
                    "announce_merchant ok merchant={} receiver={} tx={}",
                    merchant, receiver, announce_sig
                );

                // --- 5. Broadcast the just-pinned blob right away, same
                //     effect as `broadcastStored()` in the test but without
                //     waiting for a separate pass — the keeper already has
                //     the bytes in hand.
                match beanie_keeper::solana_keeper::broadcast_pending_registration(
                    &solana_rpc,
                    &reg_tx_bytes,
                )
                .await
                {
                    Ok(sig) => println!("register_merchant broadcast ok tx={}", sig),
                    Err(e) => warn!(
                        "announce worker: solana broadcast_pending_registration failed for merchant {}: {e}",
                        merchant
                    ),
                }
            }
        }
    }

    println!("Announce worker shutting down (channel closed)");
}

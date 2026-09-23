use std::sync::Arc;

use ethers::types::Address;
use ethers::utils::keccak256;
use log::info;
use starknet::accounts::Account;
use starknet::core::types::{Call, Felt};
use starknet::core::utils::get_selector_from_name;
use tokio::sync::mpsc;

use crate::models::{AnnounceTask, Chain, ReceiverFactory, derive_felt_from_foreign_address};
use crate::payment_workers::StarknetAccount;

// ---------------------------------------------------------------------------
// CCTP route encoding. The route (target chain + recipient) is hashed into the
// receiver's on-chain address, so this must byte-match the client's predict
// call (`cctpRoute` in beanie.js). Same chain => all zeros, as the factories
// require.
// ---------------------------------------------------------------------------

fn same_chain(a: &Chain, b: &Chain) -> bool {
    matches!(
        (a, b),
        (Chain::Base, Chain::Base)
            | (Chain::Ethereum, Chain::Ethereum)
            | (Chain::Starknet, Chain::Starknet)
    )
}

/// Chain-name key the factories use in `validDomains`.
fn chain_key(chain: &Chain) -> Option<&'static str> {
    match chain {
        Chain::Base => Some("BASE"),
        Chain::Ethereum => Some("ETHEREUM"),
        Chain::Starknet => Some("STARKNET"),
        _ => None,
    }
}

/// CCTP mint recipient as bytes32: an EVM address left-padded, a Starknet felt
/// as its 32-byte big-endian form. `recipient` is already canonical (the route
/// sanitized it for `target`).
fn recipient_bytes32(target: &Chain, recipient: &str) -> Option<[u8; 32]> {
    match target {
        Chain::Base | Chain::Ethereum => {
            let addr: Address = recipient.parse().ok()?;
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(addr.as_bytes());
            Some(out)
        }
        Chain::Starknet => Some(Felt::from_hex(recipient).ok()?.to_bytes_be()),
        _ => None,
    }
}

/// `(cctpMintChain, cctpMintRecipient)` for `announceReceiver`. The chain key
/// is left-aligned like a Solidity `bytes32("BASE")` literal.
fn evm_route(source: &Chain, target: &Chain, recipient: &str) -> Option<([u8; 32], [u8; 32])> {
    if same_chain(source, target) {
        return Some(([0u8; 32], [0u8; 32]));
    }
    let name = chain_key(target)?;
    let mut chain = [0u8; 32];
    chain[..name.len()].copy_from_slice(name.as_bytes());
    Some((chain, recipient_bytes32(target, recipient)?))
}

/// `(cctp_mint_chain, recipient_low, recipient_high)` for `announce_receiver`:
/// the chain as a Cairo short string, the recipient as a u256's two halves.
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

/// Background worker: receives AnnounceTask and calls on-chain
/// `announceReceiver` / `announce_receiver` so the native poller can
/// start watching the predicted address.
pub async fn run_announce_worker(
    evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    evm_factory_addr: Address,
    starknet_factory_addr: Felt,
    mut rx: mpsc::Receiver<AnnounceTask>,
) {
    info!("Announce worker starting");

    while let Some(task) = rx.recv().await {
        match task.chain {
            Chain::Base | Chain::Ethereum => {
                // Same compatibility path as the payment worker:
                // native EVM address, otherwise keccak-derived address
                // (handles Starknet / foreign merchant strings).
                let merchant: Address = task.merchant_address.parse().unwrap_or_else(|_| {
                    let hash = keccak256(task.merchant_address.as_bytes());
                    Address::from_slice(&hash[12..32])
                });

                let Some((cctp_chain, cctp_recipient)) =
                    evm_route(&task.chain, &task.target_chain, &task.target_recipient)
                else {
                    eprintln!(
                        "announce worker: unusable settlement route {:?} -> {:?} ({})",
                        task.chain, task.target_chain, task.target_recipient
                    );
                    continue;
                };

                let factory = ReceiverFactory::new(evm_factory_addr, evm_client.clone());
                let call = factory.announce_receiver(merchant, cctp_chain, cctp_recipient);

                match call.send().await {
                    Ok(pending) => match pending.await {
                        Ok(Some(receipt)) => {
                            println!(
                                "announceReceiver ok merchant={:?} (raw={}) tx={:#x}",
                                merchant, task.merchant_address, receipt.transaction_hash
                            );
                        }
                        Ok(None) => {
                            eprintln!(
                                "announceReceiver dropped for merchant {:?} (raw={})",
                                merchant, task.merchant_address
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "announceReceiver confirmation failed for {:?} (raw={}): {e}",
                                merchant, task.merchant_address
                            );
                        }
                    },
                    Err(e) => {
                        eprintln!(
                            "announceReceiver send failed for {:?} (raw={}): {e}",
                            merchant, task.merchant_address
                        );
                    }
                }
            }

            Chain::Starknet => {
                // Same compatibility path as the payment worker:
                // native felt, otherwise derive from foreign (EVM) address.
                let merchant = Felt::from_hex(&task.merchant_address)
                    .unwrap_or_else(|_| derive_felt_from_foreign_address(&task.merchant_address));

                let selector = match get_selector_from_name("announce_receiver") {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("announce worker: selector announce_receiver: {e}");
                        continue;
                    }
                };

                let Some([cctp_chain, recipient_low, recipient_high]) =
                    starknet_route(&task.chain, &task.target_chain, &task.target_recipient)
                else {
                    eprintln!(
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
                    Ok(pending) => {
                        println!(
                            "announce_receiver ok merchant={:#x} (raw={}) tx={:#x}",
                            merchant, task.merchant_address, pending.transaction_hash
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "announce_receiver failed for {:#x} (raw={}): {e}",
                            merchant, task.merchant_address
                        );
                    }
                }
            }

            _ => {
                eprintln!("announce worker: unsupported chain {:?}", task.chain);
            }
        }
    }

    println!("Announce worker shutting down (channel closed)");
}

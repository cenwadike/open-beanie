use std::sync::Arc;

use ethers::types::Address;
use ethers::utils::keccak256;
use starknet::accounts::Account;
use starknet::core::types::{Call, Felt};
use starknet::core::utils::get_selector_from_name;
use tokio::sync::mpsc;

use crate::models::{AnnounceTask, Chain, MerchantFactory, derive_felt_from_foreign_address};
use crate::payment_workers::{EvmSignerProvider, StarknetAccount};

/// Background worker: receives AnnounceTask and calls on-chain
/// `announceReceiver` / `announce_receiver` so the native poller can
/// start watching the predicted address.
pub async fn run_announce_worker(
    evm_client: Arc<EvmSignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    evm_factory_addr: Address,
    starknet_factory_addr: Felt,
    mut rx: mpsc::Receiver<AnnounceTask>,
) {
    println!("Announce worker active...");

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

                let factory = MerchantFactory::new(evm_factory_addr, evm_client.clone());
                let call = factory.announce_receiver(merchant);

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

                let call = Call {
                    to: starknet_factory_addr,
                    selector,
                    calldata: vec![merchant],
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

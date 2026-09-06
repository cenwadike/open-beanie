use starknet::{
    accounts::{Account, ConnectedAccount, SingleOwnerAccount},
    core::types::{BlockId, BlockTag, Call, Felt, FunctionCall},
    core::utils::get_selector_from_name,
    providers::Provider,
    providers::jsonrpc::{HttpTransport, JsonRpcClient},
    signers::LocalWallet as StarknetWallet,
};

use std::sync::Arc;
use tokio::sync::mpsc;

use ethers::providers::Middleware;
#[allow(unused_imports)]
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Provider as EvmProvider},
    signers::LocalWallet,
    types::U256,
};
use ethers::{types::Address, utils::keccak256};

pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, StarknetWallet>;

use crate::models::{ChainXReceiverLocal, MerchantFactory};
use crate::models::{chain_to_bytes32, chain_to_felt, derive_felt_from_foreign_address};

abigen!(
    Multicall3,
    r#"[
        {
            "inputs": [
                {
                    "components": [
                        { "internalType": "address", "name": "target", "type": "address" },
                        { "internalType": "bool", "name": "allowFailure", "type": "bool" },
                        { "internalType": "bytes", "name": "callData", "type": "bytes" }
                    ],
                    "internalType": "struct Multicall3.Call3[]",
                    "name": "calls",
                    "type": "tuple[]"
                }
            ],
            "name": "aggregate3",
            "outputs": [
                {
                    "components": [
                        { "internalType": "bool", "name": "success", "type": "bool" },
                        { "internalType": "bytes", "name": "returnData", "type": "bytes" }
                    ],
                    "internalType": "struct Multicall3.Result[]",
                    "name": "returnData",
                    "type": "tuple[]"
                }
            ],
            "stateMutability": "payable",
            "type": "function"
        }
    ]"#;
);

const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// Payment worker: processes incoming payment notifications, performs JIT
/// receiver creation if missing, and triggers `sweep()` on the receiver.

/// Payment worker: processes incoming payment notifications, performs JIT
/// receiver creation if missing, and triggers `sweep()` on the receiver.
pub async fn run_payment_worker(
    evm_client: Arc<beanie_keeper::evm_keeper::SignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    mut rx: mpsc::Receiver<crate::models::PaymentTask>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    println!("Payment worker active...");

    let starknet_factory_addr = starknet_cfg.factory_address;
    let evm_factory_addr = evm_cfg.factory_address;

    // Local abigen is declared at top-level

    while let Some(mut task) = rx.recv().await {
        task.attempts += 1;

        match task.source_chain {
            crate::models::Chain::Base | crate::models::Chain::Ethereum => {
                // EVM flow
                match task.receiver_address.parse::<Address>() {
                    Ok(receiver_addr) => {
                        // Check whether contract exists by code size
                        let code = evm_client.provider().get_code(receiver_addr, None).await;

                        let exists = match code {
                            Ok(bytes) => !bytes.0.is_empty(),
                            Err(_) => false,
                        };

                        // If receiver missing, we'll include `registerMerchant` in the atomic multicall
                        // instead of doing a separate synchronous deploy.
                        let _ = (&exists, &task.create_if_missing);

                        // Build an atomic multicall: optional registerMerchant (if missing)
                        // then sweep() on the receiver. All sent as one tx.
                        let mut calls = Vec::new();

                        // If receiver doesn't exist, add factory.registerMerchant calldata
                        if !exists && task.create_if_missing {
                            let merchant_addr: Address =
                                task.merchant_address.parse().unwrap_or_else(|_| {
                                    let hash = keccak256(task.merchant_address.as_bytes());
                                    Address::from_slice(&hash[12..32])
                                });

                            // Build bytes32 params based on the specified destination_chain/merchant_address
                            let (cctp_chain_bytes, recipient_bytes) =
                                if task.destination_chain == task.source_chain {
                                    ([0u8; 32], [0u8; 32])
                                } else {
                                    match task.destination_chain {
                                        crate::models::Chain::Starknet => {
                                            match Felt::from_hex(&task.merchant_address) {
                                                Ok(f) => (
                                                    chain_to_bytes32(task.destination_chain),
                                                    f.to_bytes_be(),
                                                ),
                                                Err(e) => {
                                                    eprintln!(
                                                        "Invalid Starknet destination_address: {}",
                                                        e
                                                    );
                                                    continue;
                                                }
                                            }
                                        }
                                        _ => match task.merchant_address.parse::<Address>() {
                                            Ok(addr) => {
                                                let mut buf = [0u8; 32];
                                                buf[12..].copy_from_slice(addr.as_bytes());
                                                (chain_to_bytes32(task.destination_chain), buf)
                                            }
                                            Err(e) => {
                                                eprintln!("Invalid EVM destination_address: {}", e);
                                                continue;
                                            }
                                        },
                                    }
                                };

                            let reg_call =
                                MerchantFactory::new(evm_factory_addr, evm_client.clone())
                                    .register_merchant(
                                        merchant_addr,
                                        cctp_chain_bytes,
                                        recipient_bytes,
                                    );
                            let reg_calldata = reg_call.calldata();

                            match reg_calldata {
                                Some(bytes) => calls.push(Call3 {
                                    target: evm_factory_addr,
                                    allow_failure: false,
                                    call_data: bytes,
                                }),
                                None => {
                                    eprintln!("Failed encoding register_merchant calldata");
                                    continue;
                                }
                            }
                        }

                        // sweep calldata
                        let receiver_contract =
                            ChainXReceiverLocal::new(receiver_addr, evm_client.clone());
                        let sweep_calldata = match receiver_contract.sweep().calldata() {
                            Some(b) => b,
                            None => {
                                eprintln!("failed to encode sweep calldata");
                                continue;
                            }
                        };

                        calls.push(Call3 {
                            target: receiver_addr,
                            allow_failure: false,
                            call_data: sweep_calldata,
                        });

                        let multicall_addr: Address =
                            MULTICALL3_ADDRESS.parse().expect("valid multicall addr");
                        let multicall = Multicall3::new(multicall_addr, evm_client.clone());

                        let mut agg = multicall.aggregate_3(calls);

                        let (suggested_max_fee, suggested_priority_fee) = evm_client
                            .estimate_eip1559_fees(None)
                            .await
                            .unwrap_or((U256::zero(), U256::zero()));
                        let max_allowed_priority = ethers::utils::parse_units("0.05", "gwei")
                            .expect("failed parsing priority fee ceiling");
                        let priority_fee =
                            std::cmp::min(suggested_priority_fee, max_allowed_priority.into());
                        let max_fee_cap = ethers::utils::parse_units("0.1", "gwei")
                            .expect("failed parsing max fee cap");
                        let max_fee = std::cmp::min(suggested_max_fee, max_fee_cap.into());

                        if let Some(eip1559_req) = agg.tx.as_eip1559_mut() {
                            eip1559_req.max_priority_fee_per_gas = Some(priority_fee);
                            eip1559_req.max_fee_per_gas = Some(max_fee);
                        }

                        match agg.send().await {
                            Ok(pending) => match pending.await {
                                Ok(Some(receipt)) => {
                                    let tx_hash = format!("{:#x}", receipt.transaction_hash);
                                    println!(
                                        "Atomic register+ sweep executed for {} -> {}",
                                        receiver_addr, tx_hash
                                    );

                                    // Build deposit payload and deliver webhook if configured
                                    if let Some(url) = &task.webhook_url {
                                        let deposit = beanie_keeper::config::Deposit {
                                            tx_hash: tx_hash.clone(),
                                            from_address: task.from_address.clone(),
                                            receiver: format!("{:?}", receiver_addr),
                                            amount_raw: task.amount_raw.clone(),
                                            block_number: receipt
                                                .block_number
                                                .map(|b| b.as_u64())
                                                .unwrap_or(0),
                                        };

                                        let keeper_cfg =
                                            beanie_keeper::config::Config::Evm((*evm_cfg).clone());

                                        let job = crate::models::WebhookJob {
                                            cfg: keeper_cfg,
                                            webhook_url: url.clone(),
                                            deposit,
                                            sweep_tx: Some(tx_hash.clone()),
                                            max_retries: 5,
                                        };

                                        if let Err(e) = webhook_tx.send(job).await {
                                            eprintln!("failed enqueuing webhook job: {e}");
                                        }
                                    }
                                }
                                Ok(None) => {
                                    eprintln!("Atomic multicall dropped for {}", receiver_addr)
                                }
                                Err(e) => eprintln!("Atomic multicall failed: {}", e),
                            },
                            Err(e) => eprintln!("Failed sending atomic multicall: {}", e),
                        }
                    }
                    Err(e) => eprintln!("Invalid EVM receiver address in payment task: {}", e),
                }
            }
            crate::models::Chain::Starknet => {
                // Starknet flow — bundle register_merchant (if missing) + sweep in one execute_v3
                match Felt::from_hex(&task.receiver_address) {
                    Ok(_receiver_felt) => {
                        // Compute merchant felt for predict call
                        let merchant_felt =
                            Felt::from_hex(&task.merchant_address).unwrap_or_else(|_| {
                                derive_felt_from_foreign_address(&task.merchant_address)
                            });

                        // Predict receiver address using factory view
                        let predict_selector =
                            match get_selector_from_name("predict_receiver_address") {
                                Ok(s) => s,
                                Err(e) => {
                                    eprintln!(
                                        "Failed to get selector for predict_receiver_address: {}",
                                        e
                                    );
                                    continue;
                                }
                            };

                        let predict_call = FunctionCall {
                            contract_address: starknet_factory_addr,
                            entry_point_selector: predict_selector,
                            calldata: vec![merchant_felt],
                        };

                        let predict_res = match starknet_account
                            .provider()
                            .call(predict_call, BlockId::Tag(BlockTag::Latest))
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("Failed predicting receiver address: {}", e);
                                continue;
                            }
                        };

                        let predicted_receiver = predict_res.first().cloned().unwrap_or(Felt::ZERO);

                        let mut calls = Vec::new();

                        if task.create_if_missing {
                            let register_selector =
                                match get_selector_from_name("register_merchant") {
                                    Ok(s) => s,
                                    Err(e) => {
                                        eprintln!(
                                            "Failed to get selector for register_merchant: {}",
                                            e
                                        );
                                        continue;
                                    }
                                };

                            // Determine CCTP params based on requested target_chain/destination_address
                            let (cctp_mint_chain_felt, cctp_recipient_low, cctp_recipient_high) =
                                if task.destination_chain == task.source_chain {
                                    (Felt::ZERO, Felt::ZERO, Felt::ZERO)
                                } else {
                                    match task.destination_chain {
                                        crate::models::Chain::Starknet => {
                                            match Felt::from_hex(&task.merchant_address) {
                                                Ok(dest_f) => {
                                                    let be = dest_f.to_bytes_be();
                                                    let high =
                                                        Felt::from_bytes_be_slice(&be[0..16]);
                                                    let low =
                                                        Felt::from_bytes_be_slice(&be[16..32]);
                                                    (
                                                        chain_to_felt(task.destination_chain),
                                                        low,
                                                        high,
                                                    )
                                                }
                                                Err(e) => {
                                                    eprintln!(
                                                        "Invalid Starknet destination_address: {}",
                                                        e
                                                    );
                                                    continue;
                                                }
                                            }
                                        }
                                        _ => match task.merchant_address.parse::<Address>() {
                                            Ok(addr) => {
                                                let mut buf = [0u8; 32];
                                                buf[12..].copy_from_slice(addr.as_bytes());
                                                let high = Felt::from_bytes_be_slice(&buf[0..16]);
                                                let low = Felt::from_bytes_be_slice(&buf[16..32]);
                                                (chain_to_felt(task.destination_chain), low, high)
                                            }
                                            Err(e) => {
                                                eprintln!("Invalid EVM destination_address: {}", e);
                                                continue;
                                            }
                                        },
                                    }
                                };

                            let register_call = Call {
                                to: starknet_factory_addr,
                                selector: register_selector,
                                calldata: vec![
                                    merchant_felt,
                                    cctp_mint_chain_felt,
                                    cctp_recipient_low,
                                    cctp_recipient_high,
                                ],
                            };

                            calls.push(register_call);
                        }

                        // sweep call to predicted receiver
                        let sweep_selector = match get_selector_from_name("sweep") {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("Failed to get selector for sweep: {}", e);
                                continue;
                            }
                        };

                        let sweep_call = Call {
                            to: predicted_receiver,
                            selector: sweep_selector,
                            calldata: vec![],
                        };

                        calls.push(sweep_call);

                        match starknet_account.execute_v3(calls).send().await {
                            Ok(pending) => {
                                let tx_hash_felt = pending.transaction_hash;
                                let tx_hash = format!("{:#x}", tx_hash_felt);
                                println!("Starknet atomic register+swap invoked tx {}", tx_hash);

                                if let Some(url) = &task.webhook_url {
                                    let deposit = beanie_keeper::config::Deposit {
                                        tx_hash: tx_hash.clone(),
                                        from_address: task.from_address.clone(),
                                        receiver: format!("{:#x}", predicted_receiver),
                                        amount_raw: task.amount_raw.clone(),
                                        block_number: 0,
                                    };

                                    let keeper_cfg = beanie_keeper::config::Config::Starknet(
                                        (*starknet_cfg).clone(),
                                    );

                                    let job = crate::models::WebhookJob {
                                        cfg: keeper_cfg,
                                        webhook_url: url.clone(),
                                        deposit,
                                        sweep_tx: Some(tx_hash.clone()),
                                        max_retries: 5,
                                    };

                                    if let Err(e) = webhook_tx.send(job).await {
                                        eprintln!("failed enqueuing webhook job: {e}");
                                    }
                                }
                            }
                            Err(e) => eprintln!("Starknet atomic invoke failed: {}", e),
                        }
                    }
                    Err(e) => eprintln!("Invalid Starknet receiver felt: {}", e),
                }
            }
            _ => {
                eprintln!(
                    "Unsupported chain for payment task: {:?}",
                    task.source_chain
                );
            }
        }
    }
}

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use beanie_keeper::{
    config::EvmConfig,
    solana_indexer::{ReceiverStatus, SolanaReceiverRecord},
};
use log::info;
use solana_sdk::{
    message::Message as SolanaMessage,
    program_pack::Pack as SolanaPack,
    pubkey::Pubkey as SolanaPubkey,
    signature::{Keypair as SolanaKeypair, Signature as SolanaSignature},
    transaction::Transaction as SolanaTransaction,
};
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
use spl_associated_token_account::get_associated_token_address;

pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, StarknetWallet>;

use crate::models::{Chain, ChainXReceiverLocal, ReceiverFactory};
use crate::models::{chain_to_bytes32, chain_to_felt, derive_felt_from_foreign_address};

use std::str::FromStr;
abigen!(
    Erc3009Usdc,
    r#"[
        "function transferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce,uint8 v,bytes32 r,bytes32 s)"
    ]"#
);

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
pub async fn run_payment_worker(
    evm_targets: std::collections::HashMap<
        Chain,
        (Arc<beanie_keeper::evm_keeper::SignerProvider>, EvmConfig),
    >,
    starknet_account: Arc<StarknetAccount>,
    // evm_cfg: Arc<beanie_keeper::config::EvmConfig>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    solana_rpc: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    solana_keeper: Arc<SolanaKeypair>,
    solana_cfg: Arc<beanie_keeper::config::SolanaConfig>,
    mut rx: mpsc::Receiver<crate::models::PaymentTask>,
    webhook_tx: Arc<mpsc::Sender<crate::models::WebhookJob>>,
) {
    info!("Payment worker starting");

    let starknet_factory_addr = starknet_cfg.factory_address;

    // Local abigen is declared at top-level

    while let Some(mut task) = rx.recv().await {
        task.attempts += 1;

        match task.source_chain {
            Chain::Base | Chain::Ethereum | Chain::Arbitrum | Chain::Monad => {
                // EVM flow
                match task.receiver_address.parse::<Address>() {
                    Ok(receiver_addr) => {
                        let Some((chain_client, evm_cfg)) =
                            evm_targets.get(&task.source_chain).cloned()
                        else {
                            eprintln!(
                                "announce worker: no client/factory configured for {:?}",
                                task.source_chain
                            );
                            continue;
                        };

                        let evm_factory_addr = evm_cfg.factory_address;

                        // Check whether contract exists by code size
                        let code = chain_client.provider().get_code(receiver_addr, None).await;

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
                                ReceiverFactory::new(evm_factory_addr, chain_client.clone())
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

                        let auth = match &task.evm_auth {
                            Some(a) => a,
                            None => {
                                eprintln!(
                                    "EVM payment task missing verified authorization, dropping"
                                );
                                continue;
                            }
                        };
                        let sig = match ethers::types::Signature::from_str(
                            auth.signature.trim_start_matches("0x"),
                        ) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("bad signature stored on task: {e}");
                                continue;
                            }
                        };
                        let value = match U256::from_dec_str(&task.amount_raw) {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("bad amount on task: {e}");
                                continue;
                            }
                        };
                        let from_addr: Address = match task.from_address.parse() {
                            Ok(a) => a,
                            Err(e) => {
                                eprintln!("bad from_address on task: {e}");
                                continue;
                            }
                        };

                        let usdc_addr: Address = evm_cfg.token_address;
                        let transfer_call = Erc3009Usdc::new(usdc_addr, chain_client.clone())
                            .transfer_with_authorization(
                                from_addr,
                                receiver_addr,
                                value,
                                auth.valid_after.into(),
                                auth.valid_before.into(),
                                auth.nonce.into(),
                                sig.v as u8,
                                sig.r.into(),
                                sig.s.into(),
                            );
                        let transfer_calldata = match transfer_call.calldata() {
                            Some(c) => c,
                            None => {
                                eprintln!("failed to encode transferWithAuthorization");
                                continue;
                            }
                        };
                        calls.push(Call3 {
                            target: usdc_addr,
                            allow_failure: false,
                            call_data: transfer_calldata,
                        });

                        // sweep calldata
                        let receiver_contract =
                            ChainXReceiverLocal::new(receiver_addr, chain_client.clone());
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
                        let multicall = Multicall3::new(multicall_addr, chain_client.clone());

                        let mut agg = multicall.aggregate_3(calls);

                        let (suggested_max_fee, suggested_priority_fee) = chain_client
                            .estimate_eip1559_fees(None)
                            .await
                            .unwrap_or((U256::zero(), U256::zero()));
                        let max_allowed_priority = ethers::utils::parse_units("0.005", "gwei")
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
                                            beanie_keeper::config::Config::Evm((evm_cfg).clone());

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
                match Felt::from_hex(&task.receiver_address) {
                    Ok(_receiver_felt) => {
                        let merchant_felt =
                            Felt::from_hex(&task.merchant_address).unwrap_or_else(|_| {
                                derive_felt_from_foreign_address(&task.merchant_address)
                            });

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

                        let auth = match &task.starknet_auth {
                            Some(a) => a,
                            None => {
                                eprintln!(
                                    "Starknet payment task missing verified authorization, dropping"
                                );
                                continue;
                            }
                        };

                        let user_account_felt = match Felt::from_hex(&auth.user_address) {
                            Ok(f) => f,
                            Err(e) => {
                                eprintln!("bad user address on task: {e}");
                                continue;
                            }
                        };

                        let execute_from_outside_selector =
                            match get_selector_from_name("execute_from_outside_v2") {
                                Ok(s) => s,
                                Err(e) => {
                                    eprintln!("selector lookup failed: {e}");
                                    continue;
                                }
                            };

                        // Serialize OutsideExecution + signature per Cairo Serde: struct fields flattened,
                        // Span<Call>/Array<felt252> length-prefixed.
                        let mut oe_calldata = vec![
                            Felt::from_hex(&auth.outside_execution.caller).unwrap(),
                            Felt::from_hex(&auth.outside_execution.nonce).unwrap(),
                            Felt::from(auth.outside_execution.execute_after),
                            Felt::from(auth.outside_execution.execute_before),
                            Felt::from(auth.outside_execution.calls.len() as u64),
                        ];
                        for c in &auth.outside_execution.calls {
                            oe_calldata.push(Felt::from_hex(&c.contract_address).unwrap());
                            oe_calldata.push(get_selector_from_name(&c.entrypoint).unwrap());
                            oe_calldata.push(Felt::from(c.calldata.len() as u64));
                            for cd in &c.calldata {
                                oe_calldata.push(Felt::from_hex(cd).unwrap());
                            }
                        }
                        oe_calldata.push(Felt::from(auth.signature.len() as u64));
                        for s in &auth.signature {
                            oe_calldata.push(Felt::from_hex(s).unwrap());
                        }

                        calls.push(Call {
                            to: user_account_felt,
                            selector: execute_from_outside_selector,
                            calldata: oe_calldata,
                        });

                        // sweep call to predicted receiver — now sweeping real funds that actually arrived
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
                                println!("Starknet sweep invoked tx {}", tx_hash);

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
                            Err(e) => eprintln!("Starknet sweep failed: {}", e),
                        }
                    }
                    Err(e) => eprintln!("Invalid Starknet receiver felt: {}", e),
                }
            }
            crate::models::Chain::Solana => {
                // `receiver_address` is always an already-live USDC ATA by
                // the time a payment can reference it (created up front in
                // `prepare_registration`, see create_workers.rs's Solana
                // arm) — but the receiver's on-chain *registration*
                // (`receiver_config`) may still be pending. Unlike
                // EVM/Starknet, that register step can't be bundled into
                // the same transaction as the gasless transfer: `reg_tx` is
                // a separate, already-fully-signed `Transaction` pinned at
                // announce time (see
                // `solana_keeper::broadcast_pending_registration`'s doc
                // comment) and can't be merged into the payer's message
                // after the fact. So this arm submits the transfer first,
                // then — same-chain destinations only — JIT-registers (if
                // needed) and sweeps inline right after, via
                // `sweep_after_solana_payment` below, instead of waiting
                // for the next indexer/reconciliation pass. Cross-chain
                // (CCTP) destinations still fall back to that existing
                // poller path, since `multicall_sweep_same_chain` doesn't
                // cover CCTP sweeps yet.
                let auth = match &task.solana_auth {
                    Some(a) => a,
                    None => {
                        eprintln!("Solana payment task missing verified authorization, dropping");
                        continue;
                    }
                };

                let message_bytes = match BASE64.decode(&auth.message) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("bad solana message encoding on task: {e}");
                        continue;
                    }
                };
                let message: SolanaMessage = match bincode::deserialize(&message_bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("bad solana message on task: {e}");
                        continue;
                    }
                };
                let owner_pk: SolanaPubkey = match auth.owner.parse() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("bad solana owner pubkey on task: {e}");
                        continue;
                    }
                };
                let owner_sig_bytes = match BASE64.decode(&auth.signature) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("bad solana signature encoding on task: {e}");
                        continue;
                    }
                };
                let owner_sig = match SolanaSignature::try_from(owner_sig_bytes.as_slice()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("bad solana signature bytes on task: {e}");
                        continue;
                    }
                };

                // Fee payer (Beanie's keeper) is always account index 0;
                // the payer's own signature slot is wherever their pubkey
                // landed among the required signers.
                let num_sigs = message.header.num_required_signatures as usize;
                let Some(owner_idx) = message.account_keys.iter().position(|k| *k == owner_pk)
                else {
                    eprintln!("solana payment task: owner not present in message account keys");
                    continue;
                };
                if owner_idx >= num_sigs {
                    eprintln!("solana payment task: owner is not a required signer");
                    continue;
                }

                let mut signatures = vec![SolanaSignature::default(); num_sigs];
                signatures[owner_idx] = owner_sig;
                let mut tx = SolanaTransaction {
                    signatures,
                    message,
                };

                // Same blockhash the payer already signed over, so this
                // only fills the keeper's own signer slot(s) rather than
                // wiping and re-signing everything.
                let recent_blockhash = tx.message.recent_blockhash;
                if let Err(e) = tx.try_partial_sign(&[solana_keeper.as_ref()], recent_blockhash) {
                    eprintln!("failed keeper co-sign for solana payment: {e}");
                    continue;
                }

                if let Err(e) = tx.verify() {
                    eprintln!("solana payment tx failed signature verification: {e}");
                    continue;
                }

                match solana_rpc.send_and_confirm_transaction(&tx).await {
                    Ok(sig) => {
                        let tx_sig = sig.to_string();
                        println!(
                            "Solana gasless transfer executed {} -> {}",
                            task.receiver_address, tx_sig
                        );

                        // Register (if not already) and sweep right away,
                        // same-chain only — see the doc comment on this
                        // match arm for why register can't be bundled into
                        // the transfer tx itself, and why cross-chain
                        // destinations skip this and fall back to the
                        // poller.
                        let sweep_tx = if task.destination_chain == task.source_chain {
                            match sweep_after_solana_payment(
                                &solana_rpc,
                                &solana_keeper,
                                &solana_cfg,
                                &task,
                            )
                            .await
                            {
                                Ok(tx) => tx,
                                Err(e) => {
                                    eprintln!(
                                        "solana register/sweep after payment failed for {}: {e:#}",
                                        task.receiver_address
                                    );
                                    None
                                }
                            }
                        } else {
                            None
                        };

                        if let Some(url) = &task.webhook_url {
                            let deposit = beanie_keeper::config::Deposit {
                                tx_hash: tx_sig.clone(),
                                from_address: task.from_address.clone(),
                                receiver: task.receiver_address.clone(),
                                amount_raw: task.amount_raw.clone(),
                                block_number: 0,
                            };

                            let keeper_cfg =
                                beanie_keeper::config::Config::Solana((*solana_cfg).clone());

                            let job = crate::models::WebhookJob {
                                cfg: keeper_cfg,
                                webhook_url: url.clone(),
                                deposit,
                                sweep_tx,
                                max_retries: 5,
                            };

                            if let Err(e) = webhook_tx.send(job).await {
                                eprintln!("failed enqueuing webhook job: {e}");
                            }
                        }
                    }
                    Err(e) => eprintln!("Solana gasless transfer failed: {}", e),
                }
            }
        }
    }
}

/// Register-if-needed + same-chain sweep for a receiver that just landed a
/// gasless-transfer payment, run inline right after that transfer confirms
/// instead of waiting for the next indexer/reconciliation pass.
///
/// The receiver's owning pubkey — what `receiver_config` and
/// `pending_registration` are actually keyed on, distinct from its token
/// account — isn't present anywhere in the payment request. It's read
/// straight off the receiver token account itself, which
/// `solana_keeper::prepare_registration` guarantees already exists by the
/// time any payment can reference it.
///
/// Same-chain only (`chain`/`recipient` zeroed, same convention as the EVM
/// arm's `task.destination_chain == task.source_chain` case) — mirrors
/// `multicall_sweep_same_chain`'s restriction. Callers must skip this for
/// cross-chain destinations and let the existing indexer/poller sweep
/// handle those once CCTP sweep support lands.
async fn sweep_after_solana_payment(
    solana_rpc: &Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    solana_keeper: &Arc<SolanaKeypair>,
    solana_cfg: &Arc<beanie_keeper::config::SolanaConfig>,
    task: &crate::models::PaymentTask,
) -> anyhow::Result<Option<String>> {
    use beanie_keeper::solana_keeper::{
        broadcast_pending_registration, derive_pending_registration, derive_receiver_config,
        multicall_sweep_same_chain,
    };

    let receiver_token_account: SolanaPubkey = task
        .receiver_address
        .parse()
        .map_err(|e| anyhow::anyhow!("bad solana receiver_address: {e}"))?;
    let merchant: SolanaPubkey = task
        .merchant_address
        .parse()
        .map_err(|e| anyhow::anyhow!("bad solana merchant_address: {e}"))?;

    let ta_account = solana_rpc
        .get_account(&receiver_token_account)
        .await
        .map_err(|e| anyhow::anyhow!("receiver token account not found on-chain: {e}"))?;
    let receiver_owner = spl_token::state::Account::unpack(&ta_account.data)
        .map_err(|e| anyhow::anyhow!("receiver token account failed to unpack: {e}"))?
        .owner;

    // Same-chain: cctp_mint_chain/recipient are zeroed.
    let chain = [0u8; 32];
    let recipient = [0u8; 32];
    let receiver_config = derive_receiver_config(
        &solana_cfg.program_id,
        &merchant,
        &receiver_owner,
        &chain,
        &recipient,
    );

    if solana_rpc.get_account(&receiver_config).await.is_err() {
        let pending_registration = derive_pending_registration(
            &solana_cfg.program_id,
            &merchant,
            &receiver_owner,
            &chain,
            &recipient,
        );
        let pending_acct = solana_rpc
            .get_account(&pending_registration)
            .await
            .map_err(|e| {
                anyhow::anyhow!("receiver not registered and no pending_registration on-chain: {e}")
            })?;
        // PendingRegistration layout: 8-byte Anchor discriminator + 1-byte
        // bump + 4-byte Vec<u8> length prefix, then reg_tx (same layout
        // solana.rs's box_fetch_reg_tx relies on).
        anyhow::ensure!(
            pending_acct.data.len() > 13,
            "pending_registration account too short"
        );
        let reg_tx = &pending_acct.data[13..];
        let sig = broadcast_pending_registration(solana_rpc, reg_tx).await?;
        println!("Solana JIT register (payment path) {receiver_owner} -> {sig}");
    }

    let merchant_token_account = get_associated_token_address(&merchant, &solana_cfg.mint);
    let rec = SolanaReceiverRecord {
        merchant,
        receiver: receiver_owner,
        receiver_token_account,
        receiver_config,
        status: ReceiverStatus::Registered,
        reg_tx: None,
    };

    multicall_sweep_same_chain(
        solana_rpc,
        &solana_keeper.insecure_clone(),
        solana_cfg,
        &[(rec, merchant_token_account)],
    )
    .await
}

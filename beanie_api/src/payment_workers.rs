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
    accounts::{Account, SingleOwnerAccount},
    core::types::{Call, Felt},
    core::utils::get_selector_from_name,
    providers::jsonrpc::{HttpTransport, JsonRpcClient},
    signers::LocalWallet as StarknetWallet,
};

use std::sync::Arc;
use tokio::sync::mpsc;

use ethers::providers::Middleware;
use ethers::types::Address;
#[allow(unused_imports)]
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Provider as EvmProvider},
    signers::LocalWallet,
    types::U256,
};
use spl_associated_token_account::get_associated_token_address;

pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, StarknetWallet>;

use crate::models::{Chain, ChainXReceiverLocal, ReceiverFactory};
use crate::transfer_workers::evm::EvmReceiverInfo;
use crate::transfer_workers::starknet::StarknetReceiverInfo;
use crate::transfer_workers::{SharedEvmRegistry, SharedSolanaRegistry, SharedStarknetRegistry};

use std::collections::HashMap;
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
pub(crate) async fn run_payment_worker(
    evm_targets: HashMap<Chain, (Arc<beanie_keeper::evm_keeper::SignerProvider>, EvmConfig)>,
    // One shared, read-only registry per chain — populated by
    // transfer_workers' own worker for that chain from
    // ReceiverAnnounced/ReceiverRegistered, never by this worker. JIT
    // register/sweep params below are looked up here, never derived from
    // the payment request itself.
    evm_registries: HashMap<Chain, SharedEvmRegistry>,
    starknet_account: Arc<StarknetAccount>,
    starknet_cfg: Arc<beanie_keeper::config::StarknetConfig>,
    starknet_registry: SharedStarknetRegistry,
    solana_rpc: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    solana_keeper: Arc<SolanaKeypair>,
    solana_cfg: Arc<beanie_keeper::config::SolanaConfig>,
    solana_registry: SharedSolanaRegistry,
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
                            // (merchant, route) come straight from what was
                            // actually announced for THIS receiver address —
                            // never derived from task.destination_chain /
                            // task.merchant_address. If the announce hasn't
                            // been indexed yet, don't guess: drop this
                            // payment for now and let it retry once it has
                            // (task.attempts already tracks retries).
                            let info: Option<EvmReceiverInfo> =
                                match evm_registries.get(&task.source_chain) {
                                    Some(registry) => {
                                        registry.read().await.get(&receiver_addr).copied()
                                    }
                                    None => None,
                                };
                            let Some(info) = info else {
                                eprintln!(
                                    "no announced route yet for evm receiver {receiver_addr:?} on {:?} — skipping this pass",
                                    task.source_chain
                                );
                                continue;
                            };
                            let Some(route) = info.route else {
                                eprintln!(
                                    "evm receiver {receiver_addr:?} known but has no announced route — skipping this pass"
                                );
                                continue;
                            };
                            let merchant_addr = info.merchant;
                            let (cctp_chain_bytes, recipient_bytes) =
                                (route.chain, route.recipient);

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
                    Ok(receiver_felt) => {
                        // (merchant, route) come straight from the
                        // announced record for THIS receiver — same
                        // registry starknet.rs's own transfer worker
                        // maintains, never re-derived from
                        // task.destination_chain / task.merchant_address.
                        // Also replaces the old on-chain
                        // predict_receiver_address call: a registry hit
                        // for receiver_felt already IS a real, previously
                        // announced receiver, a stronger guarantee than a
                        // same-tx prediction would give.
                        let info: Option<StarknetReceiverInfo> =
                            starknet_registry.read().await.get(&receiver_felt).copied();

                        let mut calls = Vec::new();

                        if task.create_if_missing {
                            let Some(info) = info else {
                                eprintln!(
                                    "no announced route yet for starknet receiver {receiver_felt:#x} — skipping this pass"
                                );
                                continue;
                            };
                            let Some(route) = info.route else {
                                eprintln!(
                                    "starknet receiver {receiver_felt:#x} known but has no announced route — skipping this pass"
                                );
                                continue;
                            };
                            let merchant_felt = info.merchant;

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

                            let register_call = Call {
                                to: starknet_factory_addr,
                                selector: register_selector,
                                calldata: vec![
                                    merchant_felt,
                                    route.chain,
                                    route.recipient_low,
                                    route.recipient_high,
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

                        // sweep call to the receiver — now sweeping real funds that actually arrived
                        let sweep_selector = match get_selector_from_name("sweep") {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("Failed to get selector for sweep: {}", e);
                                continue;
                            }
                        };

                        let sweep_call = Call {
                            to: receiver_felt,
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
                                        receiver: format!("{:#x}", receiver_felt),
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
                // then JIT-registers (if needed) and sweeps inline right
                // after via `sweep_after_solana_payment`, driven by the
                // announced record in `solana_registry` (not the payment
                // request). Same-chain and cross-chain (CCTP) routes are
                // both handled there by `solana_keeper::sweep_registered`.
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

                        // Register (if not already) and sweep right away.
                        // What to do — same-chain or CCTP sweep, or skip
                        // because the announce isn't indexed yet — is
                        // decided inside from the announced record in
                        // `solana_registry`, never from the payment
                        // request's own destination_chain.
                        let sweep_tx = match sweep_after_solana_payment(
                            &solana_rpc,
                            &solana_keeper,
                            &solana_cfg,
                            &solana_registry,
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

/// Register-if-needed + sweep for a receiver that just landed a
/// gasless-transfer payment, run inline right after that transfer confirms
/// instead of waiting for the next indexer/reconciliation pass.
///
/// Everything about the receiver (merchant, route, pending `reg_tx`) comes
/// from `solana_registry` — the same map solana.rs's own worker builds from
/// validated `ReceiverAnnounced`/`ReceiverRegistered` events. Nothing is
/// derived from the payment request, and the pending `reg_tx` is only ever
/// broadcast after `attach_reg_tx_and_merge`'s `validate_announce` has
/// already accepted it (the old path re-fetched it from chain unchecked).
///
/// Outcomes:
///   - not in registry yet          -> skip (Ok(None)), poller catches it
///   - Announced                    -> broadcast validated reg_tx, then sweep
///   - Registered                   -> sweep
///
/// The sweep itself is chosen per receiver inside
/// `solana_keeper::sweep_registered`: zero route -> same-chain, non-zero ->
/// CCTP, route unknown -> read from the on-chain `receiver_config`.
///
/// The receiver's owning pubkey (the registry key) isn't in the payment
/// request; it's read off the receiver token account, which
/// `solana_keeper::prepare_registration` guarantees already exists.
async fn sweep_after_solana_payment(
    solana_rpc: &Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    solana_keeper: &Arc<SolanaKeypair>,
    solana_cfg: &Arc<beanie_keeper::config::SolanaConfig>,
    solana_registry: &SharedSolanaRegistry,
    task: &crate::models::PaymentTask,
) -> anyhow::Result<Option<String>> {
    use beanie_keeper::solana_keeper::{broadcast_pending_registration, sweep_registered};

    let receiver_token_account: SolanaPubkey = task
        .receiver_address
        .parse()
        .map_err(|e| anyhow::anyhow!("bad solana receiver_address: {e}"))?;

    let ta_account = solana_rpc
        .get_account(&receiver_token_account)
        .await
        .map_err(|e| anyhow::anyhow!("receiver token account not found on-chain: {e}"))?;
    let receiver_owner = spl_token::state::Account::unpack(&ta_account.data)
        .map_err(|e| anyhow::anyhow!("receiver token account failed to unpack: {e}"))?
        .owner;

    let rec: Option<SolanaReceiverRecord> =
        solana_registry.read().await.get(&receiver_owner).cloned();
    let Some(rec) = rec else {
        println!(
            "no announced record yet for solana receiver {receiver_owner} — skipping inline sweep"
        );
        return Ok(None);
    };

    if rec.status == ReceiverStatus::Announced {
        let reg_tx = rec
            .reg_tx
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("receiver Announced but registry holds no reg_tx"))?;
        match broadcast_pending_registration(solana_rpc, reg_tx).await {
            Ok(sig) => println!("Solana JIT register (payment path) {receiver_owner} -> {sig}"),
            Err(e) => {
                // Registry snapshot may be stale (poller registered it
                // moments ago). Fine if receiver_config now exists.
                if solana_rpc.get_account(&rec.receiver_config).await.is_err() {
                    return Err(e);
                }
            }
        }
    }

    let merchant_token_account = get_associated_token_address(&rec.merchant, &solana_cfg.mint);
    Ok(sweep_registered(
        solana_rpc,
        solana_keeper,
        solana_cfg,
        &[(rec, merchant_token_account)],
    )
    .await)
}

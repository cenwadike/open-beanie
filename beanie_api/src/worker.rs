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
use tokio::time::{Duration, sleep};

use ethers::core::types::transaction::eip2718::TypedTransaction;
use ethers::types::transaction::eip1559::Eip1559TransactionRequest;
#[allow(unused_imports)]
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Provider as EvmProvider},
    signers::LocalWallet,
    types::U256,
};
use ethers::{providers::Middleware, utils::format_bytes32_string};
use ethers::{types::Address, utils::keccak256};

use crate::models::{Chain, DeployTask};

pub type EvmSignerProvider =
    SignerMiddleware<NonceManagerMiddleware<EvmProvider<EvmHttp>>, LocalWallet>;
pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, StarknetWallet>;

abigen!(
    MerchantFactory,
    r#"[
        function registerMerchant(address merchant, bytes32 cctpMintChain, bytes32 cctpMintRecipient) external returns (address)
        function getReceiverCount(address merchant) external view returns (uint256)
    ]"#;
    // Chain-agnostic — the ONE webhook registry across all of Beanie, not just EVM.
    // No factory-existence gate anymore: an EVM contract can't read Starknet state,
    // so any such gate could only ever cover EVM registrations. See
    // MerchantWebhookRegistry.sol for the full reasoning.
    MerchantWebhookRegistry,
    r#"[
        function setWebhookUrl(address merchant, string calldata url) external
    ]"#;
);

#[derive(Debug)]
pub enum DeployError {
    Fatal(String),
    Transient(String),
}

// Mirrors MAX_RECEIVERS_PER_MERCHANT — now literally the same constant name
// in both the Solidity and Cairo factories, keep in sync if either changes.
const MAX_RECEIVERS_PER_MERCHANT: u64 = 32;

fn chain_to_bytes32(chain: Chain) -> [u8; 32] {
    let name = match chain {
        Chain::Base => "BASE",
        Chain::Ethereum => "ETHEREUM",
        Chain::Starknet => "STARKNET",
        Chain::Solana => "SOLANA",
    };

    format_bytes32_string(name).expect("Chain name fits in bytes32")
}

fn chain_to_felt(chain: Chain) -> Felt {
    match chain {
        Chain::Base => Felt::from_hex(&hex::encode("BASE")).unwrap(),
        Chain::Ethereum => Felt::from_hex(&hex::encode("ETHEREUM")).unwrap(),
        Chain::Solana => Felt::from_hex(&hex::encode("SOLANA")).unwrap(),
        Chain::Starknet => Felt::from_hex(&hex::encode("STARKNET")).unwrap(),
    }
}

/// Consistent, chain-agnostic derivation for a NON-target leg's identity.
pub fn derive_felt_from_foreign_address(addr: &str) -> Felt {
    let hash = keccak256(addr.as_bytes());
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(&hash[12..32]);
    Felt::from_bytes_be(&buf)
}

/// Registers a webhook URL for `merchant` in the single, chain-agnostic
/// MerchantWebhookRegistry. Called from every chain's registration arm —
/// Starknet included — always through `evm_client`, since the registry
/// itself only ever lives on one EVM chain. A Starknet-only merchant still
/// needs an EVM-address-shaped identity for this call, same system-wide
/// assumption the rest of Beanie already makes about merchant_address
/// being reused verbatim across chains.
async fn register_webhook(
    evm_client: Arc<EvmSignerProvider>,
    webhook_registry_addr: Address,
    merchant_addr: Address,
    url: &str,
) -> Result<(), DeployError> {
    let registry = MerchantWebhookRegistry::new(webhook_registry_addr, evm_client);
    let tx = registry.set_webhook_url(merchant_addr, url.to_string());
    let pending_tx = tx
        .send()
        .await
        .map_err(|e| DeployError::Transient(format!("Webhook registration send failed: {e}")))?;
    pending_tx.await.map_err(|e| {
        DeployError::Transient(format!("Webhook registration confirmation failed: {e}"))
    })?;
    Ok(())
}

pub async fn execute_onchain_deployment(
    evm_client: Arc<EvmSignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    evm_factory_addr: Address,
    starknet_factory_addr: Felt,
    webhook_registry_addr: Option<Address>,
    task: &DeployTask,
) -> Result<(), DeployError> {
    match task.chain {
        Chain::Base | Chain::Ethereum => {
            let is_target = task.chain == task.target_chain;

            let merchant_addr: Address = if is_target {
                task.merchant_address.parse().map_err(|_| {
                    DeployError::Fatal(
                        "Settlement (target) chain address must be a valid EVM address".into(),
                    )
                })?
            } else {
                task.merchant_address.parse().unwrap_or_else(|_| {
                    let hash = keccak256(task.merchant_address.as_bytes());
                    Address::from_slice(&hash[12..32])
                })
            };

            let factory = MerchantFactory::new(evm_factory_addr, evm_client.clone());

            // MAX_RECEIVERS_PER_MERCHANT receivers (one per target chain, or
            // even repeats), so this isn't a "skip if already registered"
            // check — it's a cap check. Fail fast (fatal, not transient)
            // rather than sending a tx the contract will just revert with
            // MaximumReceiversExceeded.
            let existing_count: U256 = factory
                .get_receiver_count(merchant_addr)
                .call()
                .await
                .map_err(|e| {
                    DeployError::Transient(format!("Failed RPC call to getReceiverCount: {e}"))
                })?;

            if existing_count >= U256::from(MAX_RECEIVERS_PER_MERCHANT) {
                return Err(DeployError::Fatal(format!(
                    "Merchant {merchant_addr:?} already has {existing_count} receivers (max {MAX_RECEIVERS_PER_MERCHANT})"
                )));
            }

            // Same-chain (this instance's chain IS the target_chain): leave
            // the recipient zeroed so ChainXReceiver::sweep() takes the
            // local safeTransfer(merchant, net) branch instead of trying to
            // CCTP-burn back onto the chain it's already on.
            //
            // Cross-chain: recipient is the merchant's own address, encoded
            // as a proper CCTP bytes32 (12 zero bytes + 20 address bytes).
            let is_same_chain = task.chain == task.target_chain;

            let (cctp_chain_bytes, recipient_bytes) = if is_same_chain {
                ([0u8; 32], [0u8; 32])
            } else {
                let mut recipient_bytes = [0u8; 32];
                if task.target_chain == Chain::Starknet {
                    let felt =
                        Felt::from_hex(&task.merchant_address).expect("Invalid Starknet address");
                    recipient_bytes.copy_from_slice(&felt.to_bytes_be());
                } else {
                    let addr: Address = task.merchant_address.parse().expect("Invalid EVM address");
                    recipient_bytes[12..].copy_from_slice(addr.as_bytes());
                }
                (chain_to_bytes32(task.target_chain), recipient_bytes)
            };

            // 1. Query the provider's suggested EIP-1559 fees (max_fee, max_priority_fee)
            let (suggested_max_fee, suggested_priority_fee) = evm_client
                .estimate_eip1559_fees(None)
                .await
                .map_err(|e| DeployError::Transient(format!("Failed to estimate fees: {e}")))?;

            // 2. Define your maximum acceptable priority fee ceiling (e.g., 0.05 Gwei)
            let max_allowed_priority = ethers::utils::parse_units("0.05", "gwei").map_err(|e| {
                DeployError::Fatal(format!("Failed to parse priority fee ceiling: {e}"))
            })?;

            // 3. Take the smaller value between the node suggestion and your ceiling
            let priority_fee = std::cmp::min(suggested_priority_fee, max_allowed_priority.into());

            // 4. Cap max_fee_per_gas to prevent RPC inflated fee estimations (e.g., 0.1 Gwei)
            let max_fee_cap = ethers::utils::parse_units("0.1", "gwei")
                .map_err(|e| DeployError::Fatal(format!("Failed to parse max fee cap: {e}")))?;
            let max_fee = std::cmp::min(suggested_max_fee, max_fee_cap.into());

            // 5. Build the call once
            let mut tx =
                factory.register_merchant(merchant_addr, cctp_chain_bytes, recipient_bytes);

            // 6. Force it to the Eip1559 variant and set both fee fields explicitly
            if let Some(eip1559_req) = tx.tx.as_eip1559_mut() {
                eip1559_req.max_priority_fee_per_gas = Some(priority_fee);
                eip1559_req.max_fee_per_gas = Some(max_fee);
            } else {
                let legacy = tx.tx.clone();
                tx.tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
                    from: legacy.from().copied(),
                    to: legacy.to().cloned(),
                    gas: legacy.gas().copied(),
                    value: legacy.value().copied(),
                    data: legacy.data().cloned(),
                    nonce: legacy.nonce().copied(),
                    access_list: Default::default(),
                    max_priority_fee_per_gas: Some(priority_fee),
                    max_fee_per_gas: Some(max_fee),
                    chain_id: legacy.chain_id(),
                });
            }

            let pending_tx = tx
                .send()
                .await
                .map_err(|e| DeployError::Transient(format!("EVM transaction send failed: {e}")))?;

            let receipt = pending_tx
                .await
                .map_err(|e| DeployError::Transient(format!("EVM confirmation failed: {e}")))?;

            let tx_hash = receipt
                .map(|r| format!("{:#x}", r.transaction_hash))
                .unwrap_or_default();

            println!("Base receiver deployed in tx {}", tx_hash);

            if let (Some(url), Some(registry_addr)) = (&task.webhook_url, webhook_registry_addr) {
                if !url.is_empty() {
                    register_webhook(evm_client.clone(), registry_addr, merchant_addr, url).await?;
                }
            }

            Ok(())
        }
        Chain::Starknet => {
            // StarknetReceiver is symmetric with ChainXReceiver now — no
            // pool coupling, no privacy_invoke, no restriction on Starknet
            // being its own target. Privacy is a payer-side choice (an
            // unshielding transfer vs. a plain one, both landing as an
            // ordinary balance this contract sweeps identically), not a
            // property of which chain got registered.
            let is_target = task.chain == task.target_chain;

            let merchant_felt = if is_target {
                Felt::from_hex(&task.merchant_address).map_err(|e| {
                    DeployError::Fatal(format!(
                        "Settlement (target) chain address must be a valid Starknet felt252: {e}"
                    ))
                })?
            } else {
                derive_felt_from_foreign_address(&task.merchant_address)
            };

            // Cap check — mirrors the EVM arm's getReceiverCount() exactly
            // now that the Cairo factory exposes the same view.
            let check_call = FunctionCall {
                contract_address: starknet_factory_addr,
                entry_point_selector: get_selector_from_name("get_receiver_count").unwrap(),
                calldata: vec![merchant_felt],
            };

            let count_res = starknet_account
                .provider()
                .call(check_call, BlockId::Tag(BlockTag::Latest))
                .await
                .map_err(|e| DeployError::Transient(format!("Starknet view call failed: {e}")))?;

            let existing_count: u64 = count_res
                .first()
                .cloned()
                .unwrap_or(Felt::ZERO)
                .try_into()
                .unwrap_or(u64::MAX);

            if existing_count >= MAX_RECEIVERS_PER_MERCHANT {
                return Err(DeployError::Fatal(format!(
                    "Merchant {merchant_felt:#x} already has {existing_count} receivers (max {MAX_RECEIVERS_PER_MERCHANT})"
                )));
            }

            // Same-chain (Starknet targeting Starknet is now valid, same as
            // any EVM leg): zero chain + zero recipient.
            // Cross-chain: recipient is the merchant's address zero-padded
            // into a 32-byte word (same shape as evm_address_to_bytes32)
            // and split into Cairo's u256 (low, high) felt pair.
            let is_same_chain = task.chain == task.target_chain;

            let (cctp_mint_chain_felt, cctp_recipient_low, cctp_recipient_high) = if is_same_chain {
                (Felt::ZERO, Felt::ZERO, Felt::ZERO)
            } else {
                let target_chain_felt = chain_to_felt(task.target_chain);
                let recipient_be = merchant_felt.to_bytes_be();
                let high = Felt::from_bytes_be_slice(&recipient_be[0..16]);
                let low = Felt::from_bytes_be_slice(&recipient_be[16..32]);
                (target_chain_felt, low, high)
            };

            let register_call = Call {
                to: starknet_factory_addr,
                selector: get_selector_from_name("register_merchant").unwrap(),
                calldata: vec![
                    merchant_felt,
                    cctp_mint_chain_felt,
                    cctp_recipient_low,
                    cctp_recipient_high,
                ],
            };

            let invoke_result = starknet_account
                .execute_v3(vec![register_call])
                .send()
                .await
                .map_err(|e| DeployError::Transient(format!("Starknet invoke failed: {e}")))?;

            let tx_hash = invoke_result.transaction_hash;
            println!("Starknet receiver deployed in tx 0x{:x}", tx_hash);

            if let (Some(url), Some(registry_addr)) = (&task.webhook_url, webhook_registry_addr) {
                if !url.is_empty() {
                    let merchant_addr: Address =
                        task.merchant_address.parse().unwrap_or_else(|_| {
                            //  Universal Fallback: Hash the arbitrary string to get a 32-byte digest
                            let hash = keccak256(task.merchant_address.as_bytes());

                            // Construct a valid 20-byte EVM Address from the last 20 bytes of the hash
                            Address::from_slice(&hash[12..32])
                        });
                    register_webhook(evm_client.clone(), registry_addr, merchant_addr, url).await?;
                }
            }

            Ok(())
        }
        Chain::Solana => {
            // Anchor instruction for Solana PDA initialization
            Ok(())
        }
    }
}

pub async fn run_deployment_worker(
    evm_client: Arc<EvmSignerProvider>,
    starknet_account: Arc<StarknetAccount>,
    evm_factory_addr: Address,
    starknet_factory_addr: Felt,
    webhook_registry_addr: Option<Address>,
    mut rx: mpsc::Receiver<DeployTask>,
    tx: Arc<mpsc::Sender<DeployTask>>,
) {
    println!("Deployment worker active for EVM & Starknet...");

    while let Some(mut task) = rx.recv().await {
        task.attempts += 1;

        match execute_onchain_deployment(
            evm_client.clone(),
            starknet_account.clone(),
            evm_factory_addr,
            starknet_factory_addr,
            webhook_registry_addr,
            &task,
        )
        .await
        {
            Ok(_) => println!(
                "Successfully initialized {:?} lane for {}",
                task.chain, task.merchant_address
            ),
            Err(DeployError::Fatal(reason)) => eprintln!(
                "Fatal deployment error on {:?} for {}: {}. Dropping task.",
                task.chain, task.merchant_address, reason
            ),
            Err(DeployError::Transient(reason)) => {
                if task.attempts < 3 {
                    eprintln!(
                        "Transient failure on {:?} (attempt {}/3): {}. Retrying in 2s...",
                        task.chain, task.attempts, reason
                    );

                    sleep(Duration::from_secs(2)).await;

                    match execute_onchain_deployment(
                        evm_client.clone(),
                        starknet_account.clone(),
                        evm_factory_addr,
                        starknet_factory_addr,
                        webhook_registry_addr,
                        &task,
                    )
                    .await
                    {
                        Ok(_) => {
                            println!(
                                "Successfully initialized {:?} lane for {}",
                                task.chain, task.merchant_address
                            )
                        }
                        Err(_) => {
                            task.attempts += 1;
                        }
                    }
                } else {
                    eprintln!(
                        "Exhausted retries on {:?} for {}: {}. Requeue.",
                        task.chain, task.merchant_address, reason
                    );
                    let _ = tx.send(task).await;
                }
            }
        }
    }
}

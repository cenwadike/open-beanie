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

#[allow(unused_imports)]
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Provider as EvmProvider},
    signers::LocalWallet,
    types::Address,
};

use crate::models::{Chain, DeployTask};

pub type EvmSignerProvider =
    SignerMiddleware<NonceManagerMiddleware<EvmProvider<EvmHttp>>, LocalWallet>;
pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, StarknetWallet>;

abigen!(
    MerchantFactory,
    r#"[
        function registerMerchant(address merchant, bytes32 cctpMintChain, bytes32 cctpMintRecipient) external returns (address)
        function getReceiver(address merchant) external view returns (address)
    ]"#;
    MerchantWebhookRegistry,
    r#"[
        function setWebhookUrl(string calldata url) external
    ]"#;
);

#[derive(Debug)]
pub enum DeployError {
    Fatal(String),
    Transient(String),
}

fn chain_to_bytes32(chain: Chain) -> [u8; 32] {
    let name = match chain {
        Chain::Base => "BASE",
        Chain::Ethereum => "ETHEREUM",
        Chain::Starknet => "STARKNET",
        Chain::Solana => "SOLANA",
    };
    let mut bytes = [0u8; 32];
    bytes[..name.len()].copy_from_slice(name.as_bytes());
    bytes
}

fn chain_to_felt(chain: Chain) -> Felt {
    match chain {
        Chain::Base => Felt::from_hex("0x42415345").unwrap(), // "BASE"
        Chain::Ethereum => Felt::from_hex("0x455448455245554d").unwrap(), // "ETHEREUM"
        Chain::Solana => Felt::from_hex("0x534f4c414e41").unwrap(), // "SOLANA"
        Chain::Starknet => Felt::from_hex("0x535441524b4e4554").unwrap(),
    }
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
            let merchant_addr: Address = task.merchant_address.parse().map_err(|e| {
                DeployError::Fatal(format!("Invalid EVM merchant_address string: {e}"))
            })?;

            let factory = MerchantFactory::new(evm_factory_addr, evm_client.clone());

            let existing_receiver =
                factory
                    .get_receiver(merchant_addr)
                    .call()
                    .await
                    .map_err(|e| {
                        DeployError::Transient(format!("Failed RPC call to get_receiver: {e}"))
                    })?;

            if existing_receiver == Address::zero() {
                let mut recipient_bytes = [0u8; 32];
                recipient_bytes[12..].copy_from_slice(merchant_addr.as_bytes());

                let tx = factory.register_merchant(
                    merchant_addr,
                    chain_to_bytes32(task.target_chain),
                    recipient_bytes,
                );

                let pending_tx = tx.send().await.map_err(|e| {
                    DeployError::Transient(format!("EVM transaction send failed: {e}"))
                })?;

                pending_tx
                    .await
                    .map_err(|e| DeployError::Transient(format!("EVM confirmation failed: {e}")))?;
            }

            if let (Some(url), Some(registry_addr)) = (&task.webhook_url, webhook_registry_addr) {
                if !url.is_empty() {
                    let registry = MerchantWebhookRegistry::new(registry_addr, evm_client.clone());
                    let tx = registry.set_webhook_url(url.clone());
                    let pending_tx = tx.send().await.map_err(|e| {
                        DeployError::Transient(format!("Webhook registration send failed: {e}"))
                    })?;
                    pending_tx.await.map_err(|e| {
                        DeployError::Transient(format!(
                            "Webhook registration confirmation failed: {e}"
                        ))
                    })?;
                }
            }

            Ok(())
        }
        Chain::Starknet => {
            let merchant_pubkey = Felt::from_hex(&task.merchant_address).map_err(|e| {
                DeployError::Fatal(format!("Invalid Starknet merchant_pubkey felt252: {e}"))
            })?;

            // 1. Check if merchant pair already exists via get_merchant_pair(merchant_pubkey)
            let check_call = FunctionCall {
                contract_address: starknet_factory_addr,
                entry_point_selector: get_selector_from_name("get_merchant_pair").unwrap(),
                calldata: vec![merchant_pubkey],
            };

            let pair_res = starknet_account
                .provider()
                .call(check_call, BlockId::Tag(BlockTag::Latest))
                .await
                .map_err(|e| DeployError::Transient(format!("Starknet view call failed: {e}")))?;

            let shield_in_addr = pair_res.first().cloned().unwrap_or(Felt::ZERO);

            if shield_in_addr == Felt::ZERO {
                let target_chain_felt = chain_to_felt(task.target_chain);

                // Parse merchant address as uint256 (low: felt252, high: felt252)
                let cctp_recipient_low = merchant_pubkey;
                let cctp_recipient_high = Felt::ZERO;

                let register_call = Call {
                    to: starknet_factory_addr,
                    selector: get_selector_from_name("register_merchant").unwrap(),
                    calldata: vec![
                        merchant_pubkey,
                        target_chain_felt,
                        cctp_recipient_low,
                        cctp_recipient_high,
                    ],
                };

                let invoke_result = starknet_account
                    .execute_v3(vec![register_call])
                    .send()
                    .await
                    .map_err(|e| DeployError::Transient(format!("Starknet invoke failed: {e}")))?;

                println!(
                    "Starknet merchant pair deployed in tx 0x{:x}",
                    invoke_result.transaction_hash
                );
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
                    let _ = execute_onchain_deployment(
                        evm_client.clone(),
                        starknet_account.clone(),
                        evm_factory_addr,
                        starknet_factory_addr,
                        webhook_registry_addr,
                        &task,
                    )
                    .await;
                } else {
                    eprintln!(
                        "Exhausted retries on {:?} for {}: {}. Dropping task.",
                        task.chain, task.merchant_address, reason
                    );
                }
            }
        }
    }
}

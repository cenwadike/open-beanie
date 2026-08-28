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

use ethers::types::Address;
use ethers::utils::format_bytes32_string;
#[allow(unused_imports)]
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Provider as EvmProvider},
    signers::LocalWallet,
    types::U256,
};

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

// Mirrors MAX_RECEIVERS_PER_MERCHANT (Solidity) / MAX_PAIRS_PER_MERCHANT
// (Cairo) — keep in sync if either contract's cap changes.
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

fn evm_address_to_bytes32(addr: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr.as_bytes());
    out
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
            // as a proper CCTP bytes32 (12 zero bytes + 20 address bytes),
            // since the merchant is both the identity key AND the payout
            // recipient on the target chain.
            let is_same_chain = task.chain == task.target_chain;
            let recipient_bytes;

            if is_same_chain {
                recipient_bytes = [0u8; 32]
            } else {
                recipient_bytes = evm_address_to_bytes32(merchant_addr)
            };

            let tx = factory.register_merchant(
                merchant_addr,
                chain_to_bytes32(task.target_chain),
                recipient_bytes,
            );

            let pending_tx = tx
                .send()
                .await
                .map_err(|e| DeployError::Transient(format!("EVM transaction send failed: {e}")))?;

            pending_tx
                .await
                .map_err(|e| DeployError::Transient(format!("EVM confirmation failed: {e}")))?;

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
            // Starknet is the privacy leg only — it can never be its own
            // target. Staying private on Starknet means configuring
            // ShieldInAnonymizer to spend the shielded note directly
            // within the STRK20 pool, not registering a BridgeOutAnonymizer.
            // The Cairo factory's valid_domains map never registers
            // 'STARKNET' (only BASE/SOLANA/ETHEREUM), so this would always
            // revert INVALID_DOMAIN on-chain — fail fast instead of
            // spending a tx on a guaranteed revert.
            let is_same_chain = task.chain == task.target_chain;
            if is_same_chain && task.target_chain == Chain::Starknet {
                return Err(DeployError::Fatal(
                    "Starknet cannot be its own target_chain; for same-chain privacy, spend the shielded note directly within the STRK20 pool instead of registering a bridge-out pair".to_string(),
                ));
            }

            let merchant_pubkey = Felt::from_hex(&task.merchant_address).map_err(|e| {
                DeployError::Fatal(format!("Invalid Starknet merchant_pubkey felt252: {e}"))
            })?;

            // 1. Check the merchant isn't already at the pair cap.
            // the read entrypoint is `get_merchant_pairs`, which returns
            // Array<MerchantPair>. Cairo's Serde encodes an Array as
            // [length, ...elements], so the first felt in the response is
            // the number of pairs already registered — this is a cap check,
            // not an "already registered" check, since a merchant can hold
            // up to MAX_PAIRS_PER_MERCHANT pairs.
            let check_call = FunctionCall {
                contract_address: starknet_factory_addr,
                entry_point_selector: get_selector_from_name("get_merchant_pairs").unwrap(),
                calldata: vec![merchant_pubkey],
            };

            let pair_res = starknet_account
                .provider()
                .call(check_call, BlockId::Tag(BlockTag::Latest))
                .await
                .map_err(|e| DeployError::Transient(format!("Starknet view call failed: {e}")))?;

            let pair_count: u64 = pair_res
                .first()
                .cloned()
                .unwrap_or(Felt::ZERO)
                .try_into()
                .unwrap_or(u64::MAX);

            if pair_count >= MAX_RECEIVERS_PER_MERCHANT {
                return Err(DeployError::Fatal(format!(
                    "Merchant {merchant_pubkey:#x} already has {pair_count} pairs (max {MAX_RECEIVERS_PER_MERCHANT})"
                )));
            }

            let target_chain_felt = chain_to_felt(task.target_chain);

            // Encode merchant_pubkey as a proper big-endian u256 split
            // (low: felt252, high: felt252), each bounded to 128 bits.
            // The old code put the *entire* felt252 (up to ~252 bits) into
            // `low` with `high = 0` — for any real merchant identity above
            // 2^128 (e.g. one sized to double as an EVM address, per the
            // "merchant is also the recipient" model), Cairo's u128
            // range-check on `low` fails and the call reverts.
            let recipient_be = merchant_pubkey.to_bytes_be();
            let cctp_recipient_high = Felt::from_bytes_be_slice(&recipient_be[0..16]);
            let cctp_recipient_low = Felt::from_bytes_be_slice(&recipient_be[16..32]);

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

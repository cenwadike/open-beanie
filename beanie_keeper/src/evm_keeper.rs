use std::sync::Arc;

use anyhow::{Context, Result as AnyhowResult};
use ethers::{
    abi::{ParamType, Token, decode},
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http, Middleware, Provider, RetryClient},
    signers::{LocalWallet, Signer},
    types::{
        Address, BlockNumber, Eip1559TransactionRequest, Filter, H256, U256, ValueOrArray,
        transaction::eip2718::TypedTransaction,
    },
    utils::keccak256,
};

use crate::config::{Deposit, EvmConfig, now_formatted};

abigen!(
    ChainXReceiver,
    r#"[
        function sweep() external returns (uint256 net, uint256 feeToCaller, uint256 feeToTreasury, uint256 fee)
        function initialized() external view returns (bool)
    ]"#
);

abigen!(
    Erc20,
    r#"[
        function balanceOf(address) external view returns (uint256)
        event Transfer(address indexed from, address indexed to, uint256 value)
    ]"#
);

pub const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

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
    ]"#
);

pub type SignerProvider =
    SignerMiddleware<NonceManagerMiddleware<Provider<RetryClient<Http>>>, LocalWallet>;

pub async fn build_client(cfg: &EvmConfig) -> AnyhowResult<Arc<SignerProvider>> {
    // Retries 10 times max with 2000ms initial backoff on rate limits / 429s
    let provider = Provider::<RetryClient<Http>>::new_client(cfg.evm_rpc_url.as_str(), 10, 2000)
        .context("invalid RPC URL or failed initializing retry client")?;

    let chain_id = provider
        .get_chainid()
        .await
        .context("failed fetching chain id")?
        .as_u64();

    let wallet = cfg.keeper_wallet.clone().with_chain_id(chain_id);
    let address = wallet.address();

    let nonce_managed = NonceManagerMiddleware::new(provider, address);
    Ok(Arc::new(SignerMiddleware::new(nonce_managed, wallet)))
}

pub async fn discover_merchants(
    client: &Arc<SignerProvider>,
    cfg: &EvmConfig,
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<(Address, Address)>> {
    if from_block > to_block {
        return Ok(Vec::new());
    }

    let merchant_reg_topic = H256::from(keccak256("MerchantRegistered(address,address)"));
    let receiver_announced_topic =
        H256::from(keccak256("ReceiverAnnounced(address,address,uint256)"));

    let mut out = Vec::new();
    let mut chunk_start = from_block;

    while chunk_start <= to_block {
        let chunk_end = std::cmp::min(chunk_start + cfg.log_chunk_blocks - 1, to_block);

        // Fetch both event types in one eth_getLogs call
        let filter = Filter::new()
            .address(cfg.factory_address)
            .topic0(vec![merchant_reg_topic, receiver_announced_topic]) // OR
            .from_block(BlockNumber::Number(chunk_start.into()))
            .to_block(BlockNumber::Number(chunk_end.into()));

        let logs = client.get_logs(&filter).await.with_context(|| {
            format!("eth_getLogs failed for merchant registry blocks {chunk_start}-{chunk_end}")
        })?;

        for log in logs {
            if log.topics.is_empty() {
                continue;
            }

            let topic0 = log.topics[0];

            if topic0 == merchant_reg_topic {
                // MerchantRegistered(address indexed merchant, address receiver)
                // topics[1] = merchant, data[12..32] = receiver
                if log.topics.len() < 2 || log.data.len() < 32 {
                    continue;
                }
                let merchant = Address::from(log.topics[1]);
                let receiver = Address::from_slice(&log.data[12..32]);
                out.push((merchant, receiver));
            } else if topic0 == receiver_announced_topic {
                // ReceiverAnnounced(address indexed merchant, address indexed receiver, uint256 nonce)
                // topics[1] = merchant, topics[2] = receiver, data = nonce (ignored)
                if log.topics.len() < 3 {
                    continue;
                }
                let merchant = Address::from(log.topics[1]);
                let receiver = Address::from(log.topics[2]);
                out.push((merchant, receiver));
            }
        }

        chunk_start = chunk_end + 1;
    }

    // Optional: dedup (same address can appear first as Announced, later as Registered)
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out.dedup_by(|a, b| a.1 == b.1);

    Ok(out)
}

pub async fn discover_webhook_urls(
    client: &Arc<SignerProvider>,
    cfg: &EvmConfig,
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<(Address, String)>> {
    if from_block > to_block {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut chunk_start = from_block;
    let webhook_set_topic = H256::from(keccak256("WebhookUrlSet(address,string)"));

    while chunk_start <= to_block {
        let chunk_end = std::cmp::min(chunk_start + cfg.log_chunk_blocks - 1, to_block);

        let filter = Filter::new()
            .address(cfg.webhook_registry_address)
            .topic0(webhook_set_topic)
            .from_block(BlockNumber::Number(chunk_start.into()))
            .to_block(BlockNumber::Number(chunk_end.into()));

        let logs = client.get_logs(&filter).await.with_context(|| {
            format!("eth_getLogs failed for webhook registry blocks {chunk_start}-{chunk_end}")
        })?;

        for log in logs {
            if log.topics.len() < 2 {
                continue;
            }
            let merchant = Address::from(log.topics[1]);

            let url = match decode(&[ParamType::String], &log.data) {
                Ok(mut tokens) => match tokens.remove(0) {
                    Token::String(s) => s,
                    _ => continue,
                },
                Err(_) => continue,
            };

            out.push((merchant, url));
        }

        chunk_start = chunk_end + 1;
    }

    Ok(out)
}

pub async fn fetch_deposits_since_block(
    client: &Arc<SignerProvider>,
    cfg: &EvmConfig,
    receivers: &[Address],
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<Deposit>> {
    if receivers.is_empty() || from_block > to_block {
        return Ok(Vec::new());
    }

    let mut deposits = Vec::new();
    let mut chunk_start = from_block;
    let transfer_topic = H256::from(keccak256("Transfer(address,address,uint256)"));

    while chunk_start <= to_block {
        let chunk_end = std::cmp::min(chunk_start + cfg.log_chunk_blocks - 1, to_block);

        let filter = Filter::new()
            .address(cfg.token_address)
            .topic0(transfer_topic)
            .topic2(ValueOrArray::Array(
                receivers.iter().map(|a| H256::from(*a)).collect(),
            ))
            .from_block(BlockNumber::Number(chunk_start.into()))
            .to_block(BlockNumber::Number(chunk_end.into()));

        let logs = client
            .get_logs(&filter)
            .await
            .with_context(|| format!("eth_getLogs failed for blocks {chunk_start}-{chunk_end}"))?;

        for log in logs {
            if log.topics.len() < 3 {
                continue;
            }
            let from = Address::from(log.topics[1]);
            let to = Address::from(log.topics[2]);
            let amount = U256::from_big_endian(&log.data);
            let block_number = log.block_number.map(|b| b.as_u64()).unwrap_or(chunk_end);
            let tx_hash = log
                .transaction_hash
                .map(|h| format!("{h:?}"))
                .unwrap_or_default();

            deposits.push(Deposit {
                tx_hash,
                from_address: format!("{from:?}"),
                receiver: format!("{to:?}"),
                amount_raw: amount.to_string(),
                block_number,
            });
        }

        chunk_start = chunk_end + 1;
    }

    deposits.sort_by_key(|d| d.block_number);
    Ok(deposits)
}

pub async fn multicall_sweep(
    client: Arc<SignerProvider>,
    receivers: &[Address],
) -> AnyhowResult<Option<String>> {
    if receivers.is_empty() {
        return Ok(None);
    }

    let multicall_addr: Address = MULTICALL3_ADDRESS
        .parse()
        .expect("MULTICALL3_ADDRESS is a hardcoded valid address");
    let multicall = Multicall3::new(multicall_addr, client.clone());

    let mut calls = Vec::with_capacity(receivers.len());
    for &receiver in receivers {
        let receiver_contract = ChainXReceiver::new(receiver, client.clone());
        let call_data = receiver_contract
            .sweep()
            .calldata()
            .context("failed encoding sweep() calldata")?;
        calls.push(Call3 {
            target: receiver,
            allow_failure: true,
            call_data,
        });
    }

    let (suggested_max_fee, suggested_priority_fee) = client
        .estimate_eip1559_fees(None)
        .await
        .context("failed to estimate EIP-1559 fees for multicall sweep")?;

    let max_allowed_priority = ethers::utils::parse_units("0.05", "gwei")
        .context("failed parsing priority fee ceiling")?;
    let priority_fee = std::cmp::min(suggested_priority_fee, max_allowed_priority.into());

    let max_fee_cap =
        ethers::utils::parse_units("0.1", "gwei").context("failed parsing max fee cap")?;
    let max_fee = std::cmp::min(suggested_max_fee, max_fee_cap.into());

    let mut multicall_caller = multicall.aggregate_3(calls);

    let estimated_gas = multicall_caller
        .estimate_gas()
        .await
        .context("failed to estimate gas for multicall aggregate3")?;
    let buffered_gas = estimated_gas * 130 / 100;

    multicall_caller.tx.set_gas(buffered_gas);

    if let Some(eip1559_req) = multicall_caller.tx.as_eip1559_mut() {
        eip1559_req.max_priority_fee_per_gas = Some(priority_fee);
        eip1559_req.max_fee_per_gas = Some(max_fee);
    } else {
        let legacy = multicall_caller.tx.clone();
        multicall_caller.tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
            from: legacy.from().copied(),
            to: legacy.to().cloned(),
            gas: Some(buffered_gas),
            value: legacy.value().copied(),
            data: legacy.data().cloned(),
            nonce: legacy.nonce().copied(),
            access_list: Default::default(),
            max_priority_fee_per_gas: Some(priority_fee),
            max_fee_per_gas: Some(max_fee),
            chain_id: legacy.chain_id(),
        });
    }

    let pending = multicall_caller
        .send()
        .await
        .context("aggregate3 send failed")?;

    let receipt = pending
        .await
        .context("aggregate3 tx dropped before confirmation")?;

    match receipt {
        Some(r) => {
            let tx_hash = format!("{:?}", r.transaction_hash);
            println!(
                "[{}] multicall swept {} receiver(s) — tx {}",
                now_formatted(),
                receivers.len(),
                tx_hash
            );
            Ok(Some(tx_hash))
        }
        None => Ok(None),
    }
}

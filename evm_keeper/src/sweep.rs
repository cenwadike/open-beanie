use std::sync::Arc;

use anyhow::{Context, Result as AnyhowResult};
use ethers::{
    abi::{ParamType, Token, decode},
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http, Middleware, Provider},
    signers::{LocalWallet, Signer},
    types::{Address, BlockNumber, Filter, H256, U256, ValueOrArray},
};

use crate::config::{Config, Deposit, now_formatted};

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

// Deployed at this address on virtually every major EVM chain (Base and Ethereum
// mainnet included) — confirm it's actually live on whichever chain you're pointing at
// before relying on it.
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

pub type SignerProvider = SignerMiddleware<NonceManagerMiddleware<Provider<Http>>, LocalWallet>;

pub async fn build_client(cfg: &Config) -> AnyhowResult<Arc<SignerProvider>> {
    let provider = Provider::<Http>::try_from(cfg.rpc_url.as_str()).context("invalid RPC URL")?;
    let chain_id = provider
        .get_chainid()
        .await
        .context("failed fetching chain id")?
        .as_u64();

    // Same wallet used for webhook signing, now bound to this chain's ID for tx signing —
    // chain ID only affects the EIP-155 transaction domain, not the address itself.
    let wallet = cfg.keeper_wallet.clone().with_chain_id(chain_id);

    let address = wallet.address();
    // Concurrent sweep() sends from the same signer need explicit nonce sequencing —
    // without this, parallel sweeps for multiple merchants in one cycle will race.
    let nonce_managed = NonceManagerMiddleware::new(provider, address);
    Ok(Arc::new(SignerMiddleware::new(nonce_managed, wallet)))
}

/// Discover (merchant, receiver) pairs registered on the factory since `from_block`.
/// This IS the tenant registry — same principle as the Solana design's
/// `getProgramAccounts` scan: it already lives on-chain, nothing kept in sync separately.
pub async fn discover_merchants(
    client: &Arc<SignerProvider>,
    cfg: &Config,
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<(Address, Address)>> {
    if from_block > to_block {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut chunk_start = from_block;

    // Loop through blocks in structured chunks to respect the 10,000 RPC limit
    while chunk_start <= to_block {
        let chunk_end = std::cmp::min(chunk_start + cfg.log_chunk_blocks - 1, to_block);

        let filter = Filter::new()
            .address(cfg.factory_address)
            .event("MerchantRegistered(address,address)")
            .from_block(BlockNumber::Number(chunk_start.into()))
            .to_block(BlockNumber::Number(chunk_end.into()));

        let logs = client.get_logs(&filter).await.with_context(|| {
            format!("eth_getLogs failed for merchant registry blocks {chunk_start}-{chunk_end}")
        })?;

        for log in logs {
            if log.topics.len() < 2 {
                continue;
            }
            let merchant = Address::from(log.topics[1]);
            // `receiver` is non-indexed in MerchantRegistered, so it's in log.data (32-byte
            // word, address right-aligned in the last 20 bytes).
            if log.data.len() < 32 {
                continue;
            }
            let receiver = Address::from_slice(&log.data[12..32]);
            out.push((merchant, receiver));
        }

        chunk_start = chunk_end + 1;
    }

    Ok(out)
}

/// Scans MerchantWebhookRegistry's `WebhookUrlSet` events to build merchant -> URL.
/// Unlike the fixed-width address in MerchantRegistered, `url` is a dynamic `string` —
/// it can't be sliced out of log.data by a fixed offset, it needs real ABI decoding.
/// eth_getLogs returns logs in on-chain order, so later entries for the same merchant
/// correctly overwrite earlier ones as the caller folds this into a map.
pub async fn discover_webhook_urls(
    client: &Arc<SignerProvider>,
    cfg: &Config,
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<(Address, String)>> {
    if from_block > to_block {
        return Ok(Vec::new());
    }

    let filter = Filter::new()
        .address(cfg.webhook_registry_address)
        .event("WebhookUrlSet(address,string)")
        .from_block(BlockNumber::Number(from_block.into()))
        .to_block(BlockNumber::Number(to_block.into()));

    let logs = client
        .get_logs(&filter)
        .await
        .context("eth_getLogs failed for webhook registry")?;

    let mut out = Vec::new();
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
    Ok(out)
}

/// One batched scan covering every known receiver — the EVM equivalent of Solana's
/// per-account signature walk, but collapsed into a single filtered eth_getLogs call
/// per block range since `to` is an indexed Transfer topic.
pub async fn fetch_deposits_since_block(
    client: &Arc<SignerProvider>,
    cfg: &Config,
    receivers: &[Address],
    from_block: u64,
    to_block: u64,
) -> AnyhowResult<Vec<Deposit>> {
    if receivers.is_empty() || from_block > to_block {
        return Ok(Vec::new());
    }

    let mut deposits = Vec::new();
    let mut chunk_start = from_block;

    while chunk_start <= to_block {
        let chunk_end = std::cmp::min(chunk_start + cfg.log_chunk_blocks - 1, to_block);

        let filter = Filter::new()
            .address(cfg.token_address)
            .event("Transfer(address,address,uint256)")
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

/// One transaction sweeps every receiver in `receivers`, via Multicall3.aggregate3().
/// `allowFailure: true` per call means one merchant's revert (already swept, lost the
/// race to zero balance, etc.) doesn't take the rest of the batch down with it. Only
/// call this for receivers you already know saw a deposit this cycle — unlike Solana's
/// sweep instruction, every included call still costs gas on the batch regardless of
/// whether that particular receiver had anything to sweep.
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

    let multicall_caller = multicall.aggregate_3(calls);
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

use anyhow::Result;
use starknet::{
    accounts::{Account, ConnectedAccount, ExecutionEncoding, SingleOwnerAccount},
    core::{
        types::{BlockId, Call, EventFilter, Felt},
        utils::get_selector_from_name,
    },
    providers::{JsonRpcClient, Provider, jsonrpc::HttpTransport},
    signers::LocalWallet,
};
use std::sync::Arc;
use url::Url;

use crate::config::Deposit;
use crate::config::StarknetConfig;

pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>;

pub fn build_starknet_account(cfg: &StarknetConfig) -> Result<Arc<StarknetAccount>> {
    let provider = JsonRpcClient::new(HttpTransport::new(Url::parse(&cfg.rpc_url)?));

    let chain_id = starknet::core::chain_id::MAINNET;

    let account = SingleOwnerAccount::new(
        provider,
        cfg.keeper_wallet.clone(),
        cfg.keeper_address,
        chain_id,
        ExecutionEncoding::New,
    );

    Ok(Arc::new(account))
}

/// Starknet event fetcher for MerchantRegistered(merchant, receiver)
pub async fn discover_merchants(
    account: &StarknetAccount,
    cfg: &StarknetConfig,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<(Felt, Felt)>> {
    if from_block > to_block {
        return Ok(Vec::new());
    }

    let event_selector = get_selector_from_name("MerchantRegistered")?;
    let mut out = Vec::new();
    let step = cfg.log_chunk_blocks.max(1);

    let mut current_from = from_block;
    while current_from <= to_block {
        let current_to = (current_from + step - 1).min(to_block);
        let mut continuation_token: Option<String> = None;

        loop {
            let filter = EventFilter {
                from_block: Some(BlockId::Number(current_from)),
                to_block: Some(BlockId::Number(current_to)),
                address: Some(cfg.factory_address),
                keys: Some(vec![vec![event_selector]]),
            };

            let events_page = account
                .provider()
                .get_events(filter, continuation_token.clone(), 100)
                .await?;

            for event in events_page.events {
                if event.data.len() >= 2 {
                    out.push((event.data[0], event.data[1]));
                }
            }

            continuation_token = events_page.continuation_token;
            if continuation_token.is_none() {
                break;
            }
        }

        current_from = current_to + 1;
    }

    Ok(out)
}

/// Scans Starknet ERC-20 Transfer events with block chunking and pagination.
pub async fn fetch_deposits_since_block(
    account: &StarknetAccount,
    cfg: &StarknetConfig,
    receivers: &[Felt],
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Deposit>> {
    if receivers.is_empty() || from_block > to_block {
        return Ok(Vec::new());
    }

    let transfer_selector = get_selector_from_name("Transfer")?;
    let mut deposits = Vec::new();
    let keys = vec![vec![transfer_selector], vec![], receivers.to_vec()];
    let step = cfg.log_chunk_blocks.max(1);

    let mut current_from = from_block;
    while current_from <= to_block {
        let current_to = (current_from + step - 1).min(to_block);
        let mut continuation_token: Option<String> = None;

        loop {
            let filter = EventFilter {
                from_block: Some(BlockId::Number(current_from)),
                to_block: Some(BlockId::Number(current_to)),
                address: Some(cfg.token_address),
                keys: Some(keys.clone()),
            };

            let events_page = account
                .provider()
                .get_events(filter, continuation_token.clone(), 1000)
                .await?;

            for event in events_page.events {
                if event.keys.len() < 3 || event.data.len() < 2 {
                    continue;
                }

                let from = event.keys[1];
                let to = event.keys[2];

                // Cairo u256 consists of 2 felts: low (data[0]), high (data[1])
                let low: u128 = event.data[0].try_into().unwrap_or(0);
                let high: u128 = event.data[1].try_into().unwrap_or(0);
                let amount = u256_to_string(low, high);

                deposits.push(Deposit {
                    tx_hash: format!("{:#x}", event.transaction_hash),
                    from_address: format!("{:#x}", from),
                    receiver: format!("{:#x}", to),
                    amount_raw: amount,
                    block_number: event.block_number.unwrap_or(current_to),
                });
            }

            continuation_token = events_page.continuation_token;
            if continuation_token.is_none() {
                break;
            }
        }

        current_from = current_to + 1;
    }

    Ok(deposits)
}

/// Helper to convert low/high u128 felts to full integer string representation
fn u256_to_string(low: u128, high: u128) -> String {
    if high == 0 {
        low.to_string()
    } else {
        let val = (u128::from(high) << 64) | u128::from(low); // Fits within u128 for normal standard ranges
        val.to_string()
    }
}

/// Sweeps multiple receivers via Starknet's native multicall mechanism.
pub async fn multicall_sweep(
    account: Arc<StarknetAccount>,
    receivers: &[Felt],
) -> Result<Option<String>> {
    if receivers.is_empty() {
        return Ok(None);
    }

    let sweep_selector = get_selector_from_name("sweep")?;

    let calls: Vec<Call> = receivers
        .iter()
        .map(|&receiver| Call {
            to: receiver,
            selector: sweep_selector,
            calldata: vec![],
        })
        .collect();

    let result = account.execute_v3(calls).send().await?;

    Ok(Some(format!("{:#x}", result.transaction_hash)))
}

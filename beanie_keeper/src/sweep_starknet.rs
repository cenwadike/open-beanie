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

    // Choose appropriate network chain_id (e.g., MAINNET or SEPOLIA)
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

    let filter = EventFilter {
        from_block: Some(BlockId::Number(from_block)),
        to_block: Some(BlockId::Number(to_block)),
        address: Some(cfg.factory_address),
        keys: Some(vec![vec![event_selector]]),
    };

    let events_page = account.provider().get_events(filter, None, 100).await?;

    for event in events_page.events {
        // According to Cairo implementation:
        // event.data[0] = merchant, event.data[1] = receiver
        if event.data.len() >= 2 {
            out.push((event.data[0], event.data[1]));
        }
    }

    Ok(out)
}

/// Scans Starknet ERC-20 Transfer events.
/// Cairo Transfer event keys layout:
/// keys[0] = starknet_keccak("Transfer")
/// keys[1] = from
/// keys[2] = to (receiver)
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

    // Setup Starknet key query (keys[0] = Transfer, keys[1] = wildcard, keys[2] = receivers filter)
    let keys = vec![vec![transfer_selector], vec![], receivers.to_vec()];

    let filter = EventFilter {
        from_block: Some(BlockId::Number(from_block)),
        to_block: Some(BlockId::Number(to_block)),
        address: Some(cfg.token_address),
        keys: Some(keys),
    };

    let events_page = account.provider().get_events(filter, None, 1000).await?;

    for event in events_page.events {
        if event.keys.len() < 3 || event.data.len() < 2 {
            continue;
        }

        let from = event.keys[1];
        let to = event.keys[2];

        // Cairo u256 consists of 2 felts in event.data: low (data[0]), high (data[1])
        let low: u128 = event.data[0].try_into().unwrap_or(0);
        let high: u128 = event.data[1].try_into().unwrap_or(0);
        let amount = ((high as u128) << 64) | low; // Or parse into a standard Uint library

        deposits.push(Deposit {
            tx_hash: format!("{:#x}", event.transaction_hash),
            from_address: format!("{:#x}", from),
            receiver: format!("{:#x}", to),
            amount_raw: amount.to_string(),
            block_number: event.block_number.unwrap_or(to_block),
        });
    }

    Ok(deposits)
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

    // Build the array of execution calls
    let calls: Vec<Call> = receivers
        .iter()
        .map(|&receiver| Call {
            to: receiver,
            selector: sweep_selector,
            calldata: vec![],
        })
        .collect();

    // Execute directly through the account contract
    let result = account.execute_v3(calls).send().await?;

    Ok(Some(format!("{:#x}", result.transaction_hash)))
}

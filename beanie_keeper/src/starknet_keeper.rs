use anyhow::{Context, Result};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue, ORIGIN, USER_AGENT},
};
use starknet::{
    accounts::{Account, ExecutionEncoding, SingleOwnerAccount},
    core::{
        types::{BlockId, BlockTag, Call, Felt, FunctionCall, requests::CallRequest},
        utils::get_selector_from_name,
    },
    providers::{
        JsonRpcClient, Provider, ProviderRequestData, ProviderResponseData, jsonrpc::HttpTransport,
    },
    signers::LocalWallet,
};
use std::collections::HashSet;
use std::sync::Arc;
use url::Url;

use crate::config::StarknetConfig;

pub type StarknetAccount = SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>;
pub type StarknetProvider = JsonRpcClient<HttpTransport>;

pub fn build_starknet_account(cfg: &StarknetConfig) -> Result<Arc<StarknetAccount>> {
    let parsed_url = Url::parse(&cfg.rpc_url)?;
    let mut headers = HeaderMap::new();

    headers.insert(ORIGIN, HeaderValue::from_str(parsed_url.as_str())?);
    headers.insert(USER_AGENT, HeaderValue::from_static("beanie-keeper/1.0"));

    let reqwest_client = Client::builder().default_headers(headers).build()?;
    let transport = HttpTransport::new_with_client(parsed_url, reqwest_client);
    let provider = JsonRpcClient::new(transport);
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

/// Batched ERC-20 `balanceOf` check across candidate receivers in one HTTP round trip.
pub async fn batch_check_nonzero_balance(
    provider: &StarknetProvider,
    token_address: Felt,
    receivers: &[Felt],
) -> Result<HashSet<Felt>> {
    if receivers.is_empty() {
        return Ok(HashSet::new());
    }

    let balance_of_selector = get_selector_from_name("balanceOf")?;

    let requests: Vec<ProviderRequestData> = receivers
        .iter()
        .map(|&receiver| {
            ProviderRequestData::Call(CallRequest {
                request: FunctionCall {
                    contract_address: token_address,
                    entry_point_selector: balance_of_selector,
                    calldata: vec![receiver],
                },
                block_id: BlockId::Tag(BlockTag::L1Accepted),
            })
        })
        .collect();

    let responses = provider
        .batch_requests(requests)
        .await
        .context("batched balanceOf request failed")?;

    let mut nonzero = HashSet::with_capacity(receivers.len());
    for (&receiver, response) in receivers.iter().zip(responses) {
        match response {
            ProviderResponseData::Call(values) => {
                if values.iter().any(|f| *f != Felt::ZERO) {
                    nonzero.insert(receiver);
                }
            }
            other => {
                eprintln!(
                    "balanceOf batch: unexpected response variant for receiver {receiver:#x}: {other:?} — treating as zero balance"
                );
            }
        }
    }

    Ok(nonzero)
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

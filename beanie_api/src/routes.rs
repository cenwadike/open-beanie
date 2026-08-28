use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use ethers::abi::Abi;
use ethers::contract::Contract;
use ethers::providers::{Http, Provider};
use ethers::types::Address;
use starknet::core::utils::get_selector_from_name;
use starknet::providers::{JsonRpcClient, Provider as StarknetProvider};
use starknet::{
    core::types::{BlockId, BlockTag, Felt, FunctionCall},
    providers::jsonrpc::HttpTransport,
};
use std::str::FromStr;
use std::sync::Arc;

use crate::config::Config;
use crate::models::{
    Chain, DeployTask, HttpResponseCache, InitLaneRequest, InitLaneResponse, LaneDeployment,
    SocketAddr, err, mpsc,
};
use crate::rate_limiter::DualRateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub limiter: Arc<DualRateLimiter>,
    pub http_cache: Arc<HttpResponseCache>,
    pub deploy_tx: mpsc::Sender<DeployTask>,
    pub evm_provider: Arc<Provider<Http>>,
    pub starknet_provider: Arc<JsonRpcClient<HttpTransport>>,
}

// ABI snippet required specifically for factory prediction
const EVM_FACTORY_ABI: &str = r#"[
    {
        "inputs": [{"internalType": "address", "name": "merchant", "type": "address"}],
        "name": "predictReceiverAddress",
        "outputs": [{"internalType": "address", "name": "", "type": "address"}],
        "stateMutability": "view",
        "type": "function"
    }
]"#;

pub async fn init_lane(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<InitLaneRequest>,
) -> Response {
    let merchant_address = payload.merchant_address.trim();
    if merchant_address.is_empty() {
        return err(StatusCode::BAD_REQUEST, "merchant_address cannot be empty");
    }

    if payload.source_chains.is_empty() {
        return err(StatusCode::BAD_REQUEST, "source_chains cannot be empty");
    }

    let mut source_chains = payload.source_chains.clone();
    source_chains.insert(payload.target_chain);

    for chain in source_chains.iter() {
        if Chain::try_from(*chain).is_err() {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("{:?} is not a supported chain. Use BASE or STARKNET", chain),
            );
        }
    }

    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{:?}_{}_{:?}_{:?}",
                addr.ip(),
                merchant_address.to_lowercase(),
                payload.source_chains,
                payload.webhook_url
            )
        });

    if let Some((cached_status, cached_response)) = state.http_cache.get(&idempotency_key) {
        return (cached_status, Json(cached_response)).into_response();
    }

    if let Err(msg) = state.limiter.check(addr.ip(), merchant_address) {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    let mut lane_results = Vec::new();

    for chain in &source_chains {
        let is_privacy = *chain == Chain::Starknet || payload.enable_privacy;
        let predicted_address = match chain {
            Chain::Base | Chain::Ethereum => {
                let parsed_merchant = match Address::from_str(merchant_address) {
                    Ok(addr) => addr,
                    Err(_) => return err(StatusCode::BAD_REQUEST, "Invalid EVM merchant address"),
                };

                let abi: Abi =
                    serde_json::from_str(EVM_FACTORY_ABI).expect("Failed to parse EVM_FACTORY_ABI");
                let factory_contract = Contract::new(
                    state.config.evm_factory_address,
                    abi,
                    state.evm_provider.clone(),
                );

                // On-chain RPC view call to EVM MerchantFactory contract
                match factory_contract
                    .method::<_, Address>("predictReceiverAddress", parsed_merchant)
                {
                    Ok(method) => match method.call().await {
                        Ok(addr) => format!("{:#x}", addr),
                        Err(e) => {
                            return err(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &format!("Failed to predict EVM address on-chain: {e}"),
                            );
                        }
                    },
                    Err(e) => {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Invalid contract call format: {e}"),
                        );
                    }
                }
            }
            Chain::Starknet => {
                let merchant_pubkey = match Felt::from_hex(merchant_address) {
                    Ok(felt) => felt,
                    Err(_) => {
                        return err(StatusCode::BAD_REQUEST, "Invalid Starknet merchant pubkey");
                    }
                };

                // On-chain view call targeting Cairo contract: `predict_shield_in_address(merchant_pubkey)`
                let call = FunctionCall {
                    contract_address: state.config.starknet_factory_address,
                    entry_point_selector: get_selector_from_name("predict_shield_in_address")
                        .expect("Valid selector"),
                    calldata: vec![merchant_pubkey],
                };

                match state
                    .starknet_provider
                    .call(call, BlockId::Tag(BlockTag::L1Accepted))
                    .await
                {
                    Ok(result) => {
                        if let Some(stark_addr) = result.first() {
                            format!("{:#x}", stark_addr)
                        } else {
                            return err(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Starknet call returned an empty result",
                            );
                        }
                    }
                    Err(e) => {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Failed to predict Starknet address on-chain: {e}"),
                        );
                    }
                }
            }
            Chain::Solana => {
                format!("SOL_{}", &merchant_address[..6.min(merchant_address.len())])
            }
        };

        let task = DeployTask {
            chain: *chain,
            merchant_address: merchant_address.to_string(),
            target_chain: payload.target_chain,
            webhook_url: payload.webhook_url.clone(),
            attempts: 0,
        };

        if state.deploy_tx.send(task).await.is_err() {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Deployment queue is full, try again shortly",
            );
        }

        lane_results.push(LaneDeployment {
            chain: *chain,
            address: predicted_address,
            is_privacy_lane: is_privacy,
        });
    }

    let response_body = InitLaneResponse {
        lanes: lane_results,
    };
    let status = StatusCode::OK;

    state
        .http_cache
        .insert(idempotency_key, status, response_body.clone());

    (status, Json(response_body)).into_response()
}

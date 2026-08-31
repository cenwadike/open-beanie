use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use ethers::contract::Contract;
use ethers::providers::{Http, Provider};
use ethers::types::Address;
use ethers::{abi::Abi, utils::keccak256};
use starknet::core::utils::get_selector_from_name;
use starknet::providers::{JsonRpcClient, Provider as StarknetProvider};
use starknet::{
    core::types::{BlockId, BlockTag, Felt, FunctionCall},
    providers::jsonrpc::HttpTransport,
};
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceExt;
use tower_http::services::ServeFile;

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
    pub deploy_tx: Arc<mpsc::Sender<DeployTask>>,
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

    // Bypass stale local cache during dev or derive unique idempotency key
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{:?}_{}_{:?}_{:?}_{}",
                addr.ip(),
                merchant_address,
                payload.source_chains,
                payload.webhook_url,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
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
        // Whether funds actually settle here (the merchant's real wallet)
        // vs. this being a pass-through receiver that forwards on to the
        // target chain via CCTP. Only the target gets strict, fallback-free
        // address parsing — see identity::resolve_evm_identity's doc comment
        // for why silently hash-deriving a settlement address would be a
        // fund-loss footgun.
        let is_target = *chain == payload.target_chain;

        let predicted_address = match chain {
            Chain::Base | Chain::Ethereum => {
                let parsed_merchant = if is_target {
                    match Address::from_str(merchant_address) {
                        Ok(addr) => addr,
                        Err(_) => {
                            return err(
                                StatusCode::BAD_REQUEST,
                                "Settlement (target) chain address must be a valid EVM address",
                            );
                        }
                    }
                } else {
                    merchant_address.parse().unwrap_or_else(|_| {
                        let hash = keccak256(merchant_address.as_bytes());
                        Address::from_slice(&hash[12..32])
                    })
                };

                let abi: Abi =
                    serde_json::from_str(EVM_FACTORY_ABI).expect("Failed to parse EVM_FACTORY_ABI");
                let factory_contract = Contract::new(
                    state.config.evm_factory_address,
                    abi,
                    state.evm_provider.clone(),
                );

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
                // Ensure proper hex parsing for Starknet Felt
                let hex_str =
                    if merchant_address.starts_with("0x") || merchant_address.starts_with("0X") {
                        format!("0x{}", &merchant_address[2..].to_lowercase())
                    } else {
                        format!("0x{}", merchant_address.to_lowercase())
                    };

                let parsed_merchant = match Felt::from_hex(&hex_str) {
                    Ok(felt) => felt,
                    Err(_) => {
                        return err(StatusCode::BAD_REQUEST, "Invalid Starknet merchant address");
                    }
                };

                let call = FunctionCall {
                    contract_address: state.config.starknet_factory_address,
                    entry_point_selector: get_selector_from_name("predict_receiver_address")
                        .expect("Valid selector"),
                    calldata: vec![parsed_merchant],
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
                unimplemented!();
            }
        };

        let task = DeployTask {
            chain: *chain,
            merchant_address: merchant_address.to_string(),
            target_chain: payload.target_chain,
            webhook_url: payload.webhook_url.clone(),
            attempts: 0,
        };

        let _ = state.deploy_tx.send(task).await;

        lane_results.push(LaneDeployment {
            chain: *chain,
            address: predicted_address,
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

/// Clean-URL static file server for `public/`:
/// - `/`                     -> public/beanie.html
/// - `/page`                 -> public/page.html   (canonical form)
/// - `/page.html`            -> 302 redirect to `/page`
/// - `/scripts/x.js`         -> public/scripts/x.js (served as-is)
/// - `/styles/x.css`         -> public/styles/x.css (served as-is)
/// - `/assets/x.png`         -> public/assets/x.png (served as-is)
/// - anything unhandled       -> 302 redirect to `/`
pub async fn serve_static(req: Request) -> Response {
    let path = req.uri().path().to_string();

    // 1. Root route handler
    if path == "/" {
        return serve_file("public/beanie.html", Some("text/html"), req).await;
    }

    // 2. Direct ".html" requests: canonicalize to extensionless form
    if let Some(clean) = path.strip_suffix(".html") {
        let candidate = format!("public{clean}.html");
        return if tokio::fs::metadata(&candidate).await.is_ok() {
            Redirect::to(clean).into_response()
        } else {
            Redirect::to("/").into_response()
        };
    }

    // Check if the route is explicitly directed to a static directory payload
    let is_asset_dir = path.starts_with("/scripts/")
        || path.starts_with("/styles/")
        || path.starts_with("/assets/");

    let has_ext = path.rsplit('/').next().unwrap_or("").contains('.');

    // 3. Extensionless page routes (e.g., "/terms" -> "public/terms.html")
    if !has_ext && !is_asset_dir {
        let candidate = format!("public{path}.html");
        return if tokio::fs::metadata(&candidate).await.is_ok() {
            serve_file(&candidate, Some("text/html"), req).await
        } else {
            Redirect::to("/").into_response()
        };
    }

    // 4. Real static asset handling (e.g., images, scripts, stylesheets)
    let candidate = format!("public{path}");
    if tokio::fs::metadata(&candidate).await.is_ok() {
        serve_file(&candidate, None, req).await
    } else {
        // Return a clean 404 for missing assets so they don't ingest the fallback landing page
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Helper function to safely stream file assets with explicit or guessed Content-Type headers
pub async fn serve_file(path: &str, forced_content_type: Option<&str>, req: Request) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(mut res) => {
            let content_type = match forced_content_type {
                Some(explicit_type) => explicit_type.to_string(),
                None => mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string(),
            };

            res.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(&content_type).unwrap(),
            );

            res.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

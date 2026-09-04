//! Beanie Lanes API — the only HTTP endpoint in Beanie.
//!
//! One route: POST /lanes/init. No signup, no login, no wallet connect, no
//! signed message — paste a destination address and optional paramters
//! to get a deterministically predicted receivers back immediately.
//! Actual on-chain registration happens in the
//! background, paid for by whoever runs this process.
//!
//! MerchantFactory.registerMerchant() has no caller restriction — it's
//! already permissionless. This process is a convenience, not a gatekeeper:
//! anyone who doesn't trust a given operator's rate limits can call the
//! factory directly with their own wallet and skip this entirely. Because
//! there's no fee or incentive for running this API, rate limiting per IP
//! is the only thing standing between "free for everyone" and "free until
//! someone drains the operator's gas."
//!
//! Starknet's shield-in leg requires the merchant
//! to hold a real keypair to ever spend their shielded notes later, which
//! is incompatible with "no crypto knowledge required."
//!
//! Hence privacy is opted-in by privacy seeking merchants
//!
//! Env vars: RPC_URL, FACTORY_ADDRESS, CHAIN_NAME (e.g. "BASE" — used only
//! as the cctpMintChain label on the same-chain settlement path, see
//! init_lane), KEEPER_PRIVATE_KEY, RATE_LIMIT_PER_HOUR (default 5),
//! LISTEN_ADDR (default 0.0.0.0:8080).

mod config;
mod models;
mod payment_routes;
mod payment_workers;
mod rate_limiter;
mod stealth_routes;
mod stealth_workers;
mod transfer_workers;
mod webhook_worker;

use crate::stealth_workers::start_stealth_workers;
use axum::{
    Router,
    routing::{get, post},
};
use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use beanie_keeper::config::StarknetConfig;
use ethers::{
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http as EvmHttp, Middleware, Provider as EvmProvider},
    signers::{LocalWallet, Signer},
};
use starknet::{
    accounts::{ExecutionEncoding, SingleOwnerAccount},
    providers::jsonrpc::{HttpTransport, JsonRpcClient},
    signers::{LocalWallet as StarknetWallet, SigningKey},
};
use std::{sync::Arc, time::Duration};

use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::models::PaymentTask;
use crate::models::{StealthTask, mpsc};
use crate::payment_workers::run_payment_worker;
use crate::rate_limiter::DualRateLimiter;
use crate::stealth_routes::execute_stealth_claim;
use crate::{config::Config, models::AppState};

/// Fallback route handler for serving static frontend files and pretty HTML URLs.
pub async fn serve_static(req: Request) -> Response {
    let path = req.uri().path().to_string();

    // Serve root index page
    if path == "/" {
        return serve_file("public/beanie.html", Some("text/html"), req).await;
    }

    // Handle clean HTML URLs without .html extension
    if let Some(clean) = path.strip_suffix(".html") {
        let candidate = format!("public{clean}.html");
        return if tokio::fs::metadata(&candidate).await.is_ok() {
            Redirect::to(clean).into_response()
        } else {
            Redirect::to("/").into_response()
        };
    }

    let is_asset_dir = path.starts_with("/scripts/")
        || path.starts_with("/styles/")
        || path.starts_with("/assets/");

    let has_ext = path.rsplit('/').next().unwrap_or("").contains('.');

    // Fall back to clean HTML file lookup if path doesn't contain a file extension or asset directory
    if !has_ext && !is_asset_dir {
        let candidate = format!("public{path}.html");
        return if tokio::fs::metadata(&candidate).await.is_ok() {
            serve_file(&candidate, Some("text/html"), req).await
        } else {
            Redirect::to("/").into_response()
        };
    }

    // Serve raw static asset files
    let candidate = format!("public{path}");
    if tokio::fs::metadata(&candidate).await.is_ok() {
        serve_file(&candidate, None, req).await
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Helper function to stream static files using `tower_http::services::ServeFile`.
pub async fn serve_file(path: &str, forced_content_type: Option<&str>, req: Request) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(mut res) => {
            // Infer or enforce correct Content-Type header
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cfg = Config::from_env()?;

    // 1. Initialize EVM Provider & Signer Client
    let evm_provider = EvmProvider::<EvmHttp>::try_from(cfg.evm_rpc_url.as_str())?;
    let wallet: LocalWallet = cfg.evm_keeper_private_key.parse()?;
    let chain_id = evm_provider.get_chainid().await?.as_u64();
    let wallet = wallet.with_chain_id(chain_id);

    let nonce_managed = NonceManagerMiddleware::new(evm_provider.clone(), wallet.address());
    let evm_client = Arc::new(SignerMiddleware::new(nonce_managed, wallet));

    // 2. Initialize Starknet Account Client
    let rpc_client = JsonRpcClient::new(HttpTransport::new(cfg.starknet_rpc_url.clone()));
    let starknet_signer =
        StarknetWallet::from(SigningKey::from_secret_scalar(cfg.starknet_private_key));

    let starknet_account = Arc::new(SingleOwnerAccount::new(
        rpc_client.clone(),
        starknet_signer,
        cfg.starknet_account_address,
        cfg.starknet_chain_id,
        ExecutionEncoding::New,
    ));

    // 3. Setup Bounded Channels and Background Workers
    let (stealth_tx, stealth_rx) = mpsc::channel::<StealthTask>(2048);
    let stealth_tx = Arc::new(stealth_tx);

    let (payment_tx, payment_rx) = mpsc::channel::<PaymentTask>(2048);
    let payment_tx = Arc::new(payment_tx);
    let (webhook_tx, webhook_rx) = mpsc::channel::<crate::models::WebhookJob>(4096);
    let webhook_tx = Arc::new(webhook_tx);

    let state = AppState {
        limiter: Arc::new(DualRateLimiter::new(
            cfg.rate_limit_per_hour,
            2,
            Duration::from_secs(3600),
        )),
        stealth_tx: stealth_tx.clone(),
        payment_tx: payment_tx.clone(),
        app_config: Arc::new(cfg.clone()),
        starknet_config: Arc::new(StarknetConfig::from_env()?),
        evm_config: Arc::new(beanie_keeper::config::EvmConfig::from_env()?),
        reqwest_client: Arc::new(reqwest::Client::builder().build()?),
    };

    let worker_state = Arc::new(state.clone());
    tokio::spawn(start_stealth_workers(worker_state, stealth_rx));
    let evm_client_clone = evm_client.clone();
    let starknet_account_clone = starknet_account.clone();
    tokio::spawn(run_payment_worker(
        evm_client_clone,
        starknet_account_clone,
        cfg.evm_factory_address,
        cfg.starknet_factory_address,
        state.evm_config.clone(),
        state.starknet_config.clone(),
        payment_rx,
        webhook_tx.clone(),
    ));

    // Native transfer poller (sweeps and dispatches webhooks)
    let evm_client_clone2 = evm_client.clone();
    let evm_cfg_clone = state.evm_config.clone();
    let starknet_cfg_clone = state.starknet_config.clone();
    tokio::spawn(async move {
        crate::transfer_workers::run_native_transfer_poller(
            evm_client_clone2,
            evm_cfg_clone,
            starknet_cfg_clone,
            webhook_tx.clone(),
        )
        .await;
    });

    // Spawn webhook delivery worker
    let http_for_webhooks = state.reqwest_client.clone();
    tokio::spawn(async move {
        crate::webhook_worker::run_webhook_worker(http_for_webhooks, webhook_rx).await;
    });

    let app = Router::new()
        .route("/api/v1/stealth/execute", post(execute_stealth_claim))
        .route(
            "/api/v1/payment/notify",
            post(crate::payment_routes::receive_payment),
        )
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
        .fallback(serve_static);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    println!("Beanie Lanes API running on {}", cfg.listen_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::models::SocketAddr>(),
    )
    .await?;

    Ok(())
}

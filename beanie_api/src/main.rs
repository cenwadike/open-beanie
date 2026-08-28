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
mod rate_limiter;
mod routes;
mod worker;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    routing::{get, post},
};
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

use crate::config::Config;
use crate::models::{DeployTask, HttpResponseCache, mpsc};
use crate::rate_limiter::DualRateLimiter;
use crate::routes::{AppState, init_lane};
use crate::worker::run_deployment_worker;

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

    // 3. Setup Bounded Channel and Background Deployment Worker
    let (deploy_tx, deploy_rx) = mpsc::channel::<DeployTask>(2048);

    tokio::spawn(run_deployment_worker(
        evm_client,
        starknet_account,
        cfg.evm_factory_address,
        cfg.starknet_factory_address,
        cfg.webhook_registry_address,
        deploy_rx,
    ));

    // 4. Router & Server Setup
    let state = AppState {
        limiter: Arc::new(DualRateLimiter::new(
            cfg.rate_limit_per_hour,
            2,
            Duration::from_secs(3600),
        )),
        http_cache: Arc::new(HttpResponseCache::new(Duration::from_secs(86400))),
        deploy_tx,
        config: Arc::new(cfg.clone()),
        evm_provider: Arc::new(evm_provider),
        starknet_provider: Arc::new(rpc_client),
    };

    let app = Router::new()
        .route("/lanes/init", post(init_lane))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    println!("Beanie Lanes API running on {}", cfg.listen_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::models::SocketAddr>(),
    )
    .await?;

    Ok(())
}

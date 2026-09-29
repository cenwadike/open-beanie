mod auth;
mod config;
mod create_routes;
mod create_workers;
mod models;
mod payment_routes;
mod payment_workers;
mod stealth_routes;
mod stealth_workers;
mod transfer_workers;
mod webhook_workers;

use axum::{
    Router,
    routing::{get, post},
};
use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use beanie_keeper::config::{EvmConfig, SolanaConfig, StarknetConfig};
use std::{collections::HashMap, sync::Arc, time::Duration};

use tower::ServiceExt;
use tower_http::services::ServeFile;

use log::{debug, info};

use crate::auth::{
    AuthState, RateLimiter, auth_finish, auth_start, register_finish, register_start,
};
use crate::models::PaymentTask;
use crate::models::{Chain, StealthTask, mpsc};
use crate::payment_routes::receive_payment;
use crate::payment_workers::run_payment_worker;
use crate::stealth_routes::execute_stealth_claim;
use crate::transfer_workers::{SharedEvmRegistry, SharedSolanaRegistry, SharedStarknetRegistry};
use crate::{config::Config, models::AppState};
use crate::{create_routes::announce_receiver, create_workers::run_announce_worker};
use crate::{models::AnnounceTask, stealth_workers::start_stealth_workers};
use tokio::sync::RwLock as AsyncRwLock;

/// Fallback route handler for serving static frontend files and pretty HTML URLs.
pub async fn serve_static(req: Request) -> Response {
    let path = req.uri().path().to_string();

    if path == "/" {
        return serve_file("public/beanie.html", Some("text/html"), req).await;
    }

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

    if !has_ext && !is_asset_dir {
        let candidate = format!("public{path}.html");
        return if tokio::fs::metadata(&candidate).await.is_ok() {
            serve_file(&candidate, Some("text/html"), req).await
        } else {
            Redirect::to("/").into_response()
        };
    }

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
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    dotenvy::dotenv().ok();

    simple_logger::init_with_level(log::Level::Info).unwrap();

    info!("[baeanie_api::main]: Starting up Beanie API");
    let cfg = Config::from_env()?;
    let starknet_cfg = StarknetConfig::from_env()?;
    let base_cfg = EvmConfig::from_env("BASE", "base")?;
    let ethereum_cfg = EvmConfig::from_env("ETHEREUM", "ethereum")?;
    let arbitrum_cfg = EvmConfig::from_env("ARBITRUM", "arbitrum")?;
    let monad_cfg = EvmConfig::from_env("MONAD", "monad")?;
    let mut solana_cfg = SolanaConfig::from_env()?;
    debug!("[baeanie_api::main]: env loaded");

    // 1. Initialize EVM Provider & Signer Clients — one per EVM chain.
    // Base's client also backs the announce/payment/stealth flows below;
    // Arbitrum's is used by both the announce worker (new) and the native
    // transfer poller further down.
    let evm_client = beanie_keeper::evm_keeper::build_client(&base_cfg).await?;
    let ethereum_client = beanie_keeper::evm_keeper::build_client(&ethereum_cfg).await?;
    let arbitrum_client = beanie_keeper::evm_keeper::build_client(&arbitrum_cfg).await?;
    let monad_client = beanie_keeper::evm_keeper::build_client(&monad_cfg).await?;

    // 2. Initialize Starknet Account Client
    let starknet_account = beanie_keeper::starknet_keeper::build_starknet_account(&starknet_cfg)?;

    // 3. Initialize Solana RPC client + keeper wallet. No separate vendor
    // client to build — solana_indexer.rs/solana_ws.rs talk to Subsquid
    // Portal directly using solana_cfg.
    let solana_rpc = beanie_keeper::solana_keeper::build_client(&solana_cfg);

    // `treasury_token_account` lives in the on-chain FactoryConfig and can't
    // come from env — SolanaConfig::from_env leaves a placeholder. Every
    // sweep (same-chain and CCTP) passes it, so read it before the config
    // is shared.
    solana_cfg.treasury_token_account = beanie_keeper::solana_keeper::fetch_treasury_token_account(
        &solana_rpc,
        &solana_cfg.factory_config,
    )
    .await?;

    let solana_keeper_wallet = solana_cfg.keeper_wallet.clone();
    let solana_usdc_mint = solana_cfg.mint;
    let solana_program_id = solana_cfg.program_id;
    let solana_cfg = Arc::new(solana_cfg);

    debug!("[baeanie_api::main]: clients loaded");

    // Every EVM-family chain the announce worker can target, each with its
    // own signer client and its own factory address.
    let evm_targets: HashMap<
        Chain,
        (
            Arc<beanie_keeper::evm_keeper::SignerProvider>,
            ethers::types::Address,
        ),
    > = HashMap::from([
        (Chain::Base, (evm_client.clone(), base_cfg.factory_address)),
        (
            Chain::Ethereum,
            (ethereum_client.clone(), ethereum_cfg.factory_address),
        ),
        (
            Chain::Arbitrum,
            (arbitrum_client.clone(), arbitrum_cfg.factory_address),
        ),
        (
            Chain::Monad,
            (monad_client.clone(), monad_cfg.factory_address),
        ),
    ]);

    let evm_payments: HashMap<Chain, (Arc<beanie_keeper::evm_keeper::SignerProvider>, EvmConfig)> =
        HashMap::from([
            (Chain::Base, (evm_client.clone(), base_cfg.clone())),
            (
                Chain::Ethereum,
                (ethereum_client.clone(), ethereum_cfg.clone()),
            ),
            (
                Chain::Arbitrum,
                (arbitrum_client.clone(), arbitrum_cfg.clone()),
            ),
            (Chain::Monad, (monad_client.clone(), monad_cfg.clone())),
        ]);

    // 4. Setup Bounded Channels and Background Workers
    let (stealth_tx, stealth_rx) = mpsc::channel::<StealthTask>(2048);
    let stealth_tx = Arc::new(stealth_tx);

    let (payment_tx, payment_rx) = mpsc::channel::<PaymentTask>(2048);
    let payment_tx = Arc::new(payment_tx);

    let (announce_tx, announce_rx) = mpsc::channel::<AnnounceTask>(2048);
    let announce_tx = Arc::new(announce_tx);

    let (webhook_tx, webhook_rx) = mpsc::channel::<crate::models::WebhookJob>(4096);
    let webhook_tx = Arc::new(webhook_tx);

    debug!("[baeanie_api::main]: channels loaded");

    let state = AppState {
        auth: Arc::new(AuthState::new(&cfg.rp_id, &cfg.rp_origin)),
        limiter: Arc::new(RateLimiter::new(
            cfg.rate_limit_per_hour,
            8,
            32,
            Duration::from_secs(3600),
        )),
        announce_tx: announce_tx.clone(),
        stealth_tx: stealth_tx.clone(),
        payment_tx: payment_tx.clone(),
        app_config: Arc::new(cfg.clone()),
        starknet_config: Arc::new(StarknetConfig::from_env()?),
        evm_config: Arc::new(beanie_keeper::config::EvmConfig::from_env("BASE", "base")?),
        solana_config: solana_cfg.clone(),
        reqwest_client: Arc::new(reqwest::Client::builder().build()?),
    };
    let worker_state = Arc::new(state.clone());
    let base_client = evm_client.clone();
    let announce_starknet_account_clone = starknet_account.clone();
    let payment_starknet_account_clone = starknet_account.clone();
    let transfer_starknet_account_clone = starknet_account.clone();
    // solana_rpc/solana_keeper_wallet are moved into the transfer poller
    // below, so the announce worker needs its own clone of each (both are
    // Arc, so this is just a refcount bump).
    let announce_solana_rpc_clone = solana_rpc.clone();
    let announce_solana_keeper_clone = solana_keeper_wallet.clone();
    let payment_solana_rpc_clone = solana_rpc.clone();
    let payment_solana_keeper_clone = solana_keeper_wallet.clone();
    let payment_solana_cfg_clone = solana_cfg.clone();

    // One announce-log registry per EVM chain, plus one each for Starknet
    // and Solana. Built here, before either worker spawns, and shared by
    // clone: the transfer poller writes to each (populated from
    // ReceiverAnnounced/ReceiverRegistered via evm_indexer.rs /
    // starknet_indexer.rs / solana_indexer.rs), the payment worker only
    // ever reads. Single writer, so no risk of the two workers'
    // registrations racing or drifting apart.
    let base_registry: SharedEvmRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));
    let ethereum_registry: SharedEvmRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));
    let arbitrum_registry: SharedEvmRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));
    let monad_registry: SharedEvmRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));
    let starknet_registry: SharedStarknetRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));
    let solana_registry: SharedSolanaRegistry = Arc::new(AsyncRwLock::new(HashMap::new()));

    // Keyed by `Chain` so the payment worker can pick the right registry
    // straight off `PaymentTask::source_chain` — same aliasing as
    // `evm_targets` above: `Chain::Ethereum` has no client of its own yet,
    // so it shares Base's registry too.
    let evm_registries: HashMap<Chain, SharedEvmRegistry> = HashMap::from([
        (Chain::Base, base_registry.clone()),
        (Chain::Ethereum, ethereum_registry.clone()),
        (Chain::Arbitrum, arbitrum_registry.clone()),
        (Chain::Monad, monad_registry.clone()),
    ]);

    let evm_chains = vec![
        (Chain::Base, base_client, Arc::new(base_cfg), base_registry),
        (
            Chain::Ethereum,
            ethereum_client,
            Arc::new(ethereum_cfg),
            ethereum_registry,
        ),
        (
            Chain::Arbitrum,
            arbitrum_client,
            Arc::new(arbitrum_cfg),
            arbitrum_registry,
        ),
        (
            Chain::Monad,
            monad_client,
            Arc::new(monad_cfg),
            monad_registry,
        ),
    ];
    let starknet_cfg_clone = state.starknet_config.clone();
    let webhook_tx_for_transfer = webhook_tx.clone();

    debug!("[baeanie_api::main]: app state loaded");

    // Spawn announce workers
    tokio::spawn(run_announce_worker(
        evm_targets,
        announce_starknet_account_clone,
        announce_solana_rpc_clone,
        announce_solana_keeper_clone,
        starknet_cfg.factory_address,
        solana_program_id,
        solana_usdc_mint,
        announce_rx,
    ));

    // Spawn stealth workers
    tokio::spawn(start_stealth_workers(worker_state, stealth_rx));

    // Spawn payment worker — reads the same registries the transfer
    // poller below writes to, instead of deriving JIT-register/sweep
    // params from the payment request itself.
    tokio::spawn(run_payment_worker(
        evm_payments,
        evm_registries,
        payment_starknet_account_clone,
        state.starknet_config.clone(),
        starknet_registry.clone(),
        payment_solana_rpc_clone,
        payment_solana_keeper_clone,
        payment_solana_cfg_clone,
        solana_registry.clone(),
        payment_rx,
        webhook_tx.clone(),
    ));

    // Spawn native transfer worker (EVM chains + Starknet + Solana).
    tokio::spawn(async move {
        crate::transfer_workers::run_native_transfer_poller(
            evm_chains,
            transfer_starknet_account_clone,
            solana_rpc,
            solana_keeper_wallet,
            starknet_cfg_clone,
            solana_cfg,
            webhook_tx_for_transfer,
            starknet_registry,
            solana_registry,
        )
        .await;
    });

    // Spawn webhook delivery worker
    let http_for_webhooks = state.reqwest_client.clone();
    tokio::spawn(async move {
        crate::webhook_workers::run_webhook_worker(http_for_webhooks, webhook_rx).await;
    });

    debug!("[baeanie_api::main]: workers loaded");

    let app = Router::new()
        .route("/api/v1/webauthn/register/start", post(register_start))
        .route("/api/v1/webauthn/register/finish", post(register_finish))
        .route("/api/v1/webauthn/auth/start", post(auth_start))
        .route("/api/v1/webauthn/auth/finish", post(auth_finish))
        .route("/api/v1/stealth/claim", post(execute_stealth_claim))
        .route("/api/v1/create", post(announce_receiver))
        .route("/api/v1/pay", post(receive_payment))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
        .fallback(serve_static);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::models::SocketAddr>(),
    )
    .await?;

    info!(
        "[baeanie_api::main]: Beanie Lanes API running on {}",
        cfg.listen_addr
    );

    Ok(())
}

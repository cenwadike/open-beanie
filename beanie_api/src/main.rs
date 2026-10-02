mod auth;
mod config;
mod create_routes;
mod create_workers;
mod models;
mod payment_routes;
mod payment_workers;
mod rpc_proxy;
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
use beanie_keeper::{
    config::{EvmConfig, SolanaConfig, StarknetConfig},
    evm_keeper::{SignerProvider, fetch_domain_separator},
};
use ethers::types::H256;
use std::{collections::HashMap, sync::Arc, time::Duration};

use tower::ServiceExt;
use tower_http::services::ServeFile;

use log::{debug, info};

use crate::auth::{
    AuthState, RateLimiter, auth_finish, auth_start, register_finish, register_start,
};
use crate::models::AnnounceTask;
use crate::models::PaymentTask;
use crate::models::{Chain, StealthTask, mpsc};
use crate::payment_routes::receive_payment;
use crate::payment_workers::run_payment_worker;
// use crate::rpc_proxy::handle;
use crate::stealth_routes::{execute_stealth_claim, list_cosigners};
use crate::stealth_workers::{
    ChainRegistry, WorkerCtx, chain_cfgs_from_env, start_stealth_workers,
};
use crate::transfer_workers::{SharedEvmRegistry, SharedSolanaRegistry, SharedStarknetRegistry};
use crate::{config::Config, models::AppState};
use crate::{create_routes::announce_receiver, create_workers::run_announce_worker};
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
    let base_client = beanie_keeper::evm_keeper::build_client(&base_cfg).await?;
    let ethereum_client = beanie_keeper::evm_keeper::build_client(&ethereum_cfg).await?;
    let arbitrum_client = beanie_keeper::evm_keeper::build_client(&arbitrum_cfg).await?;
    let monad_client = beanie_keeper::evm_keeper::build_client(&monad_cfg).await?;

    let base_evm_client_for_webhook = base_client.clone();
    let base_evm_cfg_for_webhook = Arc::new(base_cfg.clone());

    let base_domain_seperator =
        fetch_domain_separator(base_client.clone(), base_cfg.token_address).await?;
    let ethereum_domain_seperator =
        fetch_domain_separator(ethereum_client.clone(), ethereum_cfg.token_address).await?;
    let arbitrum_domain_separator =
        fetch_domain_separator(arbitrum_client.clone(), arbitrum_cfg.token_address).await?;
    // CHECK: this passes base_cfg.token_address with the Monad client. It is
    // probably meant to be monad_cfg.token_address (unchanged here because it
    // is outside the stealth path and I can't see your Monad config).
    let monad_domain_seperator =
        fetch_domain_separator(monad_client.clone(), base_cfg.token_address).await?;

    let evm_domain_separators: HashMap<Chain, H256> = HashMap::from([
        (Chain::Base, base_domain_seperator),
        (Chain::Ethereum, ethereum_domain_seperator),
        (Chain::Arbitrum, arbitrum_domain_separator),
        (Chain::Monad, monad_domain_seperator),
    ]);

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

    // 3b. Stealth chain registry. Built BEFORE AppState because the claim and
    // cosigners routes read it (allowlists, chain family, enabled_for_claims,
    // published cosigners). Cosigner keys come from the dstack KMS (or the
    // dev_env source locally). Refuses to start if STEALTH_CHAINS_JSON is
    // invalid, if a derived cosigner differs from `expected_cosigner`, if two
    // chains share a key source or cosigner address, or if an EVM RPC reports
    // the wrong chain id.
    let stealth_chains = Arc::new(ChainRegistry::build(chain_cfgs_from_env()?).await?);

    // Every EVM-family chain the announce worker can target, each with its
    // own signer client and its own factory address.
    let evm_targets: HashMap<
        Chain,
        (
            Arc<beanie_keeper::evm_keeper::SignerProvider>,
            ethers::types::Address,
        ),
    > = HashMap::from([
        (Chain::Base, (base_client.clone(), base_cfg.factory_address)),
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
            (Chain::Base, (base_client.clone(), base_cfg.clone())),
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
        starknet_config: Arc::new(StarknetConfig::from_env()?),
        solana_config: solana_cfg.clone(),
        reqwest_client: Arc::new(reqwest::Client::builder().build()?),
        evm_domain_separators: Arc::new(evm_domain_separators),
        stealth_chains: stealth_chains.clone(),
    };
    let worker_state = Arc::new(state.clone());
    let base_client = base_client.clone();
    let announce_starknet_account_clone = starknet_account.clone();
    let payment_starknet_account_clone = starknet_account.clone();
    let transfer_starknet_account_clone = starknet_account.clone();
    // solana_rpc/solana_keeper_wallet are moved into the transfer poller
    // below, so every other consumer (announce worker, payment worker,
    // stealth worker) needs its own clone of each (both are Arc, so this is
    // just a refcount bump).
    let announce_solana_rpc_clone = solana_rpc.clone();
    let announce_solana_keeper_clone = solana_keeper_wallet.clone();
    let payment_solana_rpc_clone = solana_rpc.clone();
    let payment_solana_keeper_clone = solana_keeper_wallet.clone();
    let payment_solana_cfg_clone = solana_cfg.clone();
    let stealth_solana_rpc_clone = solana_rpc.clone();
    let stealth_solana_keeper_clone = solana_keeper_wallet.clone();

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
    // straight off `PaymentTask::source_chain`.
    let evm_registries: HashMap<Chain, SharedEvmRegistry> = HashMap::from([
        (Chain::Base, base_registry.clone()),
        (Chain::Ethereum, ethereum_registry.clone()),
        (Chain::Arbitrum, arbitrum_registry.clone()),
        (Chain::Monad, monad_registry.clone()),
    ]);

    // Per-chain keeper clients the stealth worker sends through. (Base and
    // Ethereum were swapped here before: a Base claim would have been sent
    // through the Ethereum client, and the reverse.)
    let evm_clients: HashMap<Chain, Arc<SignerProvider>> = HashMap::from([
        (Chain::Base, base_client.clone()),
        (Chain::Ethereum, ethereum_client.clone()),
        (Chain::Arbitrum, arbitrum_client.clone()),
        (Chain::Monad, monad_client.clone()),
    ]);

    let evm_chains = vec![
        (
            Chain::Base,
            base_client,
            Arc::new(base_cfg.clone()),
            base_registry,
        ),
        (
            Chain::Ethereum,
            ethereum_client,
            Arc::new(ethereum_cfg.clone()),
            ethereum_registry,
        ),
        (
            Chain::Arbitrum,
            arbitrum_client,
            Arc::new(arbitrum_cfg.clone()),
            arbitrum_registry,
        ),
        (
            Chain::Monad,
            monad_client,
            Arc::new(monad_cfg.clone()),
            monad_registry,
        ),
    ];
    let starknet_cfg_clone = state.starknet_config.clone();
    let webhook_tx_for_transfer = webhook_tx.clone();

    // Initialize Upstreams for the RPC Proxy
    let mut upstreams = std::collections::HashMap::new();
    let stealth_factories: HashMap<String, String> = stealth_chains
        .public_info()
        .into_iter()
        .filter_map(|info| {
            info.factory
                .map(|factory| (info.chain.to_ascii_lowercase(), factory))
        })
        .collect();
    let evm_allowed = |chain: &str, receiver_factory: String, token: String| {
        let mut allowed = vec![receiver_factory, token];
        if let Some(factory) = stealth_factories.get(chain) {
            allowed.push(factory.clone());
        }
        allowed
    };

    // Base upstream
    let base_allowed = evm_allowed(
        "base",
        base_cfg.factory_address.to_string(),
        base_cfg.token_address.to_string(),
    );
    upstreams.insert(
        "base".to_string(),
        rpc_proxy::Upstream::with_allowed(
            base_cfg.evm_rpc_url.clone(),
            rpc_proxy::Family::Evm,
            &base_allowed,
        ),
    );

    // Ethereum upstream
    let ethereum_allowed = evm_allowed(
        "ethereum",
        ethereum_cfg.factory_address.to_string(),
        ethereum_cfg.token_address.to_string(),
    );
    upstreams.insert(
        "ethereum".to_string(),
        rpc_proxy::Upstream::with_allowed(
            ethereum_cfg.evm_rpc_url.clone(),
            rpc_proxy::Family::Evm,
            &ethereum_allowed,
        ),
    );

    // Arbitrum upstream
    let arbitrum_allowed = evm_allowed(
        "arbitrum",
        arbitrum_cfg.factory_address.to_string(),
        arbitrum_cfg.token_address.to_string(),
    );
    upstreams.insert(
        "arbitrum".to_string(),
        rpc_proxy::Upstream::with_allowed(
            arbitrum_cfg.evm_rpc_url.clone(),
            rpc_proxy::Family::Evm,
            &arbitrum_allowed,
        ),
    );

    // Monad upstream
    let monad_allowed = evm_allowed(
        "monad",
        monad_cfg.factory_address.to_string(),
        monad_cfg.token_address.to_string(),
    );
    upstreams.insert(
        "monad".to_string(),
        rpc_proxy::Upstream::with_allowed(
            monad_cfg.evm_rpc_url.clone(),
            rpc_proxy::Family::Evm,
            &monad_allowed,
        ),
    );

    // Starknet upstream
    upstreams.insert(
        "starknet".to_string(),
        rpc_proxy::Upstream::new(
            starknet_cfg.rpc_url.clone(),
            rpc_proxy::Family::Starknet,
            &[
                &starknet_cfg.clone().factory_address.to_string(),
                &starknet_cfg.clone().token_address.to_string(),
            ],
        ),
    );

    // Solana upstream
    upstreams.insert(
        "solana".into(),
        rpc_proxy::Upstream::new(solana_cfg.rpc_url.clone(), rpc_proxy::Family::Solana, &[]),
    );

    let proxy = Arc::new(rpc_proxy::RpcProxy::new(
        reqwest::Client::new(),
        upstreams,
        false, // Set to `true` if behind a reverse proxy like Nginx/Cloudflare to parse X-Forwarded-For
    ));

    debug!("app state loaded");

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

    // Spawn stealth workers (in-TEE co-sign + gasless relay). The context
    // carries the chain registry (which holds each chain's cosigner key), the
    // per-chain EVM keeper wallets and the Solana RPC/relayer.
    let stealth_ctx = Arc::new(WorkerCtx {
        state: worker_state,
        chains: stealth_chains,
        evm_clients,
        solana_rpc: stealth_solana_rpc_clone,
        solana_keeper: stealth_solana_keeper_clone,
    });
    tokio::spawn(start_stealth_workers(stealth_ctx, stealth_rx));

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
            base_evm_client_for_webhook,
            base_evm_cfg_for_webhook,
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

    debug!("workers loaded");

    let app = Router::new()
        .route("/api/v1/webauthn/register/start", post(register_start))
        .route("/api/v1/webauthn/register/finish", post(register_finish))
        .route("/api/v1/webauthn/auth/start", post(auth_start))
        .route("/api/v1/webauthn/auth/finish", post(auth_finish))
        .route("/api/v1/stealth/claim", post(execute_stealth_claim))
        .route("/api/v1/stealth/cosigners", get(list_cosigners))
        .route("/api/v1/create", post(announce_receiver))
        .route("/api/v1/pay", post(receive_payment))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
        .merge(rpc_proxy::router(proxy))
        .fallback(serve_static);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;

    info!("Beanie Lanes API running on {}", cfg.listen_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::models::SocketAddr>(),
    )
    .await?;

    Ok(())
}

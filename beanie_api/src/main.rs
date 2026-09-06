//! Beanie API Server
//!
//! This is the main entry point for the Beanie API server.
//! It provides HTTP endpoints for using with the Beanie.
//! Supports payment and stealth systems.
//!
//! # Endpoints
//!
//! - `POST /api/v1/pay` - process gasless a payment request
//! - `POST /api/v1/stealth/claim` - process a stateless stealth account claim

mod announce_workers;
mod config;
mod create_routes;
mod models;
mod payment_routes;
mod payment_workers;
mod rate_limiter;
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
use beanie_keeper::config::{EvmConfig, StarknetConfig};
use std::{sync::Arc, time::Duration};

use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::models::PaymentTask;
use crate::models::{StealthTask, mpsc};
use crate::payment_routes::receive_payment;
use crate::payment_workers::run_payment_worker;
use crate::rate_limiter::RateLimiter;
use crate::stealth_routes::execute_stealth_claim;
use crate::{announce_workers::run_announce_worker, create_routes::announce_receiver};
use crate::{config::Config, models::AppState};
use crate::{models::AnnounceTask, stealth_workers::start_stealth_workers};

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
    let starknet_cfg = StarknetConfig::from_env()?;
    let evm_cfg = EvmConfig::from_env()?;

    // 1. Initialize EVM Provider & Signer Client
    let evm_client = beanie_keeper::evm_keeper::build_client(&evm_cfg).await?;

    // 2. Initialize Starknet Account Client
    let starknet_account = beanie_keeper::starknet_keeper::build_starknet_account(&starknet_cfg)?;

    // 3. Setup Bounded Channels and Background Workers
    let (stealth_tx, stealth_rx) = mpsc::channel::<StealthTask>(2048);
    let stealth_tx = Arc::new(stealth_tx);

    let (payment_tx, payment_rx) = mpsc::channel::<PaymentTask>(2048);
    let payment_tx = Arc::new(payment_tx);

    let (announce_tx, announce_rx) = mpsc::channel::<AnnounceTask>(2048);
    let announce_tx = Arc::new(announce_tx);

    let (webhook_tx, webhook_rx) = mpsc::channel::<crate::models::WebhookJob>(4096);
    let webhook_tx = Arc::new(webhook_tx);

    let state = AppState {
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
        evm_config: Arc::new(beanie_keeper::config::EvmConfig::from_env()?),
        reqwest_client: Arc::new(reqwest::Client::builder().build()?),
    };
    let worker_state = Arc::new(state.clone());
    let announce_evm_client_clone = evm_client.clone();
    let payment_evm_client_clone = evm_client.clone();
    let transfer_evm_client_clone = evm_client.clone();
    let announce_starknet_account_clone = starknet_account.clone();
    let payment_starknet_account_clone = starknet_account.clone();
    let transfer_starknet_account_clone = starknet_account.clone();

    // Spawn announce workers
    tokio::spawn(run_announce_worker(
        announce_evm_client_clone,
        announce_starknet_account_clone,
        evm_cfg.factory_address,
        starknet_cfg.factory_address,
        announce_rx,
    ));

    // Spawn stealth workers
    tokio::spawn(start_stealth_workers(worker_state, stealth_rx));

    // Spawn payment worker
    tokio::spawn(run_payment_worker(
        payment_evm_client_clone,
        payment_starknet_account_clone,
        state.evm_config.clone(),
        state.starknet_config.clone(),
        payment_rx,
        webhook_tx.clone(),
    ));

    // Spawn native transfer worker
    let evm_cfg_clone = state.evm_config.clone();
    let starknet_cfg_clone = state.starknet_config.clone();
    tokio::spawn(async move {
        crate::transfer_workers::run_native_transfer_poller(
            transfer_evm_client_clone,
            transfer_starknet_account_clone,
            evm_cfg_clone,
            starknet_cfg_clone,
            webhook_tx.clone(),
        )
        .await;
    });

    // Spawn webhook delivery worker
    let http_for_webhooks = state.reqwest_client.clone();
    tokio::spawn(async move {
        crate::webhook_workers::run_webhook_worker(http_for_webhooks, webhook_rx).await;
    });

    let app = Router::new()
        .route("/api/v1/stealth/claim", post(execute_stealth_claim))
        .route("/api/v1/create", post(announce_receiver))
        .route("/api/v1/pay", post(receive_payment))
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

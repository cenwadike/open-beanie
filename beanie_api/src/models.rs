//! Beanie Lanes API — the only user-facing endpoint in Beanie.
//!
//! One route: POST /lanes/init. No signup, no login, no wallet connect, no
//! signed message — paste a destination address (your own wallet, or just
//! your exchange's deposit address) and get a deterministically predicted
//! receiver back immediately. Actual on-chain registration happens in the
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
//! EVM only, deliberately: Starknet's shield-in leg requires the merchant
//! to hold a real keypair to ever spend their shielded notes later, which
//! is incompatible with "no crypto knowledge required."
//!
//! Env vars: RPC_URL, FACTORY_ADDRESS, CHAIN_NAME (e.g. "BASE" — used only
//! as the cctpMintChain label on the same-chain settlement path, see
//! init_lane), KEEPER_PRIVATE_KEY, RATE_LIMIT_PER_HOUR (default 5),
//! LISTEN_ADDR (default 0.0.0.0:8080).

pub use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
pub use serde::{Deserialize, Serialize};
pub use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};
pub use tokio::sync::mpsc;

// ── 1. Data Models & API Schemas ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Base,
    Ethereum,
    Starknet,
    Solana,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployTask {
    pub chain: Chain,
    pub merchant_address: String,
    pub target_chain: Chain,
    pub webhook_url: Option<String>,
    pub attempts: u32,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct InitLaneRequest {
    pub merchant_address: String,
    pub target_chain: Chain,
    pub source_chains: Vec<Chain>,
    #[serde(default)]
    pub enable_privacy: bool,
    pub webhook_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LaneDeployment {
    pub chain: Chain,
    pub deployment_address: String,
    pub is_privacy_lane: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct InitLaneResponse {
    pub lanes: Vec<LaneDeployment>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

// ── 2. HTTP Response Cache (Idempotency Engine) ──────────────────────────────

#[derive(Clone)]
pub struct CachedHttpResponse {
    status: StatusCode,
    body: InitLaneResponse,
    created_at: Instant,
}

pub struct HttpResponseCache {
    ttl: Duration,
    responses: Mutex<HashMap<String, CachedHttpResponse>>,
}

impl HttpResponseCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            responses: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<(StatusCode, InitLaneResponse)> {
        let mut cache = self.responses.lock().unwrap();
        if let Some(entry) = cache.get(key) {
            if entry.created_at.elapsed() < self.ttl {
                return Some((entry.status, entry.body.clone()));
            }
            cache.remove(key); // Evict expired TTL entry
        }
        None
    }

    pub fn insert(&self, key: String, status: StatusCode, body: InitLaneResponse) {
        let mut cache = self.responses.lock().unwrap();
        cache.insert(
            key,
            CachedHttpResponse {
                status,
                body,
                created_at: Instant::now(),
            },
        );
    }
}

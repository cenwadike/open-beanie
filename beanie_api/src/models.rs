pub use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
pub use serde::{Deserialize, Serialize};
use std::collections::HashSet;
pub use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};
pub use tokio::sync::mpsc;

// ── 1. Data Models & API Schemas ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Chain {
    Base,
    Starknet,
    Ethereum,
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
    pub source_chains: HashSet<Chain>,
    #[serde(default)]
    pub enable_privacy: bool,
    pub webhook_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LaneDeployment {
    pub chain: Chain,
    pub address: String,
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

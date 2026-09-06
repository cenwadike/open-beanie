// create_route.rs
//
// One route. It doesn't care whether `address` is the merchant's own wallet
// (standard mode) or a client-derived stealth address (privacy mode) — in
// both cases the job is identical: prove a passkey authorized announcing
// *this* address on *this* chain, then enqueue the on-chain announce.
// The distinction between standard/stealth lives entirely on the client;
// this handler has no reason to know which one it's looking at.

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::models::{AppState, Chain, SocketAddr, err};

#[derive(Debug, Deserialize)]
pub struct AnnounceRequest {
    pub chain: Chain,
    pub address: String,
    pub verified_token: String,
}

#[derive(Debug, Serialize)]
pub struct AnnounceResponse {
    pub status: String,
    pub message: String,
}

fn parse_and_sanitize_evm_addr(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("EVM address cannot be empty");
    }
    let addr = trimmed
        .parse::<ethers::types::Address>()
        .map_err(|_| "Invalid EVM hex address format")?;
    Ok(format!("{:#x}", addr))
}

fn parse_and_sanitize_felt(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Value cannot be empty");
    }
    let felt = starknet::core::types::Felt::from_hex(trimmed)
        .map_err(|_| "Invalid Starknet Felt hex string")?;
    Ok(format!("{:#064x}", felt))
}

/// Same wire representation the client sent, whatever your `Chain` enum's
/// serde casing convention is — avoids assuming PascalCase/UPPERCASE/etc.
fn chain_tag(chain: &Chain) -> String {
    serde_json::to_value(chain)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

pub async fn announce_receiver(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<AnnounceRequest>,
) -> Response {
    // 1. Passkey verification — proves a real, verified passkey session
    //    authorized announcing exactly this chain+address, nothing else.
    let binding = format!(
        "announce:{}:{}",
        chain_tag(&payload.chain),
        payload.address.trim()
    );
    let credential_id = match state
        .auth
        .consume_verified(&payload.verified_token, &binding)
    {
        Some(id) => id,
        None => {
            return err(
                StatusCode::UNAUTHORIZED,
                "Passkey verification missing, expired, or bound to a different chain/address",
            );
        }
    };

    // 2. Canonicalize the address per chain.
    let address = match payload.chain {
        Chain::Base | Chain::Ethereum => match parse_and_sanitize_evm_addr(&payload.address) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid address: {e}")),
        },
        Chain::Starknet => match parse_and_sanitize_felt(&payload.address) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid address: {e}")),
        },
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "Unsupported chain for receiver announcement",
            );
        }
    };

    // 3. Single rate-limit call site, now against a proven credential_id.
    if let Err(msg) = state.limiter.check(addr.ip(), &address, &credential_id) {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 4. Enqueue. Nothing else — worker does the actual on-chain announce.
    let task = crate::models::AnnounceTask {
        chain: payload.chain,
        merchant_address: address,
        credential_id,
    };

    if let Err(e) = state.announce_tx.send(task).await {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("Failed to enqueue announce task: {e}"),
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(AnnounceResponse {
            status: "accepted".to_string(),
            message: "Receiver announcement queued".to_string(),
        }),
    )
        .into_response()
}

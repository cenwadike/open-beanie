use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    models::{AppState, Chain, SocketAddr, err},
    rate_limiter::PasskeyAuth,
};

#[derive(Debug, Deserialize)]
pub struct AnnounceReceiverRequest {
    pub chain: Chain,
    pub merchant_address: String,
    pub credential_id: String,
}

#[derive(Debug, Serialize)]
pub struct AnnounceReceiverResponse {
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

fn sanitize_opaque_identifier(
    input: &str,
    min_len: usize,
    max_len: usize,
) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.len() < min_len || trimmed.len() > max_len {
        return Err("Identifier string out of acceptable length bounds");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '=' || c == '+')
    {
        return Err("Identifier contains invalid characters");
    }
    Ok(trimmed.to_string())
}

/// Auth + validate, then enqueue an announce task. Nothing else.
pub async fn announce_receiver(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    auth: PasskeyAuth,
    Json(payload): Json<AnnounceReceiverRequest>,
) -> Response {
    // Passkey header must match body
    if auth.credential_id != payload.credential_id {
        return err(
            StatusCode::FORBIDDEN,
            "Passkey credential header does not match request credential ID",
        );
    }

    let credential_id = match sanitize_opaque_identifier(&payload.credential_id, 1, 512) {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid credential_id: {e}"),
            );
        }
    };

    let merchant_address = match payload.chain {
        Chain::Base | Chain::Ethereum => {
            match parse_and_sanitize_evm_addr(&payload.merchant_address) {
                Ok(v) => v,
                Err(e) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid merchant_address: {e}"),
                    );
                }
            }
        }
        Chain::Starknet => match parse_and_sanitize_felt(&payload.merchant_address) {
            Ok(v) => v,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid merchant_address: {e}"),
                );
            }
        },
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "Unsupported chain for receiver announcement",
            );
        }
    };

    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &merchant_address, &credential_id)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    let task = crate::models::AnnounceTask {
        chain: payload.chain,
        merchant_address,
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
        Json(AnnounceReceiverResponse {
            status: "accepted".to_string(),
            message: "Receiver announcement queued".to_string(),
        }),
    )
        .into_response()
}

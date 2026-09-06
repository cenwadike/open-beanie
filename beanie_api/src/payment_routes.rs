use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::models::{AppState, Chain, PaymentTask, SocketAddr, err};

#[derive(Debug, Deserialize)]
pub struct IncomingPaymentRequest {
    pub chain: Chain,
    pub merchant_address: String,
    pub receiver_address: String,
    pub destination_chain: Chain,
    pub tx_hash: String,
    pub from_address: String,
    pub amount_raw: String,
    pub webhook_url: Option<String>,
    pub signature: Option<String>,
    pub credential_id: Option<String>,
}

#[allow(unused)]
#[derive(Debug, Deserialize)]
struct StarknetSignedPayload {
    typed_data: serde_json::Value,
    signature: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct PaymentResponse {
    status: String,
    message: String,
}

pub async fn receive_payment(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<IncomingPaymentRequest>,
) -> Response {
    // Basic validation
    if payload.tx_hash.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "tx_hash is required");
    }

    if payload.receiver_address.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "receiver_address is required");
    }

    if payload.merchant_address.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "merchant_address is required");
    }

    // Rate limiting per receiver/derived address
    let cred_key = payload
        .credential_id
        .clone()
        .unwrap_or_else(|| format!("guest::${:?}", payload.receiver_address));
    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &payload.receiver_address, &cred_key)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // Signature verification: require signature and validate depending on chain
    let sig = match &payload.signature {
        Some(s) => s.clone(),
        None => return err(StatusCode::BAD_REQUEST, "signature is required"),
    };

    match payload.chain {
        Chain::Base | Chain::Ethereum => {
            let sig_bytes = match hex::decode(sig.trim_start_matches("0x")) {
                Ok(b) => b,
                Err(_) => return err(StatusCode::BAD_REQUEST, "invalid hex signature"),
            };

            if sig_bytes.len() != 65 {
                return err(
                    StatusCode::BAD_REQUEST,
                    "signature must be 65 bytes (r,s,v)",
                );
            }
        }
        Chain::Starknet => {
            // Verify signed Starknet payload structure containing typedData & signature
            let parsed: Result<StarknetSignedPayload, _> = serde_json::from_str(&sig);
            if parsed.is_err() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "invalid Starknet signed payload JSON structure",
                );
            }
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "unsupported chain for signature verification",
            );
        }
    }

    // Build task and enqueue for background processing.
    // The signature field contains either EVM hex signature or Starknet signed payload JSON.
    let task = PaymentTask {
        source_chain: payload.chain,
        destination_chain: payload.destination_chain,
        merchant_address: payload.merchant_address,
        receiver_address: payload.receiver_address,
        tx_hash: payload.tx_hash,
        from_address: payload.from_address,
        amount_raw: payload.amount_raw,
        webhook_url: payload.webhook_url,
        attempts: 0,
        create_if_missing: true,
    };

    if let Err(_) = state.payment_tx.send(task).await {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed to enqueue payment task",
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(PaymentResponse {
            status: "accepted".to_string(),
            message: "Payment queued for processing".to_string(),
        }),
    )
        .into_response()
}

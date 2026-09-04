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
    if let Err(msg) = state.limiter.check(addr.ip(), &payload.receiver_address) {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // Signature verification: require signature and validate depending on chain
    let sig = match &payload.signature {
        Some(s) => s.clone(),
        None => return err(StatusCode::BAD_REQUEST, "signature is required"),
    };

    match payload.chain {
        Chain::Base | Chain::Ethereum => {
            // EIP-191 / personal_sign style: recover signer and compare to merchant_address
            let message = format!(
                "{}|{}|{}|{}",
                payload.tx_hash,
                payload.receiver_address,
                payload.merchant_address,
                payload.amount_raw
            );
            let hash = ethers::utils::hash_message(message);
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

            use ethers::core::types::Signature as EvmSignature;
            use std::convert::TryFrom;

            let evm_sig = match EvmSignature::try_from(sig_bytes.as_slice()) {
                Ok(s) => s,
                Err(_) => return err(StatusCode::BAD_REQUEST, "invalid signature format"),
            };

            let recovered = match evm_sig.recover(hash) {
                Ok(a) => a,
                Err(_) => return err(StatusCode::BAD_REQUEST, "signature recovery failed"),
            };

            let merchant_addr = match payload.merchant_address.parse::<ethers::types::Address>() {
                Ok(a) => a,
                Err(_) => {
                    // allow merchant_address to be an arbitrary string; derive address from keccak if not parseable
                    let hash = ethers::utils::keccak256(payload.merchant_address.as_bytes());
                    ethers::types::Address::from_slice(&hash[12..32])
                }
            };

            if recovered != merchant_addr {
                return err(
                    StatusCode::UNAUTHORIZED,
                    "signature does not match merchant address",
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

    // Build task and enqueue for background processing. We set create_if_missing=true
    // so the payment worker will attempt JIT receiver creation and sweep.
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

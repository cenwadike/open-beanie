use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use starknet::core::types::Felt;

use crate::models::{AppState, SocketAddr, StealthTask, err};
use crate::{models::Chain, rate_limiter::PasskeyAuth};

// --- Structural Constraints ---
const MAX_CALLS: usize = 20;
const MAX_CALLDATA_ITEMS: usize = 256;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ClientSignature {
    pub r1: String,
    pub s1: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CallDataPayload {
    pub contract_address: String,
    pub entrypoint: String,
    pub calldata: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecuteClaimRequest {
    pub chain: Chain,
    pub tx_hash: String,
    pub derived_address: String,
    pub client_sig: ClientSignature,
    pub credential_id: String,
    pub calls: Vec<CallDataPayload>,
}

#[derive(Debug, Serialize)]
pub struct ExecuteClaimResponse {
    pub status: String,
    pub message: String,
    pub transaction_hash: String,
}

// --- Helper Sanitizers & Parsers ---

/// Helper to sanitize and validate Starknet Hex String (Felt)
fn parse_and_sanitize_felt(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Value cannot be empty");
    }
    let felt = Felt::from_hex(trimmed).map_err(|_| "Invalid Starknet Felt hex string")?;
    // Return canonical zero-padded hex representation
    Ok(format!("{:#064x}", felt))
}

/// Helper to sanitize and validate EVM Addresses
fn parse_and_sanitize_evm_addr(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("EVM address cannot be empty");
    }
    let addr = trimmed
        .parse::<ethers::types::Address>()
        .map_err(|_| "Invalid EVM hex address format")?;
    // Return canonical checksummed / lowercase string
    Ok(format!("{:#x}", addr))
}

/// Validates base64 / hex string length and character bounds for credentials/signatures
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

// --- Handler ---
pub async fn execute_stealth_claim(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    auth: PasskeyAuth, // Stateless & Pre-Rate-Limited Passkey Auth
    Json(payload): Json<ExecuteClaimRequest>,
) -> Response {
    // 0. Cross-validate Header Credential ID against JSON Payload
    if auth.credential_id != payload.credential_id {
        return err(
            StatusCode::FORBIDDEN,
            "Passkey credential header does not match task payload credential ID",
        );
    }

    // 1. Array Bound Validation (Anti-DoS)
    if payload.calls.is_empty() {
        return err(StatusCode::BAD_REQUEST, "The 'calls' array cannot be empty");
    }
    if payload.calls.len() > MAX_CALLS {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("Exceeded maximum allowed calls count ({MAX_CALLS})"),
        );
    }

    // 2. Sanitize Signatures and Credentials
    let sanitized_r1 = match sanitize_opaque_identifier(&payload.client_sig.r1, 1, 130) {
        Ok(val) => val,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid client_sig.r1: {e}"),
            );
        }
    };
    let sanitized_s1 = match sanitize_opaque_identifier(&payload.client_sig.s1, 1, 130) {
        Ok(val) => val,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid client_sig.s1: {e}"),
            );
        }
    };
    let sanitized_cred_id = match sanitize_opaque_identifier(&payload.credential_id, 1, 512) {
        Ok(val) => val,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid credential_id: {e}"),
            );
        }
    };

    // 3. Chain-Specific Address and Transaction Hash Validation
    let (sanitized_derived_addr, sanitized_tx_hash) = match payload.chain {
        Chain::Starknet => {
            let addr = match parse_and_sanitize_felt(&payload.derived_address) {
                Ok(val) => val,
                Err(e) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid derived_address: {e}"),
                    );
                }
            };
            let tx = match parse_and_sanitize_felt(&payload.tx_hash) {
                Ok(val) => val,
                Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid tx_hash: {e}")),
            };
            (addr, tx)
        }
        Chain::Base | Chain::Ethereum => {
            let addr = match parse_and_sanitize_evm_addr(&payload.derived_address) {
                Ok(val) => val,
                Err(e) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid derived_address: {e}"),
                    );
                }
            };
            let tx = match sanitize_opaque_identifier(&payload.tx_hash, 64, 66) {
                Ok(val) => val,
                Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid tx_hash: {e}")),
            };
            (addr, tx)
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "Provided chain is not supported for stealth transactions",
            );
        }
    };

    // 4. Validate and Sanitize Nested Calls
    let mut sanitized_calls = Vec::with_capacity(payload.calls.len());
    for (idx, call) in payload.calls.into_iter().enumerate() {
        if call.calldata.len() > MAX_CALLDATA_ITEMS {
            return err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Call at index {idx} exceeds maximum calldata items limit ({MAX_CALLDATA_ITEMS})"
                ),
            );
        }

        let (sanitized_contract, sanitized_entrypoint, sanitized_calldata) = match payload.chain {
            Chain::Starknet => {
                let contract = match parse_and_sanitize_felt(&call.contract_address) {
                    Ok(val) => val,
                    Err(e) => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            &format!("Invalid contract_address at call index {idx}: {e}"),
                        );
                    }
                };

                let entrypoint = call.entrypoint.trim().to_string();
                if entrypoint.is_empty() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Empty entrypoint at call index {idx}"),
                    );
                }

                let mut sanitized_cd = Vec::with_capacity(call.calldata.len());
                for (cd_idx, cd_item) in call.calldata.into_iter().enumerate() {
                    match parse_and_sanitize_felt(&cd_item) {
                        Ok(val) => sanitized_cd.push(val),
                        Err(e) => {
                            return err(
                                StatusCode::BAD_REQUEST,
                                &format!(
                                    "Invalid calldata item at call {idx}, index {cd_idx}: {e}"
                                ),
                            );
                        }
                    }
                }

                (contract, entrypoint, sanitized_cd)
            }
            Chain::Base | Chain::Ethereum => {
                let contract = match parse_and_sanitize_evm_addr(&call.contract_address) {
                    Ok(val) => val,
                    Err(e) => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            &format!("Invalid contract_address at call index {idx}: {e}"),
                        );
                    }
                };

                let entrypoint = call.entrypoint.trim().to_string();
                if entrypoint.is_empty() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Empty entrypoint at call index {idx}"),
                    );
                }

                let sanitized_cd: Vec<String> = call
                    .calldata
                    .into_iter()
                    .map(|item| item.trim().to_string())
                    .collect();

                (contract, entrypoint, sanitized_cd)
            }
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "Provided chain is not supported for stealth transactions",
                );
            }
        };

        sanitized_calls.push(CallDataPayload {
            contract_address: sanitized_contract,
            entrypoint: sanitized_entrypoint,
            calldata: sanitized_calldata,
        });
    }

    // 5. Rate Limiting Check (using canonical derived address)
    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &sanitized_derived_addr, &auth.credential_id)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 6. Build Cleaned Task Payload
    let task = StealthTask {
        chain: payload.chain,
        tx_hash: sanitized_tx_hash.clone(),
        derived_address: sanitized_derived_addr,
        client_sig: ClientSignature {
            r1: sanitized_r1,
            s1: sanitized_s1,
        },
        credential_id: sanitized_cred_id,
        calls: sanitized_calls,
    };

    // 7. Queue Execution
    if let Err(e) = state.stealth_tx.send(task).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to queue execution task: {e}"),
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(ExecuteClaimResponse {
            status: "queued".to_string(),
            message: "Transaction payload validated and queued for co-signing and gasless relay."
                .to_string(),
            transaction_hash: sanitized_tx_hash,
        }),
    )
        .into_response()
}

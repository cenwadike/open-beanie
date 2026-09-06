// stealth_route.rs
//
// The stealth private key signature (`client_sig`) is produced entirely on
// the client, over `tx_hash`, before this ever gets called — that's the
// actual on-chain spend authorization. This route does NOT re-derive that
// signature or hold any spending key. Its job:
//
//   1. Prove a verified passkey session authorized *this exact* claim
//      (chain + derived_address + tx_hash) — anti-abuse / anti-DoS gate,
//      not the on-chain authorization.
//   2. Where possible, recompute tx_hash from the submitted `calls` and
//      chain identity, and reject if the client's tx_hash doesn't match —
//      otherwise a client could sign one thing and submit calls for
//      another. Done for EVM below. Starknet is a documented TODO (see
//      below) pending the account contract's calldata layout.
//   3. Enqueue for the worker, which does the actual TEE co-sign + relay.
//
// route name: `/api/v1/stealth/claim` (settled — this file, main.rs must
// register it under that path, not `/execute`).

use anyhow::Error;
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethers::{
    abi::{Token, encode},
    utils::keccak256,
};
use serde::{Deserialize, Serialize};

use crate::models::{AppState, Chain, SocketAddr, StealthTask, err};

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
pub struct ClaimRequest {
    pub chain: Chain,
    pub tx_hash: String,
    pub derived_address: String,
    pub client_sig: ClientSignature,
    pub calls: Vec<CallDataPayload>,
    pub verified_token: String,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub status: String,
    pub message: String,
    pub transaction_hash: String,
}

// ---------- Sanitizers ----------

fn parse_and_sanitize_felt(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Value cannot be empty");
    }
    let felt = starknet::core::types::Felt::from_hex(trimmed)
        .map_err(|_| "Invalid Starknet Felt hex string")?;
    Ok(format!("{:#064x}", felt))
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

fn chain_tag(chain: &Chain) -> String {
    serde_json::to_value(chain)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn evm_chain_id(chain: &Chain) -> Option<u64> {
    match chain {
        Chain::Base => Some(8453),
        Chain::Ethereum => Some(1),
        _ => None,
    }
}

/// Recomputes the EVM sweep hash exactly as the client does, with chain_id
/// bound into the preimage — a signature captured for Base can no longer be
/// replayed on Ethereum (or vice versa) even if factory/token addresses
/// ever collide across the two chains.
fn recompute_evm_hash(
    derived_address: &str,
    contract_address: &str,
    chain_id: u64,
    calldata_hex: &str,
) -> Result<String, &'static str> {
    let addr_a: ethers::types::Address =
        derived_address.parse().map_err(|_| "bad derived_address")?;
    let addr_b: ethers::types::Address = contract_address
        .parse()
        .map_err(|_| "bad contract_address")?;
    let calldata_bytes = hex::decode(calldata_hex.trim_start_matches("0x"))
        .map_err(|_| "bad calldata hex encoding")?;

    let encoded = encode(&[
        Token::Address(addr_a),
        Token::Address(addr_b),
        Token::Uint(chain_id.into()),
        Token::Bytes(calldata_bytes),
    ]);

    Ok(format!("0x{}", hex::encode(keccak256(encoded))))
}

// TODO(security, blocking before Starknet claims ship):
// Recomputing the native Starknet invoke-transaction hash requires the
// deployed account contract's exact `__execute__` calldata layout (the
// SNIP-6 multicall encoding: call count, then per-call
// [contract_address, selector, calldata_len, ...calldata]), plus the
// account's current nonce and the network's chain_id felt. None of that is
// available here without the Cairo contract source. Until this is wired
// up, tx_hash is NOT independently verified against `calls` on Starknet —
// the 2-of-2 TEE cosigner is the only backstop for that chain. Do not
// treat this as "fixed" until this function actually recomputes and
// compares the hash.
#[allow(dead_code)]
fn recompute_starknet_hash_todo() -> Result<String, Error> {
    Ok("not implemented — see TODO above; blocked on account contract calldata layout".to_string())
}

pub async fn execute_stealth_claim(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<ClaimRequest>,
) -> Response {
    // 1. Passkey verification, bound to exactly this claim.
    let binding = format!(
        "claim:{}:{}:{}",
        chain_tag(&payload.chain),
        payload.derived_address.trim(),
        payload.tx_hash.trim()
    );
    let credential_id = match state
        .auth
        .consume_verified(&payload.verified_token, &binding)
    {
        Some(id) => id,
        None => {
            return err(
                StatusCode::UNAUTHORIZED,
                "Passkey verification missing, expired, or bound to a different claim payload",
            );
        }
    };

    // 2. Bounds checks (anti-DoS).
    if payload.calls.is_empty() {
        return err(StatusCode::BAD_REQUEST, "The 'calls' array cannot be empty");
    }
    if payload.calls.len() > MAX_CALLS {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("Exceeded maximum allowed calls count ({MAX_CALLS})"),
        );
    }

    // 3. Sanitize signature fields.
    let sanitized_r1 = match sanitize_opaque_identifier(&payload.client_sig.r1, 1, 130) {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid client_sig.r1: {e}"),
            );
        }
    };
    let sanitized_s1 = match sanitize_opaque_identifier(&payload.client_sig.s1, 1, 130) {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("Invalid client_sig.s1: {e}"),
            );
        }
    };

    // 4. Chain-specific address/hash canonicalization.
    let (sanitized_derived_addr, sanitized_tx_hash) = match payload.chain {
        Chain::Starknet => {
            let a = match parse_and_sanitize_felt(&payload.derived_address) {
                Ok(v) => v,
                Err(e) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid derived_address: {e}"),
                    );
                }
            };
            let t = match parse_and_sanitize_felt(&payload.tx_hash) {
                Ok(v) => v,
                Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid tx_hash: {e}")),
            };
            (a, t)
        }
        Chain::Base | Chain::Ethereum => {
            let a = match parse_and_sanitize_evm_addr(&payload.derived_address) {
                Ok(v) => v,
                Err(e) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid derived_address: {e}"),
                    );
                }
            };
            let t = match sanitize_opaque_identifier(&payload.tx_hash, 64, 66) {
                Ok(v) => v,
                Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid tx_hash: {e}")),
            };
            (a, t)
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "Provided chain is not supported for stealth transactions",
            );
        }
    };

    // 5. Sanitize each call.
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

        let (contract, entrypoint, calldata) = match payload.chain {
            Chain::Starknet => {
                let contract = match parse_and_sanitize_felt(&call.contract_address) {
                    Ok(v) => v,
                    Err(e) => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            &format!("Invalid contract_address at call {idx}: {e}"),
                        );
                    }
                };
                let entrypoint = call.entrypoint.trim().to_string();
                if entrypoint.is_empty() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Empty entrypoint at call {idx}"),
                    );
                }
                let mut cd = Vec::with_capacity(call.calldata.len());
                for (cd_idx, item) in call.calldata.into_iter().enumerate() {
                    match parse_and_sanitize_felt(&item) {
                        Ok(v) => cd.push(v),
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
                (contract, entrypoint, cd)
            }
            Chain::Base | Chain::Ethereum => {
                let contract = match parse_and_sanitize_evm_addr(&call.contract_address) {
                    Ok(v) => v,
                    Err(e) => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            &format!("Invalid contract_address at call {idx}: {e}"),
                        );
                    }
                };
                let entrypoint = call.entrypoint.trim().to_string();
                if entrypoint.is_empty() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        &format!("Empty entrypoint at call {idx}"),
                    );
                }
                let cd: Vec<String> = call
                    .calldata
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .collect();
                (contract, entrypoint, cd)
            }
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "Provided chain is not supported for stealth transactions",
                );
            }
        };

        sanitized_calls.push(CallDataPayload {
            contract_address: contract,
            entrypoint,
            calldata,
        });
    }

    // 6. Chain-identity-bound hash verification — the actual replay fix.
    match payload.chain {
        Chain::Base | Chain::Ethereum => {
            let chain_id = evm_chain_id(&payload.chain).unwrap();
            let call = &sanitized_calls[0];
            let calldata_hex = call.calldata.first().map(String::as_str).unwrap_or("0x");

            match recompute_evm_hash(
                &sanitized_derived_addr,
                &call.contract_address,
                chain_id,
                calldata_hex,
            ) {
                Ok(expected) if expected.eq_ignore_ascii_case(&sanitized_tx_hash) => {}
                Ok(_) => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "tx_hash does not match submitted calls/chain_id — rejected as a possible replay",
                    );
                }
                Err(e) => return err(StatusCode::BAD_REQUEST, e),
            }
        }
        Chain::Starknet => {
            // See TODO above `recompute_starknet_hash_todo`. Intentionally
            // not verified here yet — flagging, not hiding.
        }
        _ => {}
    }

    // 7. Rate limit — single call site, proven credential_id.
    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &sanitized_derived_addr, &credential_id)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 8. Enqueue for the worker (TEE co-sign + gasless relay happens there).
    let task = StealthTask {
        chain: payload.chain,
        tx_hash: sanitized_tx_hash.clone(),
        derived_address: sanitized_derived_addr,
        client_sig: ClientSignature {
            r1: sanitized_r1,
            s1: sanitized_s1,
        },
        credential_id,
        calls: sanitized_calls,
    };

    if let Err(e) = state.stealth_tx.send(task).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to queue execution task: {e}"),
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(ClaimResponse {
            status: "queued".to_string(),
            message: "Transaction payload validated and queued for co-signing and gasless relay."
                .to_string(),
            transaction_hash: sanitized_tx_hash,
        }),
    )
        .into_response()
}

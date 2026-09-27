// create_routes.rs
//
// One route, one shape, for every chain including Solana. `address` is always
// the merchant identity the client wants announced (their own wallet in
// standard mode, a client-derived stealth address in privacy mode); the
// distinction between standard/stealth lives entirely on the client and this
// handler never needs to know which one it's looking at.
//
// Every announce also names where the merchant is paid (`target_chain` +
// `target_recipient`, both mandatory, chosen by the client).
//
// Solana note: earlier drafts of this route also asked the client for a
// `solana_merchant` pubkey and a pre-signed `solana_reg_tx_hex` blob, on the
// theory that Solana's on-chain `announce_merchant(merchant, ..., receiver,
// reg_tx)` needs a `receiver` keypair's signature that only the client could
// produce. That's wrong — see `tests/solana_beanie.ts`'s `prepare()`: the
// `receiver` there is a keypair *generated on the spot, used to sign exactly
// one tx, and discarded* ("the key is unreachable after return"). Nothing
// about it is merchant-controlled or client-known; it's a disposable signer
// invented purely to satisfy the program's account-creation requirements.
// The keeper — which already pays for the nonce account, the ATAs, and the
// announce tx itself — can generate that throwaway keypair, sign the
// registration tx with it, and discard it, exactly like `prepare()` does in
// the test. That whole dance now lives in `create_workers.rs`'s Solana arm.
// So Solana needs nothing extra from the client: `address` carries the
// merchant identity here too, same as every other chain.

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
    pub lane_id: String,
    pub verified_token: String,
    /// Chain the merchant wants to be paid on. Equal to `chain` = same-chain.
    pub target_chain: Chain,
    /// The merchant's address on `target_chain` (the wallet in standard mode,
    /// the target-chain stealth address in privacy mode).
    pub target_recipient: String,
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

fn parse_and_sanitize_solana_addr(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Solana address cannot be empty");
    }
    let pubkey = trimmed
        .parse::<solana_sdk::pubkey::Pubkey>()
        .map_err(|_| "Invalid Solana base58 address")?;
    Ok(pubkey.to_string())
}

/// Per-chain address canonicalization. Arbitrum shares Base/Ethereum's EVM
/// hex-address parsing (same address format, different deployment); Solana
/// gets its own base58 pubkey parsing. No wildcard arm on purpose — adding a
/// `Chain` variant without a case here is now a compile error, not a
/// silent 400.
fn sanitize_address_for_chain(chain: Chain, input: &str) -> Result<String, &'static str> {
    match chain {
        Chain::Base | Chain::Ethereum | Chain::Arbitrum => parse_and_sanitize_evm_addr(input),
        Chain::Starknet => parse_and_sanitize_felt(input),
        Chain::Solana => parse_and_sanitize_solana_addr(input),
    }
}

pub async fn announce_receiver(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<AnnounceRequest>,
) -> Response {
    // 1. Passkey verification — proves a real, verified passkey session
    //    authorized announcing exactly this chain+address, nothing else.
    let lane_id = payload.lane_id.trim();
    if lane_id.is_empty() || lane_id.len() > 128 {
        return err(StatusCode::BAD_REQUEST, "Invalid lane_id");
    }

    let binding = format!("create-lane:{lane_id}");
    let credential_id = match state
        .auth
        .consume_verified(&payload.verified_token, &binding)
    {
        Some(id) => id,
        None => {
            return err(
                StatusCode::UNAUTHORIZED,
                "Passkey verification missing, expired, or bound to a different lane",
            );
        }
    };

    // 2. Canonicalize the address per chain (same code path for all five now).
    let address = match sanitize_address_for_chain(payload.chain, &payload.address) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("Invalid address: {e}")),
    };

    // 2b. Settlement target: must be a real address on the chosen chain.
    let target_recipient =
        match sanitize_address_for_chain(payload.target_chain, &payload.target_recipient) {
            Ok(v) => v,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid settlement address: {e}"),
                );
            }
        };

    // 3. Single rate-limit call site, now against a proven credential_id.
    if let Err(msg) = state.limiter.check(addr.ip(), &address, &credential_id) {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 4. Enqueue. Nothing else — worker does the actual on-chain announce,
    //    including (for Solana) generating and discarding its own throwaway
    //    receiver keypair. See create_workers.rs.
    let task = crate::models::AnnounceTask {
        chain: payload.chain,
        merchant_address: address,
        credential_id,
        target_chain: payload.target_chain,
        target_recipient,
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

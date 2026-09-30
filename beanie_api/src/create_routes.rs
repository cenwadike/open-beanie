// create_routes.rs
//
// One request, one proven identity (chain + address), one settlement target
// (target_chain + target_recipient) — fanned out server-side into six
// AnnounceTasks, one per supported chain:
//
//   - the target_chain leg gets merchant_address = target_recipient
//     (this is the leg sweep() pays out on when CCTP fields are zero;
//     target_recipient is the only one of the two values guaranteed to be
//     the real payout destination — address and target_recipient can
//     legitimately differ in stealth mode)
//   - every other leg gets merchant_address = address (the raw, unparsed
//     string) — each chain's worker arm (create_workers.rs) either parses
//     it natively or falls back to a per-chain derivation when it isn't
//     that chain's native format. That fallback is what lets one client
//     address create receivers across all six chains.
//
// The client still only proves ownership of ONE identity on ONE chain via
// the passkey ceremony — the other five legs are not separately owned, so
// the binding stays lane_id-only. No change to auth.rs.

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::warn;
use serde::{Deserialize, Serialize};

use crate::models::{AnnounceTask, AppState, Chain, SocketAddr, err};

/// Every chain a single announce request fans out to. Adding a `Chain`
/// variant without adding it here is caught by the exhaustive match in
/// `sanitize_address_for_chain` below (that match has no wildcard arm), so
/// this list and that match can't silently drift apart for long — but this
/// array itself is not compiler-enforced exhaustive, so bump it by hand
/// whenever `Chain` gains a variant.
const ALL_CHAINS: [Chain; 6] = [
    Chain::Base,
    Chain::Ethereum,
    Chain::Arbitrum,
    Chain::Monad,
    Chain::Starknet,
    Chain::Solana,
];

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
    pub webhook_url: Option<String>,
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

fn parse_and_sanitize_webhook_url(input: &str) -> Result<Option<String>, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let parsed = url::Url::parse(trimmed).map_err(|_| "Invalid URL format")?;

    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("Webhook URL scheme must be http or https"),
    }

    if !parsed.has_host() {
        return Err("Webhook URL must contain a valid host");
    }

    Ok(Some(parsed.to_string()))
}

/// Per-chain address canonicalization. Arbitrum and Monad share Base/
/// Ethereum's EVM hex-address parsing (same address format, different
/// deployments); Solana gets its own base58 pubkey parsing. No wildcard arm
/// on purpose — adding a `Chain` variant without a case here is now a
/// compile error, not a silent 400.
///
/// NOTE: this validates `address` and `target_recipient` as *native*
/// addresses on their own declared chain only — it is NOT run against the
/// other five fan-out legs. The other legs accept `address` as an opaque
/// string and rely on each worker arm's own native-parse-or-derive fallback
/// (see create_workers.rs). That's intentional: address is only required to
/// be valid on the one chain the client claims it for.
fn sanitize_address_for_chain(chain: Chain, input: &str) -> Result<String, &'static str> {
    match chain {
        Chain::Base | Chain::Ethereum | Chain::Arbitrum | Chain::Monad => {
            parse_and_sanitize_evm_addr(input)
        }
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
    //    Binding stays lane_id-only: the fan-out below does not need a
    //    wider binding, because the other five legs aren't separately
    //    owned identities — they're the same proven address/recipient,
    //    just re-announced per chain.
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

    // 2. Canonicalize the address per chain — native-format validation only
    //    on the two chains the client actually claimed (chain, target_chain).
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

    // 2c. Webhook URL must be valid url
    let webhook_url = match payload.webhook_url.as_deref() {
        Some(raw_url) => match parse_and_sanitize_webhook_url(raw_url) {
            Ok(url) => url,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid webhook_url: {e}"),
                );
            }
        },
        None => None,
    };

    // 3. Single rate-limit call site, against the one proven credential_id.
    //    Not looped per fan-out leg — it's still one proven identity plus
    //    one settlement target, same as before the fan-out existed.
    if let Err(msg) = state.limiter.check(addr.ip(), &address, &credential_id) {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 4. Fan out: one AnnounceTask per chain, all sharing target_chain /
    //    target_recipient. The target_chain leg carries target_recipient as
    //    its merchant_address (the guaranteed real payout destination);
    //    every other leg carries the raw, unparsed `address` string and
    //    lets that chain's worker arm derive a native identity from it if
    //    needed. CCTP route fields are NOT computed here — evm_route /
    //    starknet_route / solana_route in create_workers.rs already derive
    //    zeroed fields automatically via same_chain(leg, target_chain), so
    //    nothing extra is needed on the target leg beyond the
    //    merchant_address swap above.
    //
    //    At-least-once delivery is fine here: announceReceiver /
    //    announce_receiver is a pure event-emit on every chain, it doesn't
    //    mutate merchantReceiversMap / the receivers array, so there's no
    //    atomicity requirement across the six sends.
    let mut enqueue_failures = 0u8;
    for leg in ALL_CHAINS {
        let merchant_address = if leg == payload.target_chain {
            target_recipient.clone()
        } else {
            address.clone()
        };

        let task = AnnounceTask {
            chain: leg,
            merchant_address,
            credential_id: credential_id.clone(),
            target_chain: payload.target_chain,
            target_recipient: target_recipient.clone(),
            webhook_url: webhook_url.clone(),
        };

        if let Err(e) = state.announce_tx.send(task).await {
            // Channel-level failure only (worker gone / backpressure),
            // not a validation failure — log and keep sending the
            // remaining legs rather than aborting the whole request.
            warn!("announce_receiver: failed to enqueue leg {leg:?}: {e}");
            enqueue_failures += 1;
        }
    }

    if enqueue_failures == ALL_CHAINS.len() as u8 {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Failed to enqueue announce task on any chain",
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(AnnounceResponse {
            status: "accepted".to_string(),
            message: "Receiver announcement queued across all chains".to_string(),
        }),
    )
        .into_response()
}

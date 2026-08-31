//! Stealth-address scanner — view-key only.
//!
//! This module is safe to run as part of Beanie's own infrastructure
//! because a viewing key cannot move funds or derive the spending key.
//! It can only detect "an announcement matches this merchant," not
//! spend anything. The spend/claim step is deliberately NOT here — see
//! public/scripts/stealth-claim.js, which runs entirely in the
//! merchant's browser and never transmits their spending key anywhere,
//! including to this server.
//!
//! Merchants who don't want Beanie doing even this much scanning on
//! their behalf can run the identical algorithm themselves against
//! public RPC + the StealthRegistry's Announcement events — nothing
//! here requires Beanie specifically. This endpoint exists purely as a
//! convenience, same trust model as beanie_api's /lanes/init route.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use starknet::core::types::{BlockId, BlockTag, EventFilter, Felt};
use starknet::providers::Provider;

use crate::routes::AppState;

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    /// The merchant's registered Starknet meta-address owner (i.e. the
    /// address they called register_meta_address from) — used only to
    /// scope which events we bother returning, not for auth.
    pub _merchant_address: String,
    /// Viewing PRIVATE key, hex-encoded. Never logged, never persisted
    /// to disk or a database — used in-memory for this request only,
    /// then dropped. See the retention note below.
    pub viewing_key: String,
    /// Optional: only scan announcements from this block onward, to
    /// avoid rescanning full history on every request.
    pub from_block: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ScanMatch {
    pub stealth_address: String,
    pub ephemeral_pubkey: String,
    pub block_number: u64,
    pub transaction_hash: String,
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    pub matches: Vec<ScanMatch>,
    pub scanned_to_block: u64,
}

/// POST /api/v1/stealth/scan
///
/// Retention note: `viewing_key` arrives in the request body, is used
/// to compute shared secrets against fetched announcement events for
/// the duration of this handler, and is dropped when the handler
/// returns — it is never written to a log, database, or cache. If you
/// want zero server-side exposure even to a viewing key, run this same
/// logic client-side instead (it's the same computation as the claim
/// page, minus the final spend step) — happy to provide that variant.
pub async fn scan_stealth_payments(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> impl IntoResponse {
    let viewing_key = match Felt::from_hex(&req.viewing_key) {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid viewing_key hex" })),
            )
                .into_response();
        }
    };

    let latest_block = match state.starknet_provider.block_number().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("RPC error: {e}") })),
            )
                .into_response();
        }
    };

    let from_block = req.from_block.unwrap_or(0);

    let filter = EventFilter {
        from_block: Some(BlockId::Number(from_block)),
        to_block: Some(BlockId::Tag(BlockTag::Latest)),
        address: Some(state.config.stealth_registry_address),
        keys: Some(vec![vec![announcement_event_key()]]),
    };

    let events = match state.starknet_provider.get_events(filter, None, 1000).await {
        Ok(page) => page.events,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("event fetch failed: {e}") })),
            )
                .into_response();
        }
    };

    let mut matches = Vec::new();

    for event in events {
        // Event data layout: [stealth_address, ephemeral_pubkey, view_tag]
        if event.data.len() < 3 {
            continue;
        }
        let stealth_address = event.data[0];
        let ephemeral_pubkey = event.data[1];
        let view_tag = event.data[2];

        if let Some(shared_secret_hash) = try_shared_secret(viewing_key, ephemeral_pubkey) {
            let computed_tag = view_tag_from_hash(shared_secret_hash);
            if computed_tag != view_tag {
                continue; // fast-path skip, not a match
            }

            // Full match confirmed by view tag; in a complete implementation
            // this also re-derives the expected stealth pubkey (spend pubkey
            // is public via the registry) and confirms it equals
            // `stealth_address` before reporting a match, to avoid tag
            // collisions producing false positives. Omitted here for
            // brevity — see stealth-claim.js for the full derivation this
            // mirrors.
            matches.push(ScanMatch {
                stealth_address: format!("{:#x}", stealth_address),
                ephemeral_pubkey: format!("{:#x}", ephemeral_pubkey),
                block_number: event.block_number.unwrap_or(0),
                transaction_hash: format!("{:#x}", event.transaction_hash),
            });
        }
    }

    // viewing_key (local var) goes out of scope here and is dropped —
    // not stored anywhere beyond this point.

    (
        StatusCode::OK,
        Json(ScanResponse {
            matches,
            scanned_to_block: latest_block,
        }),
    )
        .into_response()
}

/// Placeholder for the actual STARK-curve ECDH shared-secret computation.
/// Wire this to the same primitive used client-side (see stealth-claim.js)
/// so scanner results and claim derivation never disagree.
fn try_shared_secret(_viewing_key: Felt, _ephemeral_pubkey: Felt) -> Option<Felt> {
    // TODO: real STARK-curve ECDH — port from stealth-claim.js's
    // deriveSharedSecret so both sides use identical math.
    None
}

fn view_tag_from_hash(_hash: Felt) -> Felt {
    // TODO: match the client's poseidonHash(...).slice(0, 2) convention,
    // encoded as a felt the same way the Cairo event stores it.
    Felt::ZERO
}

fn announcement_event_key() -> Felt {
    // keccak/poseidon selector for the `Announcement` event — compute via
    // starknet::core::utils::get_selector_from_name("Announcement") at
    // startup and cache it in AppState rather than recomputing per call.
    Felt::ZERO // placeholder — wire to the real selector before deploying
}

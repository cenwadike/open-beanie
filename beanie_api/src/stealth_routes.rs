// stealth_routes.rs
//
// POST /api/v1/stealth/claim
// GET  /api/v1/stealth/cosigners
//
// The stealth private key signature (`client_sig`) is produced entirely on the
// client, over the chain's signing hash, before this is called. That is the
// actual on-chain spend authorization. This route holds no spending key.
// Its job:
//
//   1. Prove a verified passkey session authorized *this exact* claim
//      (chain + derived_address + tx_hash): anti-abuse gate, not the on-chain
//      authorization.
//   2. Canonicalize inputs by chain FAMILY (from the registry, not a hardcoded
//      `match chain`).
//   3. Run `stealth_workers::precheck`, the same function the worker runs. It
//      recomputes the signing hash from the submitted data (EVM EIP-3009
//      digest, Starknet invoke-v3 hash, Solana sha256(message)), enforces
//      allowlists, and verifies the client signature where it can, so a bad
//      claim gets a 400 now instead of a silent worker failure later.
//   4. Rate limit, then enqueue for the worker (in-TEE co-sign + relay).
//
// What `tx_hash` means per family (dictated by the account contracts):
//   EVM       EIP-712 digest D of the USDC TransferWithAuthorization. The
//             client signs EIP-191(D). No `calls`: one claim = one
//             authorization, described by `auth3009`.
//   Starknet  native INVOKE V3 transaction hash.
//   Solana    sha256(message_bytes).
//
// GET /api/v1/stealth/cosigners publishes the cosigner each chain's accounts
// bind to. Clients must PIN the expected cosigner out of band; this endpoint
// is for discovery and monitoring, not trust.

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use solana_sdk::{pubkey::Pubkey, signature::Signer};
use starknet::core::types::Felt;
use std::str::FromStr;

use crate::models::{AppState, Chain, SocketAddr, StealthTask, err, mpsc};
use crate::stealth_workers::{CosignerInfo, FamilyRt, precheck};

const MAX_CALLS: usize = 20;
const MAX_CALLDATA_ITEMS: usize = 256;
const MAX_WORD_HEX: usize = 66; // 0x + 64
const MAX_SOLANA_MESSAGE: usize = 1232;

// ---------- Request types (shared with the worker) ----------

/// Untagged so the existing client payload `{ "r1": .., "s1": .. }` still parses.
/// `v` is optional: if the client sends none (or a hardcoded 27) the worker
/// resolves the parity in preflight.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum ClientSignature {
    Ecdsa {
        r1: String,
        s1: String,
        #[serde(default)]
        v: Option<u8>,
    },
    Ed25519 {
        sig_hex: String,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CallDataPayload {
    pub contract_address: String,
    pub entrypoint: String,
    pub calldata: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ResourceBoundParam {
    pub max_amount: String,
    pub max_price_per_unit: String,
}

fn zero() -> String {
    "0".to_string()
}

/// Starknet only: everything the invoke-v3 hash depends on besides the calls.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StarknetTxParams {
    /// STARK pubkey (felt) the account was created with.
    pub client_pubkey: String,
    /// Salt used for the counterfactual address (deployer = 0, via UDC).
    pub deploy_salt: String,
    pub nonce: String,
    #[serde(default = "zero")]
    pub tip: String,
    pub l1_gas: ResourceBoundParam,
    pub l2_gas: ResourceBoundParam,
    pub l1_data_gas: ResourceBoundParam,
}

/// EVM only (USDC EIP-3009): parameters signed by client for TransferWithAuthorization.
/// The cosigner is deliberately NOT a request field: the worker always uses its
/// own, and the account address must match factory.getAddress(client, ours, salt).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Auth3009Params {
    pub client: String,
    pub to: String,
    pub value: String,
    pub valid_after: String,
    pub valid_before: String,
    pub nonce: String,
    pub salt: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaimRequest {
    pub chain: Chain,
    pub tx_hash: String,
    pub derived_address: String,
    pub client_sig: ClientSignature,
    /// Starknet: the calls. EVM and Solana: ignored (send `[]`).
    #[serde(default)]
    pub calls: Vec<CallDataPayload>,
    pub verified_token: String,
    #[serde(default)]
    pub auth3009: Option<Auth3009Params>,
    #[serde(default)]
    pub starknet: Option<StarknetTxParams>,
    /// Solana: hex of the canonically serialized legacy Message.
    #[serde(default)]
    pub message_bytes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub status: String,
    pub message: String,
    pub transaction_hash: String,
}

#[derive(Debug, Serialize)]
pub struct CosignersResponse {
    /// One entry per configured chain: algo, cosigner address/pubkey, factory.
    /// Public data only.
    pub cosigners: Vec<CosignerInfo>,
    /// 64-byte hex. The `report_data` a TEE quote for this app should commit to
    /// (sha256 of the canonical cosigner list || 32 zero bytes), so a client
    /// holding a quote can check "these exact cosigners belong to this attested
    /// app". Not a substitute for pinning.
    pub report_data: String,
}

// ---------- Sanitizers ----------

fn bad(msg: impl AsRef<str>) -> Response {
    err(StatusCode::BAD_REQUEST, msg.as_ref())
}

fn cap(name: &str, s: &str, max: usize) -> Result<(), String> {
    if s.len() > max {
        return Err(format!("{name} is too long"));
    }
    Ok(())
}

fn is_hex_word(s: &str) -> bool {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    !t.is_empty() && t.len() <= 64 && t.chars().all(|c| c.is_ascii_hexdigit())
}

fn sanitize_client_sig(sig: &ClientSignature) -> Result<ClientSignature, String> {
    match sig {
        ClientSignature::Ecdsa { r1, s1, v } => {
            if !is_hex_word(r1) {
                return Err("Invalid client_sig.r1: expected up to 64 hex chars".into());
            }
            if !is_hex_word(s1) {
                return Err("Invalid client_sig.s1: expected up to 64 hex chars".into());
            }
            Ok(ClientSignature::Ecdsa {
                r1: r1.trim().to_string(),
                s1: s1.trim().to_string(),
                v: *v,
            })
        }
        ClientSignature::Ed25519 { sig_hex } => {
            let s = sig_hex.trim();
            if s.len() != 128 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err("Invalid client_sig.sig_hex: expected 128 hex chars".into());
            }
            Ok(ClientSignature::Ed25519 {
                sig_hex: s.to_string(),
            })
        }
    }
}

fn parse_and_sanitize_felt(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Value cannot be empty");
    }
    let felt = Felt::from_hex(trimmed).map_err(|_| "Invalid Starknet Felt hex string")?;
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

/// Strict 32-byte hex -> canonical lowercase `0x` + 64 hex.
fn canon_hash32(input: &str) -> Result<String, &'static str> {
    let t = input.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if t.len() != 64 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("expected a 32-byte hex hash");
    }
    Ok(format!("0x{}", t.to_lowercase()))
}

struct Canon {
    derived: String,
    tx_hash: String,
    calls: Vec<CallDataPayload>,
    message_bytes: Option<Vec<u8>>,
}

/// Starknet only. EVM and Solana claims carry no `calls`.
fn canonicalize_starknet_calls(
    calls: Vec<CallDataPayload>,
) -> Result<Vec<CallDataPayload>, String> {
    let mut out = Vec::with_capacity(calls.len());
    for (idx, call) in calls.into_iter().enumerate() {
        if call.calldata.len() > MAX_CALLDATA_ITEMS {
            return Err(format!(
                "Call at index {idx} exceeds maximum calldata items limit ({MAX_CALLDATA_ITEMS})"
            ));
        }
        let entrypoint = call.entrypoint.trim().to_string();
        if entrypoint.is_empty() {
            return Err(format!("Empty entrypoint at call {idx}"));
        }
        let contract = parse_and_sanitize_felt(&call.contract_address)
            .map_err(|e| format!("Invalid contract_address at call {idx}: {e}"))?;
        let mut cd = Vec::with_capacity(call.calldata.len());
        for (cd_idx, item) in call.calldata.iter().enumerate() {
            cd.push(parse_and_sanitize_felt(item).map_err(|e| {
                format!("Invalid calldata item at call {idx}, index {cd_idx}: {e}")
            })?);
        }
        out.push(CallDataPayload {
            contract_address: contract,
            entrypoint,
            calldata: cd,
        });
    }
    Ok(out)
}

fn canonicalize(
    fam: &FamilyRt,
    derived: &str,
    tx_hash: &str,
    calls: Vec<CallDataPayload>,
    message_hex: Option<&str>,
) -> Result<Canon, String> {
    match fam {
        FamilyRt::Evm(_) => {
            // One claim = one EIP-3009 authorization (see `auth3009`). The
            // worker's precheck rejects non-empty `calls`, so reject it here
            // with a clear message instead.
            if !calls.is_empty() {
                return Err("EVM claims carry no `calls`; send `auth3009`".into());
            }
            Ok(Canon {
                derived: parse_and_sanitize_evm_addr(derived)
                    .map_err(|e| format!("Invalid derived_address: {e}"))?,
                tx_hash: canon_hash32(tx_hash).map_err(|e| format!("Invalid tx_hash: {e}"))?,
                calls: vec![],
                message_bytes: None,
            })
        }
        FamilyRt::Starknet(_) => {
            if calls.is_empty() || calls.len() > MAX_CALLS {
                return Err(format!("`calls` must contain 1..={MAX_CALLS} items"));
            }
            Ok(Canon {
                derived: parse_and_sanitize_felt(derived)
                    .map_err(|e| format!("Invalid derived_address: {e}"))?,
                tx_hash: parse_and_sanitize_felt(tx_hash)
                    .map_err(|e| format!("Invalid tx_hash: {e}"))?,
                calls: canonicalize_starknet_calls(calls)?,
                message_bytes: None,
            })
        }
        FamilyRt::Solana => {
            if !calls.is_empty() {
                return Err("Solana claims carry no `calls`; send message_bytes".into());
            }
            let derived = Pubkey::from_str(derived.trim())
                .map_err(|_| "Invalid derived_address: not a base58 pubkey".to_string())?
                .to_string();
            let hex_str = message_hex.ok_or("message_bytes is required for Solana")?;
            cap("message_bytes", hex_str, 2 + 2 * MAX_SOLANA_MESSAGE)?;
            let t = hex_str.trim();
            let t = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            let bytes = hex::decode(t).map_err(|_| "message_bytes is not valid hex".to_string())?;
            Ok(Canon {
                derived,
                tx_hash: canon_hash32(tx_hash).map_err(|e| format!("Invalid tx_hash: {e}"))?,
                calls: vec![],
                message_bytes: Some(bytes),
            })
        }
    }
}

fn check_param_sizes(
    req_auth3009: &Option<Auth3009Params>,
    sn: &Option<StarknetTxParams>,
) -> Result<(), String> {
    if let Some(u) = req_auth3009 {
        cap("auth3009.client", &u.client, MAX_WORD_HEX)?;
        cap("auth3009.to", &u.to, MAX_WORD_HEX)?;
        cap("auth3009.value", &u.value, MAX_WORD_HEX)?;
        cap("auth3009.valid_after", &u.valid_after, MAX_WORD_HEX)?;
        cap("auth3009.valid_before", &u.valid_before, MAX_WORD_HEX)?;
        cap("auth3009.nonce", &u.nonce, MAX_WORD_HEX)?;
        cap("auth3009.salt", &u.salt, MAX_WORD_HEX)?;
    }
    if let Some(s) = sn {
        for (n, v) in [
            ("starknet.client_pubkey", &s.client_pubkey),
            ("starknet.deploy_salt", &s.deploy_salt),
            ("starknet.nonce", &s.nonce),
            ("starknet.tip", &s.tip),
            ("starknet.l1_gas.max_amount", &s.l1_gas.max_amount),
            (
                "starknet.l1_gas.max_price_per_unit",
                &s.l1_gas.max_price_per_unit,
            ),
            ("starknet.l2_gas.max_amount", &s.l2_gas.max_amount),
            (
                "starknet.l2_gas.max_price_per_unit",
                &s.l2_gas.max_price_per_unit,
            ),
            ("starknet.l1_data_gas.max_amount", &s.l1_data_gas.max_amount),
            (
                "starknet.l1_data_gas.max_price_per_unit",
                &s.l1_data_gas.max_price_per_unit,
            ),
        ] {
            cap(n, v, MAX_WORD_HEX)?;
        }
    }
    Ok(())
}

fn chain_tag(chain: &Chain) -> String {
    serde_json::to_value(chain)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------- GET /api/v1/stealth/cosigners ----------

/// Publishes the cosigner each chain's accounts will be bound to. Because the
/// cosigner is part of the account address (EVM: factory.getAddress(client,
/// cosigner, salt); Starknet: constructor args; Solana: derive_multisig), a
/// client can verify every address it derives.
///
/// PINNING: clients MUST ship the expected cosigner per chain (hardcoded or
/// published in docs) and refuse to create an account if this response differs.
/// A cosigner fetched only from this endpoint proves nothing against whoever
/// controls the API: they could return their own key and hold funds hostage.
pub async fn list_cosigners(State(state): State<AppState>) -> Response {
    let report_data = match state.stealth_chains.attestation_report_data() {
        Ok(rd) => hex::encode(rd),
        Err(_) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not compute cosigner commitment",
            );
        }
    };
    Json(CosignersResponse {
        cosigners: state.stealth_chains.public_info(),
        report_data,
    })
    .into_response()
}

// ---------- POST /api/v1/stealth/claim ----------

pub async fn execute_stealth_claim(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<ClaimRequest>,
) -> Response {
    let ClaimRequest {
        chain,
        tx_hash,
        derived_address,
        client_sig,
        calls,
        verified_token,
        starknet,
        message_bytes,
        auth3009,
    } = payload;

    // 1. Passkey verification, bound to exactly this claim (raw, trimmed values).
    let binding = format!(
        "claim:{}:{}:{}",
        chain_tag(&chain),
        derived_address.trim(),
        tx_hash.trim()
    );
    let credential_id = match state.auth.consume_verified(&verified_token, &binding) {
        Some(id) => id,
        None => {
            return err(
                StatusCode::UNAUTHORIZED,
                "Passkey verification missing, expired, or bound to a different claim payload",
            );
        }
    };

    // 2. Chain resolution from the registry.
    let rt = match state.stealth_chains.get(chain) {
        Ok(rt) => rt,
        Err(_) => {
            return bad("Provided chain is not supported for stealth transactions");
        }
    };
    if !rt.cfg.enabled_for_claims {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Claims are currently disabled for this chain",
        );
    }

    // 3. Bounds + signature shape (anti-DoS).
    if calls.len() > MAX_CALLS {
        return bad(format!(
            "Exceeded maximum allowed calls count ({MAX_CALLS})"
        ));
    }
    if let Err(e) = check_param_sizes(&auth3009, &starknet) {
        return bad(e);
    }
    let sanitized_sig = match sanitize_client_sig(&client_sig) {
        Ok(s) => s,
        Err(e) => return bad(e),
    };

    // 4. Canonicalize by family.
    let canon = match canonicalize(
        &rt.fam,
        &derived_address,
        &tx_hash,
        calls,
        message_bytes.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => return bad(e),
    };

    // 5. Build the task and run the worker's own screening on it.
    let task = StealthTask {
        chain,
        tx_hash: canon.tx_hash.clone(),
        derived_address: canon.derived.clone(),
        client_sig: sanitized_sig,
        credential_id: credential_id.clone(),
        calls: canon.calls,
        auth3009: match rt.fam {
            FamilyRt::Evm(_) => auth3009,
            _ => None,
        },
        starknet: match rt.fam {
            FamilyRt::Starknet(_) => starknet,
            _ => None,
        },
        message_bytes: canon.message_bytes,
    };

    let relayer = state.solana_config.keeper_wallet.pubkey();
    if let Err(e) = precheck(rt, &relayer, &task) {
        return bad(format!("Claim rejected: {e:#}"));
    }

    // 6. Rate limit: single call site, proven credential_id.
    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &canon.derived, &credential_id)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    // 7. Enqueue for the worker (in-TEE co-sign + relay happens there).
    if let Err(e) = state.stealth_tx.try_send(task) {
        return match e {
            mpsc::error::TrySendError::Full(_) => err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Claim queue is full, retry shortly",
            ),
            mpsc::error::TrySendError::Closed(_) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Claim worker is not running",
            ),
        };
    }

    (
        StatusCode::ACCEPTED,
        Json(ClaimResponse {
            status: "queued".to_string(),
            message: "Claim validated and queued for co-signing and gasless relay.".to_string(),
            transaction_hash: canon.tx_hash,
        }),
    )
        .into_response()
}

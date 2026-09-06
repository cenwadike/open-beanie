use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethers::abi::{Token, encode};
use ethers::types::{Address, H256, Signature, U256};
use ethers::utils::keccak256;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::OutsideExecutionDto;
use crate::models::{AppState, Chain, EvmAuth, PaymentTask, SocketAddr, StarknetAuth, err};

const BASE_CHAIN_ID: u64 = 8453;
const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const STARKNET_USDC: &str = "0x033068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb";

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

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SignaturePayload {
    Evm {
        from: String,
        to: String,
        value: String,
        #[serde(rename = "validAfter")]
        valid_after: u64,
        #[serde(rename = "validBefore")]
        valid_before: u64,
        nonce: String,
        signature: String,
    },
    Starknet {
        #[serde(rename = "outsideExecution")]
        outside_execution: OutsideExecutionDto,
        signature: Vec<String>, // felt hex strings
        #[serde(rename = "userAddress")]
        user_address: String,
    },
}

#[derive(Debug, Serialize)]
struct PaymentResponse {
    status: String,
    message: String,
}

fn domain_separator_base_usdc() -> H256 {
    let typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let encoded = encode(&[
        Token::FixedBytes(typehash.to_vec()),
        Token::FixedBytes(keccak256(b"USD Coin").to_vec()),
        Token::FixedBytes(keccak256(b"2").to_vec()),
        Token::Uint(U256::from(BASE_CHAIN_ID)),
        Token::Address(Address::from_str(BASE_USDC).expect("valid USDC address")),
    ]);
    H256::from(keccak256(encoded))
}

fn transfer_auth_digest(
    from: Address,
    to: Address,
    value: U256,
    valid_after: u64,
    valid_before: u64,
    nonce: H256,
) -> H256 {
    let typehash = keccak256(
        b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
    );
    let struct_hash = keccak256(encode(&[
        Token::FixedBytes(typehash.to_vec()),
        Token::Address(from),
        Token::Address(to),
        Token::Uint(value),
        Token::Uint(U256::from(valid_after)),
        Token::Uint(U256::from(valid_before)),
        Token::FixedBytes(nonce.as_bytes().to_vec()),
    ]));
    let mut buf = vec![0x19u8, 0x01u8];
    buf.extend_from_slice(domain_separator_base_usdc().as_bytes());
    buf.extend_from_slice(&struct_hash);
    H256::from(keccak256(buf))
}

/// Full cryptographic verification: recovers the signer and binds the signed
/// amount/receiver/expiry to what the request actually claims.
fn verify_evm_authorization(
    payload: &IncomingPaymentRequest,
    from: &str,
    to: &str,
    value: &str,
    valid_after: u64,
    valid_before: u64,
    nonce_hex: &str,
    sig_hex: &str,
) -> Result<EvmAuth, &'static str> {
    let from_addr = Address::from_str(from).map_err(|_| "bad from address")?;
    let to_addr = Address::from_str(to).map_err(|_| "bad to address")?;
    let receiver_addr =
        Address::from_str(&payload.receiver_address).map_err(|_| "bad receiver_address")?;

    if to_addr != receiver_addr {
        return Err("signed 'to' does not match receiver_address");
    }
    if from.to_lowercase() != payload.from_address.to_lowercase() {
        return Err("signer does not match from_address");
    }

    let value_u256 = U256::from_dec_str(value).map_err(|_| "bad value")?;
    let claimed_amount = U256::from_dec_str(&payload.amount_raw).map_err(|_| "bad amount_raw")?;
    if value_u256 != claimed_amount {
        return Err("signed value does not match amount_raw");
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now < valid_after || now > valid_before {
        return Err("authorization expired or not yet valid");
    }

    let nonce_bytes = hex::decode(nonce_hex.trim_start_matches("0x")).map_err(|_| "bad nonce")?;
    if nonce_bytes.len() != 32 {
        return Err("nonce must be 32 bytes");
    }
    let nonce = H256::from_slice(&nonce_bytes);

    let digest = transfer_auth_digest(
        from_addr,
        to_addr,
        value_u256,
        valid_after,
        valid_before,
        nonce,
    );
    let signature = Signature::from_str(sig_hex.trim_start_matches("0x"))
        .map_err(|_| "bad signature encoding")?;
    let recovered = signature
        .recover(digest)
        .map_err(|_| "signature recovery failed")?;
    if recovered != from_addr {
        return Err("signature does not authorize this transfer");
    }

    Ok(EvmAuth {
        nonce,
        valid_after,
        valid_before,
        signature: sig_hex.to_string(),
    })
}

fn verify_starknet_outside_execution(
    oe: &OutsideExecutionDto,
    keeper_address: &str,
    receiver_address: &str,
    amount_raw: &str,
) -> Result<(), &'static str> {
    if !oe.caller.eq_ignore_ascii_case(keeper_address) {
        return Err("outside execution caller is not Beanie's relayer — refusing to submit");
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now < oe.execute_after || now > oe.execute_before {
        return Err("outside execution window is not currently valid");
    }
    if oe.execute_before.saturating_sub(oe.execute_after) > 3600 {
        return Err("validity window too wide"); // cap griefing/replay surface
    }

    if oe.calls.len() != 1 {
        return Err("expected exactly one call");
    }
    let call = &oe.calls[0];
    if !call.contract_address.eq_ignore_ascii_case(STARKNET_USDC) {
        return Err("call does not target USDC");
    }
    if call.entrypoint != "transfer" {
        return Err("call is not a transfer");
    }
    if call.calldata.len() < 3 || !call.calldata[0].eq_ignore_ascii_case(receiver_address) {
        return Err("call does not send to the claimed receiver");
    }

    let amount = u128::from_str_radix(amount_raw, 10).map_err(|_| "bad amount_raw")?;
    let expected_low = amount.to_string();
    if call.calldata[1] != expected_low || call.calldata[2] != "0" {
        return Err("call amount does not match amount_raw");
    }

    Ok(())
}

pub async fn receive_payment(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<IncomingPaymentRequest>,
) -> Response {
    if payload.tx_hash.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "tx_hash is required");
    }
    if payload.receiver_address.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "receiver_address is required");
    }
    if payload.merchant_address.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "merchant_address is required");
    }
    if payload.amount_raw.trim().is_empty() || payload.amount_raw.parse::<u128>().is_err() {
        return err(
            StatusCode::BAD_REQUEST,
            "amount_raw must be a non-negative integer",
        );
    }

    let cred_key = format!("{}::{}", payload.from_address, payload.receiver_address);
    if let Err(msg) = state
        .limiter
        .check(addr.ip(), &payload.receiver_address, &cred_key)
    {
        return err(StatusCode::TOO_MANY_REQUESTS, msg);
    }

    let sig = match &payload.signature {
        Some(s) => s.clone(),
        None => return err(StatusCode::BAD_REQUEST, "signature is required"),
    };
    let parsed: SignaturePayload = match serde_json::from_str(&sig) {
        Ok(p) => p,
        Err(_) => return err(StatusCode::BAD_REQUEST, "malformed signature payload"),
    };

    let (evm_auth, starknet_auth) = match (payload.chain, &parsed) {
        (
            Chain::Base | Chain::Ethereum,
            SignaturePayload::Evm {
                from,
                to,
                value,
                valid_after,
                valid_before,
                nonce,
                signature,
            },
        ) => {
            match verify_evm_authorization(
                &payload,
                from,
                to,
                value,
                *valid_after,
                *valid_before,
                nonce,
                signature,
            ) {
                Ok(auth) => (Some(auth), None),
                Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
            }
        }
        (
            Chain::Starknet,
            SignaturePayload::Starknet {
                outside_execution,
                signature,
                user_address,
            },
        ) => {
            if !user_address.eq_ignore_ascii_case(&payload.from_address) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "signer does not match from_address",
                );
            }

            let keeper_address = &state.starknet_config.keeper_address;

            if let Err(msg) = verify_starknet_outside_execution(
                outside_execution,
                &keeper_address.to_string(),
                &payload.receiver_address,
                &payload.amount_raw,
            ) {
                return err(StatusCode::BAD_REQUEST, msg);
            }

            (
                None,
                Some(StarknetAuth {
                    outside_execution: outside_execution.clone(),
                    signature: signature.clone(),
                    user_address: user_address.clone(),
                }),
            )
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "signature type does not match chain",
            );
        }
    };

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
        evm_auth,
        starknet_auth,
    };

    if state.payment_tx.send(task).await.is_err() {
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

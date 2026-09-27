use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ethers::abi::{Token, encode};
use ethers::types::{Address, H256, Signature, U256};
use ethers::utils::keccak256;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    message::Message as SolanaMessage, pubkey::Pubkey as SolanaPubkey,
    signature::Signature as SolanaSignature, signature::Signer as SolanaSigner,
};
use spl_associated_token_account::get_associated_token_address;
use spl_token::instruction::TokenInstruction;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::OutsideExecutionDto;
use crate::models::{
    AppState, Chain, EvmAuth, PaymentTask, SocketAddr, SolanaAuth, StarknetAuth, err,
};

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
    Solana {
        /// Base64 bincode-serialized `solana_sdk::message::Message`, as
        /// compiled client-side with Beanie's keeper as fee payer.
        message: String,
        /// Base64 ed25519 signature over `message`.
        signature: String,
        /// Base58 pubkey of the payer / transfer authority.
        owner: String,
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

/// Verifies a payer's gasless SPL-token transfer authorization for Solana.
///
/// Mirrors `verify_evm_authorization`/`verify_starknet_outside_execution`:
/// the client compiles (but does not send) a `solana_sdk::message::Message`
/// with Beanie's keeper set as fee payer, containing exactly one SPL Token
/// `Transfer`/`TransferChecked` instruction moving `amount_raw` from the
/// payer's own USDC ATA to `receiver_address`, and signs it. The keeper
/// never rebuilds this message — it only checks it says what the request
/// claims, then later co-signs the *exact same bytes* as fee payer.
///
/// This function does not submit anything or touch the network; the worker
/// (`payment_workers.rs`) reconstructs the transaction from the returned
/// `SolanaAuth`, adds the keeper's signature, and broadcasts it. Landing
/// the deposit in `receiver_address` is as far as this flow goes — sweeping
/// it on to the merchant is handled by the same indexer/poller-driven
/// `sweep` path that already covers ordinary (non-gasless) Solana deposits,
/// so there's no separate sweep step to verify or enqueue here.
fn verify_solana_authorization(
    payload: &IncomingPaymentRequest,
    solana_cfg: &beanie_keeper::config::SolanaConfig,
    message_b64: &str,
    signature_b64: &str,
    owner: &str,
) -> Result<SolanaAuth, &'static str> {
    if owner != payload.from_address {
        return Err("signer does not match from_address");
    }

    let owner_pk = SolanaPubkey::from_str(owner).map_err(|_| "bad owner pubkey")?;
    let receiver_pk =
        SolanaPubkey::from_str(&payload.receiver_address).map_err(|_| "bad receiver_address")?;
    let claimed_amount: u64 = payload
        .amount_raw
        .parse()
        .map_err(|_| "amount_raw does not fit a Solana token amount (u64)")?;

    let message_bytes = BASE64
        .decode(message_b64)
        .map_err(|_| "bad message encoding")?;
    let message: SolanaMessage =
        bincode::deserialize(&message_bytes).map_err(|_| "malformed solana message")?;

    // Fee payer is always account index 0. It must be Beanie's keeper —
    // the whole point is the payer never holds or spends SOL.
    let fee_payer = message
        .account_keys
        .first()
        .ok_or("message has no accounts")?;
    if *fee_payer != solana_cfg.keeper_wallet.pubkey() {
        return Err("fee payer is not Beanie's relayer — refusing to submit");
    }

    if message.instructions.len() != 1 {
        return Err("expected exactly one instruction");
    }
    let ix = &message.instructions[0];
    let ix_program = message
        .account_keys
        .get(ix.program_id_index as usize)
        .ok_or("bad program id index")?;
    if *ix_program != spl_token::ID {
        return Err("instruction does not target the SPL Token program");
    }

    let token_ix =
        TokenInstruction::unpack(&ix.data).map_err(|_| "unparseable token instruction")?;

    // account index layout differs slightly between the two instruction
    // kinds; resolve both to (source, destination, authority, amount, mint).
    let (source_i, dest_i, authority_i, amount, mint_i) = match token_ix {
        TokenInstruction::TransferChecked { amount, .. } => (
            ix.accounts.first().copied(),
            ix.accounts.get(2).copied(),
            ix.accounts.get(3).copied(),
            amount,
            ix.accounts.get(1).copied(),
        ),
        TokenInstruction::Transfer { amount } => (
            ix.accounts.first().copied(),
            ix.accounts.get(1).copied(),
            ix.accounts.get(2).copied(),
            amount,
            None,
        ),
        _ => return Err("instruction is not a token transfer"),
    };

    if amount != claimed_amount {
        return Err("signed amount does not match amount_raw");
    }

    let account_at = |idx: Option<u8>| -> Result<&SolanaPubkey, &'static str> {
        message
            .account_keys
            .get(idx.ok_or("instruction missing an expected account")? as usize)
            .ok_or("account index out of range")
    };

    let source = account_at(source_i)?;
    let destination = account_at(dest_i)?;
    let authority = account_at(authority_i)?;

    if *authority != owner_pk {
        return Err("transfer authority does not match from_address");
    }
    let expected_source = get_associated_token_address(&owner_pk, &solana_cfg.mint);
    if *source != expected_source {
        return Err("source token account is not the signer's USDC account");
    }
    if *destination != receiver_pk {
        return Err("destination does not match receiver_address");
    }
    if let Some(mint_i) = mint_i {
        let mint = account_at(Some(mint_i))?;
        if *mint != solana_cfg.mint {
            return Err("mint does not match configured USDC");
        }
    }

    let sig_bytes = BASE64
        .decode(signature_b64)
        .map_err(|_| "bad signature encoding")?;
    let signature =
        SolanaSignature::try_from(sig_bytes.as_slice()).map_err(|_| "bad signature bytes")?;
    if !signature.verify(owner_pk.as_ref(), &message_bytes) {
        return Err("signature does not authorize this transfer");
    }

    Ok(SolanaAuth {
        message: message_b64.to_string(),
        signature: signature_b64.to_string(),
        owner: owner.to_string(),
    })
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

    let (evm_auth, starknet_auth, solana_auth) = match (payload.chain, &parsed) {
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
                Ok(auth) => (Some(auth), None, None),
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
                None,
            )
        }
        (
            Chain::Solana,
            SignaturePayload::Solana {
                message,
                signature,
                owner,
            },
        ) => {
            match verify_solana_authorization(
                &payload,
                &state.solana_config,
                message,
                signature,
                owner,
            ) {
                Ok(auth) => (None, None, Some(auth)),
                Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
            }
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
        solana_auth,
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

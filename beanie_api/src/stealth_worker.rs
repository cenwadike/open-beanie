use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use ethers::signers::Signer;
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Address, Bytes, TransactionRequest};
use serde::{Deserialize, Serialize};
use starknet::accounts::ConnectedAccount;
use starknet::{
    accounts::Account,
    core::types::{Call, Felt},
    core::utils::get_selector_from_name,
    providers::Provider,
};
use tokio::sync::mpsc;

use crate::models::{AppState, Chain, StealthTask};

#[derive(Debug, Serialize)]
struct LitJsParams {
    pub chain: String,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "derivedAddress")]
    pub derived_address: String,
    #[serde(rename = "clientSig")]
    pub client_sig: ClientSignaturePayload,
    #[serde(rename = "credentialId")]
    pub credential_id: String,
}

#[derive(Debug, Serialize)]
struct ClientSignaturePayload {
    pub r1: String,
    pub s1: String,
}

#[derive(Debug, Serialize)]
struct LitRelayRequest {
    #[serde(rename = "ipfsCid")]
    pub ipfs_cid: String,
    #[serde(rename = "jsParams")]
    pub js_params: LitJsParams,
}

#[derive(Debug, Deserialize)]
struct LitCosignResponse {
    pub r2: String,
    pub s2: String,
    pub v2: Option<u8>,
}

pub async fn start_stealth_workers(state: Arc<AppState>, mut rx: mpsc::Receiver<StealthTask>) {
    println!("[worker] Gasless paymaster worker running...");

    while let Some(task) = rx.recv().await {
        let task_hash = task.tx_hash.clone();

        let res = match task.chain {
            Chain::Starknet => process_starknet_task(&state, task).await,
            Chain::Base | Chain::Ethereum => process_evm_task(&state, task).await,
            _ => Err(anyhow!(
                "Unsupported chain for stealth execution: {:?}",
                task.chain
            )),
        };

        match res {
            Ok(tx_hash) => {
                println!(
                    "[worker] Execution submitted successfully. Hash: {}",
                    tx_hash
                );
            }
            Err(e) => {
                eprintln!("[worker] Execution failed for tx {}: {:#}", task_hash, e);
            }
        }
    }
}

async fn process_starknet_task(state: &AppState, task: StealthTask) -> Result<String> {
    // 1. Obtain Lit TEE co-signature (r2, s2)
    let cosign_data = fetch_lit_cosignature(state, &task).await?;

    // 2. Assemble 4-element signature vector [r1, s1, r2, s2] expected by StealthAccount.cairo
    let full_signature = vec![
        Felt::from_hex(&task.client_sig.r1)?,
        Felt::from_hex(&task.client_sig.s1)?,
        Felt::from_hex(&cosign_data.r2)?,
        Felt::from_hex(&cosign_data.s2)?,
    ];

    // 3. Reconstruct execution calls from payload
    let mut calls = Vec::with_capacity(task.calls.len());
    for call_item in task.calls {
        let mut calldata_felts = Vec::with_capacity(call_item.calldata.len());
        for cd in call_item.calldata {
            calldata_felts.push(Felt::from_hex(&cd)?);
        }

        calls.push(Call {
            to: Felt::from_hex(&call_item.contract_address)?,
            selector: get_selector_from_name(&call_item.entrypoint)?,
            calldata: calldata_felts,
        });
    }

    // 4. Paymaster prepares and broadcasts transaction payload with 2-of-2 custom signature
    let starknet_config = &state.starknet_config;
    let account = beanie_keeper::starknet_keeper::build_starknet_account(starknet_config)?;

    let prepared = account
        .execute_v3(calls)
        .prepared()
        .map_err(|e| anyhow::anyhow!("Failed to prepare Starknet V3 transaction: {e}"))?;

    let mut invoke_v3 = prepared
        .get_invoke_request(false, true)
        .await
        .map_err(|_| anyhow!("Failed to derive invoke transaction payload"))?;

    // Attach client + TEE 2-of-2 signature array
    invoke_v3.signature = full_signature;

    let res = account
        .provider()
        .add_invoke_transaction(invoke_v3)
        .await
        .context("Paymaster failed to relay execution")?;

    Ok(format!("{:#x}", res.transaction_hash))
}

async fn process_evm_task(state: &AppState, task: StealthTask) -> Result<String> {
    // 1. Obtain Lit TEE co-signature (r2, s2)
    let cosign_data = fetch_lit_cosignature(state, &task).await?;

    // 2. Pack 130-byte 2-of-2 signature [65-byte Client Sig | 65-byte TEE Sig] expected by StealthAccount.sol
    let client_sig_bytes = pack_65byte_evm_signature(&task.client_sig.r1, &task.client_sig.s1, 27)?;
    let cosigner_sig_bytes = pack_65byte_evm_signature(
        &cosign_data.r2,
        &cosign_data.s2,
        cosign_data.v2.unwrap_or(27),
    )?;

    let mut full_130byte_signature = Vec::with_capacity(130);
    full_130byte_signature.extend_from_slice(&client_sig_bytes);
    full_130byte_signature.extend_from_slice(&cosigner_sig_bytes);

    // 3. Reconstruct execution payload
    let call = &task.calls[0];
    let to_addr: Address = call.contract_address.parse()?;

    let mut calldata_bytes = hex::decode(call.calldata.join("").trim_start_matches("0x"))?;
    calldata_bytes.extend_from_slice(&full_130byte_signature);

    let tx = TransactionRequest::new()
        .to(to_addr)
        .data(Bytes::from(calldata_bytes));

    let typed_tx: TypedTransaction = tx.into();

    // 4. Paymaster signs gas fee payment wrapper
    let signature = state
        .evm_config
        .keeper_wallet
        .sign_transaction(&typed_tx)
        .await?;

    Ok(format!("0x{}", hex::encode(signature.to_vec())))
}

/// Packs ECDSA r, s, and v into a 65-byte slice required by StealthAccount.sol
fn pack_65byte_evm_signature(r_hex: &str, s_hex: &str, v: u8) -> Result<Vec<u8>> {
    let mut r_bytes = hex::decode(r_hex.trim_start_matches("0x"))?;
    let mut s_bytes = hex::decode(s_hex.trim_start_matches("0x"))?;

    if r_bytes.len() < 32 {
        let mut padded = vec![0u8; 32 - r_bytes.len()];
        padded.extend(r_bytes);
        r_bytes = padded;
    }
    if s_bytes.len() < 32 {
        let mut padded = vec![0u8; 32 - s_bytes.len()];
        padded.extend(s_bytes);
        s_bytes = padded;
    }

    let v_normalized = if v < 27 { v + 27 } else { v };

    let mut sig = Vec::with_capacity(65);
    sig.extend_from_slice(&r_bytes[..32]);
    sig.extend_from_slice(&s_bytes[..32]);
    sig.push(v_normalized);

    Ok(sig)
}

async fn fetch_lit_cosignature(state: &AppState, task: &StealthTask) -> Result<LitCosignResponse> {
    let chain_str = match task.chain {
        Chain::Starknet => "starknet",
        Chain::Base => "base",
        Chain::Ethereum => "ethereum",
        Chain::Solana => "solana",
    };

    let lit_payload = LitRelayRequest {
        ipfs_cid: state.app_config.lit_action_ipfs_cid.clone(),
        js_params: LitJsParams {
            chain: chain_str.to_string(),
            tx_hash: task.tx_hash.clone(),
            derived_address: task.derived_address.clone(),
            client_sig: ClientSignaturePayload {
                r1: task.client_sig.r1.clone(),
                s1: task.client_sig.s1.clone(),
            },
            credential_id: task.credential_id.clone(),
        },
    };

    let lit_res = state
        .reqwest_client
        .post(&state.app_config.lit_relay_url)
        .bearer_auth(state.app_config.lit_api_key.clone())
        .json(&lit_payload)
        .send()
        .await
        .context("Failed calling Lit TEE co-signer")?;

    if !lit_res.status().is_success() {
        let err_text = lit_res.text().await.unwrap_or_default();
        return Err(anyhow!("Lit TEE execution rejected: {err_text}"));
    }

    lit_res
        .json::<LitCosignResponse>()
        .await
        .context("Failed parsing Lit co-signer response")
}

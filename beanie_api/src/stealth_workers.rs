// ============================================================================
// stealth_workers.rs  (dstack/Phala: cosigner key derived inside the TEE)
//
// One "round" = one StealthTask dequeued and driven to a confirmed tx:
//   1 resolve   chain registry lookup, claims enabled?
//   2 screen    `precheck`: allowlists, hash recompute, client-sig check.
//               Pure and sync. The route runs the SAME function, so a claim
//               the worker would reject is rejected with a 400 instead.
//   3 cosign    sign with the chain's own in-process cosigner key (lives only
//               in the TEE's encrypted RAM); VERIFY the result
//   4 assemble  build the 2-of-2 signature / transaction
//   5 preflight simulate (bad sigs fail here, before fees)
//   6 relay     keeper submits, we wait for on-chain success
//
// Cosigner keys (trust model): each provider runs its OWN worker as a dstack
// app (confidential VM). Key material comes from the dstack KMS, bound to the
// app's identity and a per-chain `path`, so the same app gets the same key on
// every restart and on any host, and no human ever holds it. Which code may
// run as that app is controlled by the provider's own dstack authorization
// (their IaC / on-chain allowlist); that is the provider's security posture
// and the one remaining authority, outside this file.
//
// The final key is derived: sha256(domain || algo || path || kms_material),
// so we rely only on the KMS returning stable secret bytes. The cosigner
// ADDRESS is derived from the key, never configured. `expected_cosigner` (the
// address clients pinned) turns any drift (app identity change, wrong path)
// into a refusal to boot, instead of silently moving to a new address.
//
// The factory is immutable, has no owner, and takes the cosigner PER ACCOUNT:
// the account address commits to (entryPoint, client, cosigner, salt). One
// factory per chain therefore serves every provider; there is nothing to pin
// at the factory. The binding check is
//     factory.getAddress(client, OUR_COSIGNER, salt) == from
//
// What each chain's contract forces (read from StealthAccount.sol/.cairo):
//
//  EVM  (USDC EIP-3009, NO ERC-4337). The stealth account holds only USDC and
//       never pays gas. The client signs a USDC `TransferWithAuthorization`
//       and the keeper submits it, so there is no paymaster and no approve.
//         D      = EIP-712 digest of TransferWithAuthorization (USDC domain).
//                  `tx_hash` for EVM IS D.
//         signed = EIP-191(D). StealthAccount.isValidSignature wraps the hash
//                  it receives in the EIP-191 prefix, so BOTH signers sign
//                  hash_message(D), never raw D.
//         sig    = client65 || cosigner65 (130 bytes), passed to the `bytes`
//                  overload of transferWithAuthorization (FiatTokenV2_2).
//         tx     = Multicall3.aggregate3([factory.createAccount(client,
//                  cosigner, salt) (only if the account has no code yet),
//                  usdc.transferWithAuthorization]).
//       USDC verifies via ERC-1271, which needs code, so createAccount goes
//       first in the same atomic batch. `to`, `value` and `nonce` are inside
//       what is signed, so a relayer cannot redirect funds, and replay is
//       blocked on-chain by USDC's authorizationState. One claim = one
//       authorization.
//       Sends go through the shared `SignerProvider` (NonceManagerMiddleware),
//       the same client the payment/sweep workers use, so nonces never drift.
//
//  Starknet  (SNIP-6 account). Verifies over the native V3 invoke tx hash:
//       [r1, s1, r2_lo, r2_hi, s2_lo, s2_hi, v2]. The worker recomputes that
//       hash (poseidon) from the calls + client-chosen nonce/resource bounds,
//       so the cosigner can never be made to sign a hash that does not match
//       the screened calls. The stealth account pays its own fee in STRK, and
//       it may not exist yet, so the keeper (1) deploys it through the UDC
//       (unique=false => deployer 0 => same address as a DEPLOY_ACCOUNT) and
//       (2) tops up the STRK shortfall, in ONE multicall, before the claim.
//
//  Solana  2-of-2 SPL multisig, created lazily. Unchanged design.
//
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anchor_lang::solana_program::system_instruction;
use anyhow::{Context, Result, anyhow, bail, ensure};
use beanie_keeper::evm_keeper::{Call3, MULTICALL3_ADDRESS, Multicall3, SignerProvider};
use beanie_keeper::starknet_keeper::build_starknet_account;
use dstack_sdk::dstack_client::DstackClient;
use ethers::abi::{Token, encode};
use ethers::contract::abigen;
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer as EthSigner};
use ethers::types::{
    Address, Bytes, Eip1559TransactionRequest, H256, Signature as EthSignature, U256,
};
use ethers::utils::{hash_message, keccak256};
use log::{error, info, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::CommitmentConfig;
use solana_sdk::{
    message::Message as SolanaMessage,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signature as SolanaSignature, Signer as SolSigner, keypair_from_seed},
    transaction::Transaction as SolanaTransaction,
};
use spl_token::instruction::TokenInstruction;
use starknet::accounts::{Account, ConnectedAccount};
use starknet::core::crypto::{Signature as StarkSignature, ecdsa_verify};
use starknet::core::types::{
    BlockId, BlockTag, BroadcastedInvokeTransactionV3, Call as StarknetCall, DataAvailabilityMode,
    ExecutionResult, Felt, FunctionCall, ResourceBounds, ResourceBoundsMapping, StarknetError,
};
use starknet::core::utils::{get_contract_address, get_selector_from_name};
use starknet::providers::{Provider as StarknetProvider, ProviderError};
use starknet_crypto::poseidon_hash_many;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::models::{AppState, Chain, StealthTask};
use crate::stealth_routes::{Auth3009Params, ClientSignature, StarknetTxParams};

const MAX_CONCURRENT_ROUNDS: usize = 8;
const ROUND_TIMEOUT: Duration = Duration::from_secs(300);
const DEDUPE_TTL: Duration = Duration::from_secs(3600);
const EVM_RECEIPT_POLL: Duration = Duration::from_secs(2);
const EVM_RECEIPT_ATTEMPTS: usize = 90;
const STARK_POLL_INTERVAL: Duration = Duration::from_secs(3);
const STARK_POLL_ATTEMPTS: usize = 60;

/// Cairo's `signature_from_vrs(v, r, s)` sets `y_parity = (v % 2 == 0)`, i.e. it
/// expects Ethereum-style v (27 => even y, 28 => odd y). Sending raw 0/1 would
/// INVERT the parity. So v2 = 27 + recid. Confidence: from corelib source as
/// remembered, not re-read. Confirm once on devnet (a wrong value fails closed:
/// the recovered address will not match and __validate__ rejects).
const STARKNET_V_OFFSET: u8 = 27;

const MAX_CALLS: usize = 20;
const MAX_SOLANA_MESSAGE: usize = 1232;
const MULTISIG_SEED_DOMAIN: &[u8] = b"beanie-multisig-v1";

/// Domain separation + dstack `purpose` for cosigner key derivation. Changing
/// this changes every cosigner address: never change it after launch.
const KEY_PURPOSE: &str = "beanie-cosigner-v1";

/// An EVM authorization must still be valid this long after the worker starts
/// on it: cosigning and inclusion both take time.
const MIN_AUTH_TTL_SECS: u64 = 120;

// Starknet resource names inside the V3 fee-fields hash.
const NAME_L1_GAS: u64 = 0x4c31_5f47_4153; // "L1_GAS"
const NAME_L2_GAS: u64 = 0x4c32_5f47_4153; // "L2_GAS"
const NAME_L1_DATA: u64 = 0x004c_315f_4441_5441; // "L1_DATA"

// ============================================================================
// Chain registry (config-driven; add/remove chains without touching logic)
// ============================================================================

fn default_multicall3() -> String {
    MULTICALL3_ADDRESS.to_string()
}

fn default_max_auth_window_secs() -> u64 {
    86_400
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainFamily {
    Evm {
        chain_id: u64,
        /// Used for the startup checks only. Runtime traffic goes through the
        /// shared keeper `SignerProvider`.
        rpc_url: String,
        /// StealthAccountFactory (immutable, no owner; takes the cosigner per
        /// account, so one factory per chain serves every provider).
        factory: String,
        /// The EIP-3009 token (Circle USDC, FiatTokenV2_2 with the `bytes`
        /// signature overload). Must also be listed in `allowed_targets`.
        usdc: String,
        /// EIP-712 domain of that token. Per chain: do NOT assume "USD Coin"/"2".
        /// Checked against the token's DOMAIN_SEPARATOR() at startup.
        domain_name: String,
        domain_version: String,
        /// Defaults to the canonical Multicall3 address.
        #[serde(default = "default_multicall3")]
        multicall3: String,
        /// Optional cap on one claim, in token base units (decimal or 0x-hex).
        #[serde(default)]
        max_value: Option<String>,
        /// Cap on (validBefore - now) for a claim.
        #[serde(default = "default_max_auth_window_secs")]
        max_auth_window_secs: u64,
    },
    Starknet {
        /// Chain id felt, e.g. "0x534e5f4d41494e" (SN_MAIN).
        chain_id: String,
        /// StealthAccount class hash.
        account_class_hash: String,
        /// Universal Deployer Contract.
        udc_address: String,
        /// STRK token address (V3 fees are paid in STRK).
        fee_token: String,
        /// Hard cap on sum(max_amount * max_price_per_unit), in fri, as a string.
        max_fee_fri: String,
    },
    Solana,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigAlgo {
    EcdsaSecp256k1,
    Ed25519,
}

/// Where a chain's cosigner key material comes from.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySourceCfg {
    /// Material from the dstack KMS, bound to THIS app's identity. `path`
    /// separates chains: same app + same path => same key, always.
    Dstack { path: String },
    /// Local development only. Refused unless ALLOW_DEV_KEYS=1. `env` is the
    /// NAME of an env var holding arbitrary secret text.
    DevEnv { env: String },
}

fn yes() -> bool {
    true
}

/// One entry per chain, loaded from env (STEALTH_CHAINS_JSON). Example:
/// [{"chain":"BASE",
///   "family":{"evm":{"chain_id":8453,"rpc_url":"https://...",
///                    "factory":"0x<StealthAccountFactory>",
///                    "usdc":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
///                    "domain_name":"USD Coin","domain_version":"2"}},
///   "key_source":{"dstack":{"path":"beanie/cosigner/base"}},
///   "expected_cosigner":"0x<published cosigner>",
///   "algo":"ecdsa_secp256k1",
///   "allowed_targets":["0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]},
///  {"chain":"STARKNET",
///   "family":{"starknet":{"chain_id":"0x534e5f4d41494e","account_class_hash":"0x..",
///     "udc_address":"0x041a78e741e5af2fec34b695679bc6891742439f7afb8484ecd7766661ad02bf",
///     "fee_token":"0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d",
///     "max_fee_fri":"200000000000000000"}},
///   "key_source":{"dstack":{"path":"beanie/cosigner/starknet"}},
///   "algo":"ecdsa_secp256k1",
///   "allowed_targets":["0x<token>","0x<pool>"],"allowed_entrypoints":["transfer","approve"]},
///  {"chain":"SOLANA","family":"solana",
///   "key_source":{"dstack":{"path":"beanie/cosigner/solana"}},
///   "algo":"ed25519",
///   "allowed_targets":["<usdc mint base58>"]}]
///
/// Never put a key in this JSON.
#[derive(Clone, Debug, Deserialize)]
pub struct ChainCfg {
    pub chain: Chain,
    pub family: ChainFamily,
    /// Where the cosigner key material comes from (dstack KMS in production).
    pub key_source: KeySourceCfg,
    /// STRONGLY RECOMMENDED. The cosigner address/pubkey you published and
    /// clients pinned. Boot is refused if the derived one differs (app
    /// identity change, wrong path, changed KEY_PURPOSE), because accounts
    /// bind to the old address and a silent change would strand funds.
    #[serde(default)]
    pub expected_cosigner: Option<String>,
    pub algo: SigAlgo,
    /// EVM: the USDC contract (must equal `family.evm.usdc`). Starknet: token +
    /// pool contracts. Solana: mints (base58).
    pub allowed_targets: HashSet<String>,
    /// Starknet: REQUIRED non-empty (entrypoint names a claim may call).
    /// EVM/Solana: ignored.
    #[serde(default)]
    pub allowed_entrypoints: HashSet<String>,
    /// Never set false while lanes exist on this chain: funds would strand.
    #[serde(default = "yes")]
    pub enabled_for_claims: bool,
}

pub fn chain_cfgs_from_env() -> Result<Vec<ChainCfg>> {
    let raw = match std::env::var("STEALTH_CHAINS_PATH") {
        Ok(path) => std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
        Err(_) => std::env::var("STEALTH_CHAINS_JSON")
            .context("set STEALTH_CHAINS_PATH or STEALTH_CHAINS_JSON")?,
    };
    serde_json::from_str(&raw).context("chain config is not valid JSON")
}

#[derive(Clone, Copy, Debug)]
pub enum CosignerId {
    Eth(Address),
    Ed25519(Pubkey),
}

/// The cosigner's private key, held in process memory only (the TEE's
/// encrypted RAM). Never logged, never serialized.
pub enum CosignerKey {
    Secp256k1(LocalWallet),
    Ed25519(Keypair),
}

impl CosignerKey {
    fn id(&self) -> CosignerId {
        match self {
            CosignerKey::Secp256k1(w) => CosignerId::Eth(w.address()),
            CosignerKey::Ed25519(k) => CosignerId::Ed25519(k.pubkey()),
        }
    }
}

/// Pure and deterministic: (algo, label, KMS material) -> 32-byte key.
/// Hashing means we depend only on the KMS returning stable secret bytes, not
/// on how it encodes them. Lengths are prefixed so fields cannot bleed into
/// each other. The counter covers the negligible chance of an invalid scalar.
fn derive_key_bytes(algo: SigAlgo, label: &str, material: &[u8]) -> Result<[u8; 32]> {
    for ctr in 0u8..=255 {
        let mut h = Sha256::new();
        h.update(KEY_PURPOSE.as_bytes());
        h.update([algo as u8]);
        h.update((label.len() as u32).to_be_bytes());
        h.update(label.as_bytes());
        h.update((material.len() as u32).to_be_bytes());
        h.update(material);
        h.update([ctr]);
        let out: [u8; 32] = h.finalize().into();
        match algo {
            SigAlgo::Ed25519 => return Ok(out),
            SigAlgo::EcdsaSecp256k1 => {
                if LocalWallet::from_bytes(&out).is_ok() {
                    return Ok(out);
                }
            }
        }
    }
    bail!("no valid key derived")
}

fn key_from_bytes(algo: SigAlgo, b: &[u8; 32]) -> Result<CosignerKey> {
    match algo {
        SigAlgo::EcdsaSecp256k1 => Ok(CosignerKey::Secp256k1(
            LocalWallet::from_bytes(b).map_err(|e| anyhow!("invalid secp256k1 key: {e}"))?,
        )),
        SigAlgo::Ed25519 => Ok(CosignerKey::Ed25519(
            keypair_from_seed(b).map_err(|e| anyhow!("invalid ed25519 seed: {e}"))?,
        )),
    }
}

/// Fetches the key material for one chain and derives its cosigner key.
/// dstack calls assume `DstackClient::new(None)` (default /var/run/dstack.sock)
/// and `get_key(path, purpose)` returning a response with a `key` string.
/// Verify against the SDK version you pin.
async fn load_cosigner_key(cfg: &ChainCfg) -> Result<CosignerKey> {
    let (label, material): (String, Vec<u8>) = match &cfg.key_source {
        KeySourceCfg::Dstack { path } => {
            let client = DstackClient::new(None);
            let resp = client
                .get_key(Some(path.clone()), Some(KEY_PURPOSE.to_string()))
                .await
                .map_err(|e| anyhow!("dstack get_key({path}) failed: {e}"))?;
            ensure!(
                !resp.key.is_empty(),
                "dstack returned an empty key for {path}"
            );
            (path.clone(), resp.key.into_bytes())
        }
        KeySourceCfg::DevEnv { env } => {
            ensure!(
                std::env::var("ALLOW_DEV_KEYS").as_deref() == Ok("1"),
                "dev_env key source refused: set ALLOW_DEV_KEYS=1 (never in production)"
            );
            let v = std::env::var(env).with_context(|| format!("env var {env} is not set"))?;
            ensure!(!v.trim().is_empty(), "env var {env} is empty");
            (env.clone(), v.trim().as_bytes().to_vec())
        }
    };
    let bytes = derive_key_bytes(cfg.algo, &label, &material)?;
    key_from_bytes(cfg.algo, &bytes)
}

pub struct EvmRt {
    pub factory: Address,
    pub usdc: Address,
    pub multicall3: Address,
    /// Built locally from (domain_name, domain_version, chain_id, usdc) and
    /// verified against the token's DOMAIN_SEPARATOR() at startup.
    pub domain_separator: [u8; 32],
    pub max_value: Option<U256>,
    pub max_auth_window_secs: u64,
}

pub struct StarknetRt {
    pub chain_id: Felt,
    pub class_hash: Felt,
    pub udc: Felt,
    pub fee_token: Felt,
    pub max_fee: u128,
}

pub enum FamilyRt {
    Evm(EvmRt),
    Starknet(StarknetRt),
    Solana,
}

pub struct ChainRuntime {
    pub cfg: ChainCfg,
    /// Public identity, derived from `key` at startup.
    pub cosigner: CosignerId,
    /// The private key. Only `cosign_*` touch it.
    key: CosignerKey,
    pub fam: FamilyRt,
    /// Serializes the Starknet keeper's deploy/top-up nonce use per chain.
    /// (EVM sends go through the shared NonceManager-backed SignerProvider.)
    pub nonce_lock: Mutex<()>,
}

pub struct ChainRegistry {
    chains: HashMap<Chain, ChainRuntime>,
}

fn norm_set(s: &HashSet<String>) -> HashSet<String> {
    s.iter().map(|t| norm_target(t)).collect()
}

/// Public cosigner identity for one chain. Contains no secret.
#[derive(serde::Serialize, Debug)]
pub struct CosignerInfo {
    pub chain: String,
    pub algo: &'static str,
    pub cosigner: String,
    pub factory: Option<String>,
}

impl ChainRegistry {
    /// Loads each cosigner key from its source, derives its address, and
    /// refuses to start if two chains share a key source or a cosigner
    /// address, if a derived cosigner differs from `expected_cosigner`, or if
    /// the config is too loose to be safe.
    pub async fn build(cfgs: Vec<ChainCfg>) -> Result<Self> {
        ensure!(!cfgs.is_empty(), "STEALTH_CHAINS_JSON has no chains");
        let mut chains = HashMap::new();
        let mut source_owner: HashMap<String, Chain> = HashMap::new();
        let mut addr_owner: HashMap<String, Chain> = HashMap::new();

        for mut cfg in cfgs {
            let chain = cfg.chain;
            ensure!(
                !chains.contains_key(&chain),
                "duplicate config for {chain:?}"
            );
            match (&cfg.family, cfg.algo) {
                (ChainFamily::Solana, SigAlgo::Ed25519) => {}
                (
                    ChainFamily::Evm { .. } | ChainFamily::Starknet { .. },
                    SigAlgo::EcdsaSecp256k1,
                ) => {}
                _ => bail!("{chain:?}: algo does not match family"),
            }
            ensure!(
                !cfg.allowed_targets.is_empty(),
                "{chain:?}: allowed_targets must not be empty"
            );
            let source_id = format!("{:?}", cfg.key_source);
            if let Some(other) = source_owner.insert(source_id.clone(), chain) {
                bail!(
                    "key source {source_id} used by {other:?} and {chain:?}: one key per chain required"
                );
            }

            let key = load_cosigner_key(&cfg)
                .await
                .with_context(|| format!("loading cosigner key for {chain:?}"))?;
            let cosigner = key.id();
            let addr_key = match cosigner {
                CosignerId::Eth(a) => format!("{a:?}"),
                CosignerId::Ed25519(p) => p.to_string(),
            };
            if let Some(want) = &cfg.expected_cosigner {
                ensure!(
                    norm_target(want) == norm_target(&addr_key),
                    "{chain:?}: derived cosigner {addr_key} != expected_cosigner {want}. \
                     App identity, key path or KEY_PURPOSE changed? Do NOT proceed: \
                     existing accounts bind to the old address."
                );
            } else {
                warn!(
                    "{chain:?}: no expected_cosigner set; publish {addr_key} and pin it in config"
                );
            }
            if let Some(other) = addr_owner.insert(addr_key.clone(), chain) {
                bail!("cosigner {addr_key} shared by {other:?} and {chain:?}");
            }

            cfg.allowed_targets = norm_set(&cfg.allowed_targets);

            let fam = match &cfg.family {
                ChainFamily::Evm { .. } => FamilyRt::Evm(
                    build_evm_rt(chain, &cfg, cosigner)
                        .await
                        .with_context(|| format!("{chain:?}: evm startup checks failed"))?,
                ),
                ChainFamily::Starknet {
                    chain_id,
                    account_class_hash,
                    udc_address,
                    fee_token,
                    max_fee_fri,
                } => {
                    ensure!(
                        !cfg.allowed_entrypoints.is_empty(),
                        "{chain:?}: allowed_entrypoints must not be empty for Starknet"
                    );
                    FamilyRt::Starknet(StarknetRt {
                        chain_id: Felt::from_hex(chain_id).context("bad starknet chain_id")?,
                        class_hash: Felt::from_hex(account_class_hash)
                            .context("bad account_class_hash")?,
                        udc: Felt::from_hex(udc_address).context("bad udc_address")?,
                        fee_token: Felt::from_hex(fee_token).context("bad fee_token")?,
                        max_fee: parse_u128(max_fee_fri).context("bad max_fee_fri")?,
                    })
                }
                ChainFamily::Solana => FamilyRt::Solana,
            };
            info!("chain {chain:?} ready, cosigner {addr_key}");
            chains.insert(
                chain,
                ChainRuntime {
                    cfg,
                    cosigner,
                    key,
                    fam,
                    nonce_lock: Mutex::new(()),
                },
            );
        }
        Ok(Self { chains })
    }

    pub fn get(&self, chain: Chain) -> Result<&ChainRuntime> {
        self.chains
            .get(&chain)
            .ok_or_else(|| anyhow!("{chain:?} is not configured"))
    }

    pub fn len(&self) -> usize {
        self.chains.len()
    }

    /// Public data only, in a canonical order. Serve this at
    /// GET /api/v1/stealth/cosigners. Clients must ALSO pin the expected
    /// cosigner out of band (app / docs): a value fetched only from the API
    /// proves nothing against whoever controls the API.
    pub fn public_info(&self) -> Vec<CosignerInfo> {
        let mut v: Vec<CosignerInfo> = self
            .chains
            .iter()
            .map(|(chain, rt)| CosignerInfo {
                chain: format!("{chain:?}"),
                algo: match rt.cfg.algo {
                    SigAlgo::EcdsaSecp256k1 => "ecdsa_secp256k1",
                    SigAlgo::Ed25519 => "ed25519",
                },
                cosigner: match rt.cosigner {
                    CosignerId::Eth(a) => format!("{a:?}"),
                    CosignerId::Ed25519(p) => p.to_string(),
                },
                factory: match &rt.fam {
                    FamilyRt::Evm(ev) => Some(format!("{:?}", ev.factory)),
                    _ => None,
                },
            })
            .collect();
        v.sort_by(|a, b| a.chain.cmp(&b.chain));
        v
    }

    /// 64-byte TEE quote `report_data`: sha256(canonical cosigner list) || 32
    /// zero bytes. Request a quote with this so a client can verify "these
    /// exact cosigners are held by this attested app".
    pub fn attestation_report_data(&self) -> Result<[u8; 64]> {
        let json = serde_json::to_vec(&self.public_info())?;
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&Sha256::digest(json));
        Ok(out)
    }
}

/// Lowercase, strip 0x and leading zeros for hex; leave base58 untouched.
pub fn norm_target(s: &str) -> String {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => h.trim_start_matches('0').to_lowercase(),
        None => t.to_string(),
    }
}

// ============================================================================
// Worker context + loop
// ============================================================================

pub struct WorkerCtx {
    pub state: Arc<AppState>,
    pub chains: Arc<ChainRegistry>,
    /// The SAME per-chain keeper clients the payment and sweep workers use
    /// (NonceManagerMiddleware inside), so stealth txs never desync nonces.
    pub evm_clients: HashMap<Chain, Arc<SignerProvider>>,
    pub solana_rpc: Arc<RpcClient>,
    pub solana_keeper: Arc<Keypair>,
}

#[derive(Default)]
struct Recent {
    map: Mutex<HashMap<String, (Instant, bool)>>,
}

impl Recent {
    async fn try_begin(&self, key: &str) -> bool {
        let mut m = self.map.lock().await;
        m.retain(|_, (t, _)| t.elapsed() < DEDUPE_TTL);
        if m.contains_key(key) {
            return false;
        }
        m.insert(key.to_string(), (Instant::now(), false));
        true
    }
    async fn finish(&self, key: &str, ok: bool) {
        let mut m = self.map.lock().await;
        if ok {
            m.insert(key.to_string(), (Instant::now(), true));
        } else {
            m.remove(key); // failed rounds may be retried
        }
    }
}

pub async fn start_stealth_workers(ctx: Arc<WorkerCtx>, mut rx: mpsc::Receiver<StealthTask>) {
    info!("stealth worker starting, {} chains", ctx.chains.len());
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_ROUNDS));
    let recent = Arc::new(Recent::default());
    while let Some(task) = rx.recv().await {
        // Backpressure: a slow receipt poll no longer blocks every other claim.
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break;
        };
        let (ctx, recent) = (ctx.clone(), recent.clone());
        tokio::spawn(async move {
            let _permit = permit;
            run_round(&ctx, &recent, task).await;
        });
    }
}

/// The in-memory dedupe is only a first line of defense (it is lost on restart).
/// The real idempotency is on chain: EVM USDC authorizationState, Starknet
/// account nonce, Solana blockhash + balances. A replayed claim fails at
/// preflight.
async fn run_round(ctx: &WorkerCtx, recent: &Recent, task: StealthTask) {
    let key = format!("{:?}:{}", task.chain, task.tx_hash.to_lowercase());
    if !recent.try_begin(&key).await {
        warn!("[round {key}] duplicate, dropped");
        return;
    }
    let outcome = match tokio::time::timeout(ROUND_TIMEOUT, execute(ctx, &task)).await {
        Ok(r) => r,
        Err(_) => Err(anyhow!("round timed out after {ROUND_TIMEOUT:?}")),
    };
    match outcome {
        Ok(h) => {
            recent.finish(&key, true).await;
            info!("[round {key}] confirmed, chain tx {h}");
        }
        Err(e) => {
            recent.finish(&key, false).await;
            error!("[round {key}] failed: {e:#}");
        }
    }
}

async fn execute(ctx: &WorkerCtx, task: &StealthTask) -> Result<String> {
    // 1 resolve
    let rt = ctx.chains.get(task.chain)?;
    ensure!(
        rt.cfg.enabled_for_claims,
        "{:?} claims are disabled",
        task.chain
    );
    // 2 screen (same function the route ran; defense in depth)
    let plan = precheck(rt, &ctx.solana_keeper.pubkey(), task)?;
    match (&rt.fam, plan) {
        (FamilyRt::Evm(ev), Plan::Evm(p)) => relay_evm(ctx, rt, ev, task, p).await,
        (FamilyRt::Starknet(st), Plan::Starknet(p)) => relay_starknet(ctx, rt, st, p).await,
        (FamilyRt::Solana, Plan::Solana(p)) => relay_solana(ctx, rt, p).await,
        _ => bail!("plan does not match chain family"),
    }
}

// ============================================================================
// Screening (pure, sync). The route calls this too.
// ============================================================================

pub enum Plan {
    Evm(EvmPlan),
    Starknet(StarknetPlan),
    Solana(SolanaPlan),
}

pub fn precheck(rt: &ChainRuntime, solana_relayer: &Pubkey, task: &StealthTask) -> Result<Plan> {
    match &rt.fam {
        FamilyRt::Evm(ev) => precheck_evm(ev, task).map(Plan::Evm),
        FamilyRt::Starknet(st) => precheck_starknet(rt, st, task).map(Plan::Starknet),
        FamilyRt::Solana => precheck_solana(rt, solana_relayer, task).map(Plan::Solana),
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

fn strip0x(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t)
}

/// "" and "0x" decode to empty. Odd length is an error (never silently padded).
fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    let t = strip0x(s);
    ensure!(t.len() % 2 == 0, "odd-length hex");
    hex::decode(t).context("not hex")
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let v = hex_bytes(s)?;
    ensure!(v.len() == 32, "expected 32 bytes, got {}", v.len());
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Left-pads short values (r/s words). Use `hex32` for hashes.
fn pad32(hex_str: &str) -> Result<[u8; 32]> {
    let t = strip0x(hex_str);
    ensure!(t.len() <= 64, "value exceeds 256 bits");
    let v = hex::decode(format!("{t:0>64}")).context("not hex")?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

fn parse_u256(s: &str) -> Result<U256> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        ensure!(!h.is_empty() && h.len() <= 64, "bad uint256");
        U256::from_str_radix(h, 16).map_err(|e| anyhow!("bad uint256: {e}"))
    } else {
        U256::from_dec_str(t).map_err(|e| anyhow!("bad uint256: {e}"))
    }
}

fn parse_u128(s: &str) -> Result<u128> {
    let v = parse_u256(s)?;
    ensure!(v <= U256::from(u128::MAX), "value exceeds u128");
    Ok(v.as_u128())
}

fn parse_u64(s: &str) -> Result<u64> {
    let v = parse_u256(s)?;
    ensure!(v <= U256::from(u64::MAX), "value exceeds u64");
    Ok(v.as_u64())
}

fn to_be32(u: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    u.to_big_endian(&mut b);
    b
}

fn secp_n() -> U256 {
    U256::from_str_radix(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",
        16,
    )
    .expect("constant")
}

/// Returns (s normalized to the low half, whether it was flipped).
fn low_s(s: [u8; 32]) -> ([u8; 32], bool) {
    let n = secp_n();
    let su = U256::from_big_endian(&s);
    if su > (n >> 1) {
        (to_be32(n - su), true)
    } else {
        (s, false)
    }
}

fn be32_to_low_high(b: &[u8; 32]) -> (u128, u128) {
    let high = u128::from_be_bytes(b[0..16].try_into().unwrap());
    let low = u128::from_be_bytes(b[16..32].try_into().unwrap());
    (low, high)
}

fn pack_sig65(r: [u8; 32], s: [u8; 32], v: u8) -> [u8; 65] {
    let mut out = [0u8; 65];
    out[..32].copy_from_slice(&r);
    out[32..64].copy_from_slice(&s);
    out[64] = if v < 27 { v + 27 } else { v };
    out
}

struct Secp256k1Cosig {
    r: [u8; 32],
    s: [u8; 32],
    recid: u8, // 0 or 1
}

/// Signs `digest` with `wallet`, normalizes to low-s, then finds the recovery
/// id that recovers to `expected`. We never trust the library's `v`: if the
/// low-s flip changed the parity, recovery picks the right one. Fails closed
/// if none recovers. (`sign_hash` signs the 32 bytes as-is, no extra hashing.)
fn sign_secp256k1_with(
    wallet: &LocalWallet,
    expected: Address,
    digest: [u8; 32],
) -> Result<Secp256k1Cosig> {
    let sig = wallet
        .sign_hash(H256::from(digest))
        .map_err(|e| anyhow!("cosigner signing failed: {e}"))?;
    let r = to_be32(sig.r);
    let (s, _) = low_s(to_be32(sig.s));
    let (r_u, s_u) = (U256::from_big_endian(&r), U256::from_big_endian(&s));
    for recid in [0u8, 1u8] {
        let candidate = EthSignature {
            r: r_u,
            s: s_u,
            v: 27 + recid as u64,
        };
        if candidate.recover(H256::from(digest)).ok() == Some(expected) {
            return Ok(Secp256k1Cosig { r, s, recid });
        }
    }
    bail!("cosignature does not recover to the registered cosigner {expected:?}")
}

/// Step 3 for secp256k1 chains (EVM + Starknet). Synchronous: the key is in
/// process memory, so there is no network round trip and no polling.
fn cosign_secp256k1(rt: &ChainRuntime, digest: [u8; 32]) -> Result<Secp256k1Cosig> {
    let CosignerId::Eth(expected) = rt.cosigner else {
        bail!("cosigner is not a secp256k1 address")
    };
    let CosignerKey::Secp256k1(wallet) = &rt.key else {
        bail!("cosigner key is not secp256k1")
    };
    sign_secp256k1_with(wallet, expected, digest)
}

// ============================================================================
// EVM  (USDC EIP-3009 + StealthAccount ERC-1271, NO ERC-4337)
// ============================================================================

abigen!(
    Erc3009UsdcBytes,
    r#"[
        function transferWithAuthorization(address from, address to, uint256 value, uint256 validAfter, uint256 validBefore, bytes32 nonce, bytes signature)
        function authorizationState(address authorizer, bytes32 nonce) external view returns (bool)
        function DOMAIN_SEPARATOR() external view returns (bytes32)
    ]"#
);

// The factory takes the cosigner per account: no `cosigner()` getter exists.
abigen!(
    StealthFactory,
    r#"[
        function createAccount(address client, address cosigner, bytes32 salt) external returns (address)
        function getAddress(address client, address cosigner, bytes32 salt) external view returns (address)
        function entryPoint() external view returns (address)
    ]"#
);

/// The fields of one USDC `TransferWithAuthorization`.
#[derive(Clone, Copy, Debug)]
pub struct TransferAuth {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub valid_after: U256,
    pub valid_before: U256,
    pub nonce: [u8; 32],
}

pub struct EvmPlan {
    auth: TransferAuth,
    client: Address,
    salt: [u8; 32],
    /// EIP-191(D): what BOTH signers sign.
    signed: [u8; 32],
    client_r: [u8; 32],
    client_s: [u8; 32],
    /// 27 or 28, resolved by recovery against `client`.
    client_v: u8,
}

/// EIP-712 domain separator for a token's (name, version, chainId, verifyingContract).
pub fn eip712_domain_separator(
    name: &str,
    version: &str,
    chain_id: u64,
    verifying_contract: Address,
) -> [u8; 32] {
    keccak256(encode(&[
        Token::FixedBytes(
            keccak256(
                "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            )
            .to_vec(),
        ),
        Token::FixedBytes(keccak256(name).to_vec()),
        Token::FixedBytes(keccak256(version).to_vec()),
        Token::Uint(U256::from(chain_id)),
        Token::Address(verifying_contract),
    ]))
}

/// EIP-712 digest D of a USDC `TransferWithAuthorization`, computed locally.
pub fn transfer_auth_digest(domain_separator: [u8; 32], a: &TransferAuth) -> [u8; 32] {
    let struct_hash = keccak256(encode(&[
        Token::FixedBytes(
            keccak256(
                "TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
            )
            .to_vec(),
        ),
        Token::Address(a.from),
        Token::Address(a.to),
        Token::Uint(a.value),
        Token::Uint(a.valid_after),
        Token::Uint(a.valid_before),
        Token::FixedBytes(a.nonce.to_vec()),
    ]));
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&domain_separator);
    buf.extend_from_slice(&struct_hash);
    keccak256(buf)
}

/// Checks the client's signature over `signed` (= EIP-191(D)) against the
/// `client` key from the request. Normalizes to low-s (OpenZeppelin's ECDSA in
/// StealthAccount rejects high-s), then finds the parity that recovers to
/// `client`. The client's own `v` is ignored: recovery is authoritative.
fn verify_client_sig(
    client: Address,
    signed: [u8; 32],
    r1: &str,
    s1: &str,
) -> Result<([u8; 32], [u8; 32], u8)> {
    let r = pad32(r1).context("bad client r")?;
    let (s, _) = low_s(pad32(s1).context("bad client s")?);
    ensure!(r != [0u8; 32] && s != [0u8; 32], "zero r/s");
    let (r_u, s_u) = (U256::from_big_endian(&r), U256::from_big_endian(&s));
    for v in [27u64, 28u64] {
        let sig = EthSignature { r: r_u, s: s_u, v };
        if sig.recover(H256::from(signed)).ok() == Some(client) {
            return Ok((r, s, v as u8));
        }
    }
    bail!("client signature does not recover to `client` over EIP-191(digest)")
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// Startup checks for one EVM chain. Every failure here refuses to start the
/// service rather than discovering a misconfigured chain on the first claim.
async fn build_evm_rt(chain: Chain, cfg: &ChainCfg, cosigner: CosignerId) -> Result<EvmRt> {
    let ChainFamily::Evm {
        chain_id,
        rpc_url,
        factory,
        usdc,
        domain_name,
        domain_version,
        multicall3,
        max_value,
        max_auth_window_secs,
    } = &cfg.family
    else {
        bail!("not an evm chain config")
    };

    let provider = Arc::new(Provider::<Http>::try_from(rpc_url.as_str())?);
    let live = provider
        .get_chainid()
        .await
        .with_context(|| format!("{chain:?}: rpc unreachable"))?;
    ensure!(
        live == U256::from(*chain_id),
        "{chain:?}: rpc reports chain id {live}, config says {chain_id}"
    );

    let usdc_addr: Address = usdc.parse().context("bad usdc address")?;
    let factory_addr: Address = factory.parse().context("bad factory address")?;
    let multicall_addr: Address = multicall3.parse().context("bad multicall3 address")?;
    ensure!(
        cfg.allowed_targets.contains(&norm_target(usdc)),
        "{chain:?}: allowed_targets must contain the usdc address"
    );

    // 1. Our EIP-712 domain must equal the token's own. A wrong name/version
    //    would make every digest (and so every signature) wrong.
    let domain_separator =
        eip712_domain_separator(domain_name, domain_version, *chain_id, usdc_addr);
    let token = Erc3009UsdcBytes::new(usdc_addr, provider.clone());
    let onchain_ds = token
        .domain_separator()
        .call()
        .await
        .context("USDC.DOMAIN_SEPARATOR() failed (is `usdc` an EIP-3009 token?)")?;
    ensure!(
        onchain_ds == domain_separator,
        "domain separator mismatch: token reports 0x{}, config (name/version/chain_id/usdc) gives 0x{}",
        hex::encode(onchain_ds),
        hex::encode(domain_separator)
    );

    // 2. The factory is immutable and takes the cosigner per account, so there
    //    is nothing to pin here. Prove it is a StealthAccountFactory with the
    //    3-arg ABI and that it accepts OUR cosigner address.
    let CosignerId::Eth(our_cosigner) = cosigner else {
        bail!("evm cosigner must be a secp256k1 address")
    };
    let fac = StealthFactory::new(factory_addr, provider.clone());
    let entry_point = fac
        .entry_point()
        .call()
        .await
        .context("factory.entryPoint() failed (wrong factory address?)")?;
    ensure!(
        entry_point != Address::zero(),
        "factory reports a zero entryPoint"
    );
    fac.get_address(Address::repeat_byte(1), our_cosigner, [0u8; 32])
        .call()
        .await
        .context("factory.getAddress(client, cosigner, salt) failed (wrong factory version?)")?;

    // 3. The `bytes` overload only exists in FiatTokenV2_2.
    probe_bytes_overload(&token).await?;

    let max_value = match max_value {
        Some(s) => Some(parse_u256(s).context("bad max_value")?),
        None => None,
    };

    info!("EVM runtime initialized for chain id {}", *chain_id);

    Ok(EvmRt {
        factory: factory_addr,
        usdc: usdc_addr,
        multicall3: multicall_addr,
        domain_separator,
        max_value,
        max_auth_window_secs: *max_auth_window_secs,
    })
}

/// eth_call of the `bytes` overload with junk args. A token that has the
/// function reverts WITH a reason (invalid signature, etc). One that lacks it
/// reverts with empty data. Fails closed if the RPC gives us no revert data.
async fn probe_bytes_overload<M: Middleware + 'static>(token: &Erc3009UsdcBytes<M>) -> Result<()> {
    let call = token.transfer_with_authorization(
        Address::from_low_u64_be(1),
        Address::from_low_u64_be(2),
        U256::zero(),
        U256::zero(),
        U256::from(u64::MAX),
        [0u8; 32],
        Bytes::default(),
    );
    match call.call().await {
        Ok(_) => bail!("bytes-overload probe unexpectedly succeeded"),
        Err(e) => match e.as_revert() {
            Some(data) if data.len() >= 4 => Ok(()),
            _ => bail!(
                "token has no `transferWithAuthorization(..., bytes)` overload, or the RPC \
                 returned no revert data ({e}). Only FiatTokenV2_2 USDC is supported."
            ),
        },
    }
}

fn precheck_evm(ev: &EvmRt, task: &StealthTask) -> Result<EvmPlan> {
    ensure!(
        task.calls.is_empty(),
        "evm claims carry no `calls`: one claim is one authorization"
    );
    let p: &Auth3009Params = task
        .auth3009
        .as_ref()
        .context("evm claim requires `auth3009` params")?;

    let from: Address = task
        .derived_address
        .parse()
        .context("bad derived_address")?;
    let to: Address = p.to.parse().context("bad to")?;
    let client: Address = p.client.parse().context("bad client")?;
    ensure!(to != Address::zero(), "`to` is the zero address");
    ensure!(to != from, "`to` equals the stealth account");

    let value = parse_u256(&p.value).context("bad value")?;
    ensure!(!value.is_zero(), "value is zero");
    if let Some(max) = ev.max_value {
        ensure!(
            value <= max,
            "value {value} exceeds the per-claim cap {max}"
        );
    }

    let valid_after = parse_u256(&p.valid_after).context("bad valid_after")?;
    let valid_before = parse_u256(&p.valid_before).context("bad valid_before")?;
    let now = U256::from(unix_now()?);
    // USDC requires validAfter < block.timestamp < validBefore (both strict).
    ensure!(valid_after < now, "authorization is not valid yet");
    ensure!(
        valid_before > now + U256::from(MIN_AUTH_TTL_SECS),
        "authorization expires in under {MIN_AUTH_TTL_SECS}s"
    );
    ensure!(
        valid_before <= now + U256::from(ev.max_auth_window_secs),
        "authorization window exceeds {}s",
        ev.max_auth_window_secs
    );

    let auth = TransferAuth {
        from,
        to,
        value,
        valid_after,
        valid_before,
        nonce: hex32(&p.nonce).context("bad nonce")?,
    };
    let salt = hex32(&p.salt).context("bad salt")?;

    // tx_hash MUST be D of exactly these fields (this chain's USDC domain).
    let digest = transfer_auth_digest(ev.domain_separator, &auth);
    ensure!(
        digest == hex32(&task.tx_hash).context("bad tx_hash")?,
        "tx_hash does not match the EIP-3009 digest of the submitted authorization"
    );
    let signed = hash_message(digest).0; // EIP-191 over the 32 raw bytes

    let ClientSignature::Ecdsa { r1, s1, .. } = &task.client_sig else {
        bail!("evm task needs an ecdsa client signature")
    };
    let (client_r, client_s, client_v) = verify_client_sig(client, signed, r1, s1)?;

    Ok(EvmPlan {
        auth,
        client,
        salt,
        signed,
        client_r,
        client_s,
        client_v,
    })
}

/// handleOps-style partial success does not exist here: aggregate3 runs with
/// allowFailure=false, so a status-1 receipt means createAccount (if present)
/// AND the transfer both succeeded.
async fn confirm_claim(sp: &SignerProvider, tx_hash: H256) -> Result<()> {
    for _ in 0..EVM_RECEIPT_ATTEMPTS {
        tokio::time::sleep(EVM_RECEIPT_POLL).await;
        let rcpt = match sp.get_transaction_receipt(tx_hash).await {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                warn!("receipt poll error for {tx_hash:#x}: {e}");
                continue;
            }
        };
        ensure!(
            rcpt.status.map(|s| s.as_u64()) == Some(1),
            "claim tx {tx_hash:#x} reverted"
        );
        return Ok(());
    }
    bail!("timed out waiting for the receipt of {tx_hash:#x}")
}

async fn relay_evm(
    ctx: &WorkerCtx,
    rt: &ChainRuntime,
    ev: &EvmRt,
    task: &StealthTask,
    plan: EvmPlan,
) -> Result<String> {
    let sp = ctx
        .evm_clients
        .get(&task.chain)
        .with_context(|| format!("no keeper client for {:?}", task.chain))?
        .clone();
    let a = plan.auth;
    let CosignerId::Eth(our_cosigner) = rt.cosigner else {
        bail!("evm cosigner must be a secp256k1 address")
    };

    // 3a. BEFORE spending a cosignature: the account must be the one the
    //     factory derives for (client, OUR cosigner, salt), and the
    //     authorization must still be unused.
    let factory = StealthFactory::new(ev.factory, sp.clone());
    let expected_from = factory
        .get_address(plan.client, our_cosigner, plan.salt)
        .call()
        .await
        .context("factory.getAddress failed")?;
    // The address commits to (entryPoint, client, cosigner, salt), so a match
    // proves the account is bound to OUR cosigner. Any other address is refused.
    ensure!(
        expected_from == a.from,
        "derived_address {:?} != factory.getAddress(client, our cosigner, salt) {expected_from:?}",
        a.from
    );
    let usdc = Erc3009UsdcBytes::new(ev.usdc, sp.clone());
    let used = usdc
        .authorization_state(a.from, a.nonce)
        .call()
        .await
        .context("authorizationState failed")?;
    ensure!(!used, "authorization nonce already used or canceled");
    // ERC-1271 needs code, so createAccount is batched ahead of the transfer
    // when the account is not deployed yet.
    let deployed = !sp
        .get_code(a.from, None)
        .await
        .context("get_code failed")?
        .0
        .is_empty();

    // 3. cosign EIP-191(D) locally. Both signers sign this same 32-byte hash.
    let co = cosign_secp256k1(rt, plan.signed)?;

    // 4. assemble: sig = client65 || cosigner65, then the atomic batch.
    let mut sig130 = Vec::with_capacity(130);
    sig130.extend_from_slice(&pack_sig65(plan.client_r, plan.client_s, plan.client_v));
    sig130.extend_from_slice(&pack_sig65(co.r, co.s, co.recid));

    let mut calls: Vec<Call3> = Vec::with_capacity(2);
    if !deployed {
        calls.push(Call3 {
            target: ev.factory,
            allow_failure: false, // idempotent, and the transfer needs it
            call_data: factory
                .create_account(plan.client, our_cosigner, plan.salt)
                .calldata()
                .context("encode createAccount")?,
        });
    }
    calls.push(Call3 {
        target: ev.usdc,
        allow_failure: false,
        call_data: usdc
            .transfer_with_authorization(
                a.from,
                a.to,
                a.value,
                a.valid_after,
                a.valid_before,
                a.nonce,
                Bytes::from(sig130),
            )
            .calldata()
            .context("encode transferWithAuthorization")?,
    });
    let batch = Multicall3::new(ev.multicall3, sp.clone())
        .aggregate_3(calls)
        .calldata()
        .context("encode aggregate3")?;

    // 5. preflight: a bad signature / balance / window fails here, before fees.
    //    Fees come from the provider; no hardcoded caps (they are chain-specific).
    let (max_fee, priority_fee) = sp
        .estimate_eip1559_fees(None)
        .await
        .context("fee estimation failed")?;
    let tx = Eip1559TransactionRequest::new()
        .from(sp.address())
        .to(ev.multicall3)
        .data(batch)
        .max_fee_per_gas(max_fee)
        .max_priority_fee_per_gas(priority_fee);
    let gas = sp
        .estimate_gas(&tx.clone().into(), None)
        .await
        .map_err(|e| anyhow!("preflight failed (bad signature, balance, window or nonce): {e}"))?;
    let tx = tx.gas(gas * U256::from(12u64) / U256::from(10u64));

    // 6. relay through the shared NonceManager-backed client.
    let pending = sp
        .send_transaction(tx, None)
        .await
        .map_err(|e| anyhow!("broadcast failed: {e}"))?;
    let tx_hash = pending.tx_hash();
    drop(pending);

    confirm_claim(&sp, tx_hash).await?;
    Ok(format!("{tx_hash:#x}"))
}

// ============================================================================
// Starknet  (SNIP-6 account, StealthAccount.cairo)
// ============================================================================

pub struct StarknetPlan {
    sender: Felt,
    calls: Vec<StarknetCall>,
    calldata: Vec<Felt>,
    client_pubkey: Felt,
    cosigner_felt: Felt,
    salt: Felt,
    nonce: Felt,
    tip: u64,
    l1_gas: (u64, u128),
    l2_gas: (u64, u128),
    l1_data_gas: (u64, u128),
    r1: Felt,
    s1: Felt,
    hash: Felt,
    max_fee: u128,
}

/// SNIP-6 `__execute__` calldata: [count, (to, selector, len, ...data)*].
fn encode_calls_for_execute(calls: &[StarknetCall]) -> Vec<Felt> {
    let mut out = vec![Felt::from(calls.len() as u64)];
    for c in calls {
        out.push(c.to);
        out.push(c.selector);
        out.push(Felt::from(c.calldata.len() as u64));
        out.extend(c.calldata.iter().copied());
    }
    out
}

/// Packs a resource bound into a single Felt for V3 transactions
fn pack_bound(name: u64, max_amount: u64, max_price: u128) -> Felt {
    // Left-shift name by 192 bits (64 bits data, placed at top of 256 bits)
    // Left-shift max_amount by 128 bits
    // max_price takes the bottom 128 bits
    // Adjust logic here based on your actual `pack_bound` implementation

    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&name.to_be_bytes());
    bytes[8..16].copy_from_slice(&max_amount.to_be_bytes());
    bytes[16..32].copy_from_slice(&max_price.to_be_bytes());

    Felt::from_bytes_be(&bytes)
}

/// Native Starknet INVOKE V3 transaction hash (starknet >= 0.13.4 layout):
///   h("invoke", 3, sender, h(tip, L1_GAS, L2_GAS, L1_DATA), h(paymaster_data),
///     chain_id, nonce, da_modes, h(account_deployment_data), h(calldata))
/// Paymaster data / deployment data are always empty and both DA modes L1 (=0),
/// matching what `relay_starknet` broadcasts. Confidence: layout from the
/// Starknet docs as remembered. TEST IT against a real V3 invoke hash before
/// enabling Starknet; a wrong layout fails closed (every claim is rejected).
#[allow(clippy::too_many_arguments)]
pub fn starknet_invoke_v3_hash(
    chain_id: Felt,
    sender: Felt,
    calldata: &[Felt],
    nonce: Felt,
    tip: u64,
    l1_gas: (u64, u128),
    l2_gas: (u64, u128),
    l1_data_gas: (u64, u128),
) -> Felt {
    // 1. Hash the fee bounds (includes tip as the first element)
    let fee_fields = poseidon_hash_many(&[
        Felt::from(tip),
        pack_bound(NAME_L1_GAS, l1_gas.0, l1_gas.1),
        pack_bound(NAME_L2_GAS, l2_gas.0, l2_gas.1),
        pack_bound(NAME_L1_DATA, l1_data_gas.0, l1_data_gas.1),
    ]);

    // 2. Compute the full V3 transaction hash using Poseidon
    poseidon_hash_many(&[
        Felt::from_bytes_be_slice(b"invoke"),
        Felt::from(3u64),             // version 3
        sender,                       // sender_address
        fee_fields,                   // fee_data bounds hash
        poseidon_hash_many(&[]),      // paymaster_data hash
        chain_id,                     // chain_id
        nonce,                        // nonce
        Felt::ZERO,                   // data_availability_modes ((nonce_da << 32) | fee_da)
        poseidon_hash_many(&[]),      // account_deployment_data hash
        poseidon_hash_many(calldata), // calldata hash
    ])
}

fn parse_bound(b: &crate::stealth_routes::ResourceBoundParam) -> Result<(u64, u128)> {
    Ok((
        parse_u64(&b.max_amount)?,
        parse_u128(&b.max_price_per_unit)?,
    ))
}

fn precheck_starknet(
    rt: &ChainRuntime,
    st: &StarknetRt,
    task: &StealthTask,
) -> Result<StarknetPlan> {
    let sp: &StarknetTxParams = task
        .starknet
        .as_ref()
        .context("starknet claim requires `starknet` params")?;
    let cfg = &rt.cfg;
    ensure!(
        !task.calls.is_empty() && task.calls.len() <= MAX_CALLS,
        "starknet claim needs 1..={MAX_CALLS} calls"
    );

    let mut calls = Vec::with_capacity(task.calls.len());
    for (i, c) in task.calls.iter().enumerate() {
        ensure!(
            cfg.allowed_targets
                .contains(&norm_target(&c.contract_address)),
            "call {i}: target {} not allowed",
            c.contract_address
        );
        let ep = c.entrypoint.trim();
        ensure!(
            cfg.allowed_entrypoints.contains(ep),
            "call {i}: entrypoint {ep} not allowed"
        );
        let calldata = c
            .calldata
            .iter()
            .map(|x| Felt::from_hex(x))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("call {i}: bad calldata felt: {e}"))?;
        calls.push(StarknetCall {
            to: Felt::from_hex(&c.contract_address)
                .map_err(|e| anyhow!("call {i}: bad contract_address: {e}"))?,
            selector: get_selector_from_name(ep)?,
            calldata,
        });
    }

    let sender = Felt::from_hex(&task.derived_address).context("bad derived_address")?;
    let client_pubkey = Felt::from_hex(&sp.client_pubkey).context("bad client_pubkey")?;
    let salt = Felt::from_hex(&sp.deploy_salt).context("bad deploy_salt")?;
    let nonce = Felt::from_hex(&sp.nonce).context("bad nonce")?;
    let tip = parse_u64(&sp.tip).context("bad tip")?;
    let l1_gas = parse_bound(&sp.l1_gas).context("bad l1_gas")?;
    let l2_gas = parse_bound(&sp.l2_gas).context("bad l2_gas")?;
    let l1_data_gas = parse_bound(&sp.l1_data_gas).context("bad l1_data_gas")?;

    // The keeper will fund up to this much STRK. Cap it.
    let mut max_fee: u128 = 0;
    for (amt, price) in [l1_gas, l2_gas, l1_data_gas] {
        max_fee = (amt as u128)
            .checked_mul(price)
            .and_then(|x| x.checked_add(max_fee))
            .context("resource bounds overflow")?;
    }
    ensure!(
        max_fee <= st.max_fee,
        "max fee {max_fee} exceeds the cap {}",
        st.max_fee
    );

    // Bind (client_pubkey, our cosigner) to the account address: the address
    // is class + ctor args, so the account at `sender` can only be one whose
    // cosigner is OUR cosigner key. UDC unique=false => deployer 0.
    let CosignerId::Eth(cosigner) = rt.cosigner else {
        bail!("starknet cosigner must be a secp256k1 address")
    };
    let cosigner_felt = Felt::from_bytes_be_slice(cosigner.as_bytes());
    let derived = get_contract_address(
        salt,
        st.class_hash,
        &[client_pubkey, cosigner_felt],
        Felt::ZERO,
    );
    ensure!(
        derived == sender,
        "derived_address {sender:#x} != address of (class, salt, client_pubkey, cosigner) {derived:#x}"
    );

    let calldata = encode_calls_for_execute(&calls);
    let expected = starknet_invoke_v3_hash(
        st.chain_id,
        sender,
        &calldata,
        nonce,
        tip,
        l1_gas,
        l2_gas,
        l1_data_gas,
    );
    let hash = Felt::from_hex(&task.tx_hash).context("bad tx_hash")?;
    ensure!(
        expected == hash,
        "tx_hash does not match the invoke-v3 hash of the submitted calls/bounds/nonce"
    );

    let ClientSignature::Ecdsa { r1, s1, .. } = &task.client_sig else {
        bail!("starknet task needs an ecdsa client signature")
    };
    let r1 = Felt::from_hex(r1).context("bad r1")?;
    let s1 = Felt::from_hex(s1).context("bad s1")?;
    let ok = ecdsa_verify(&client_pubkey, &hash, &StarkSignature { r: r1, s: s1 })
        .map_err(|e| anyhow!("client signature check failed: {e:?}"))?;
    ensure!(ok, "client signature does not verify against client_pubkey");

    Ok(StarknetPlan {
        sender,
        calls,
        calldata,
        client_pubkey,
        cosigner_felt,
        salt,
        nonce,
        tip,
        l1_gas,
        l2_gas,
        l1_data_gas,
        r1,
        s1,
        hash,
        max_fee,
    })
}

async fn wait_starknet_tx<A: ConnectedAccount + Sync>(
    keeper: &A,
    hash: Felt,
    what: &str,
) -> Result<()> {
    for _ in 0..STARK_POLL_ATTEMPTS {
        tokio::time::sleep(STARK_POLL_INTERVAL).await;
        match keeper.provider().get_transaction_receipt(hash).await {
            Ok(r) => match r.receipt.execution_result() {
                ExecutionResult::Succeeded => return Ok(()),
                ExecutionResult::Reverted { reason } => {
                    bail!("{what} tx {hash:#x} reverted: {reason}")
                }
            },
            Err(ProviderError::StarknetError(StarknetError::TransactionHashNotFound)) => continue,
            Err(e) => {
                warn!("starknet receipt poll error for {hash:#x}: {e}");
                continue;
            }
        }
    }
    bail!("{what} tx {hash:#x} not confirmed in time")
}

async fn relay_starknet(
    ctx: &WorkerCtx,
    rt: &ChainRuntime,
    st: &StarknetRt,
    plan: StarknetPlan,
) -> Result<String> {
    // 3 cosign FIRST: if the key is misconfigured we have not spent keeper funds.
    let co = cosign_secp256k1(rt, plan.hash.to_bytes_be())?;

    let keeper = build_starknet_account(&ctx.state.starknet_config)
        .map_err(|e| anyhow!("keeper account: {e:?}"))?;
    let provider = keeper.provider();

    // 5 preflight: is the account deployed, is the nonce still current?
    let deployed = match provider
        .get_class_hash_at(BlockId::Tag(BlockTag::PreConfirmed), plan.sender)
        .await
    {
        Ok(_) => true,
        Err(ProviderError::StarknetError(StarknetError::ContractNotFound)) => false,
        Err(e) => return Err(anyhow!(e)).context("class hash lookup"),
    };
    let chain_nonce = if deployed {
        provider
            .get_nonce(BlockId::Tag(BlockTag::PreConfirmed), plan.sender)
            .await
            .context("account nonce")?
    } else {
        Felt::ZERO
    };
    ensure!(
        chain_nonce == plan.nonce,
        "signed nonce {:#x} != account nonce {:#x}",
        plan.nonce,
        chain_nonce
    );

    // The account pays its own fee in STRK and holds none: fund the shortfall.
    let bal = provider
        .call(
            FunctionCall {
                contract_address: st.fee_token,
                entry_point_selector: get_selector_from_name("balanceOf")?,
                calldata: vec![plan.sender],
            },
            BlockId::Tag(BlockTag::PreConfirmed),
        )
        .await
        .context("fee token balanceOf")?;
    ensure!(bal.len() == 2, "unexpected balanceOf return");
    let balance = if bal[1] != Felt::ZERO {
        u128::MAX
    } else {
        u128::try_from(bal[0]).unwrap_or(u128::MAX)
    };
    let shortfall = plan.max_fee.saturating_sub(balance);

    let mut prep: Vec<StarknetCall> = Vec::new();
    if !deployed {
        // UDC.deployContract(class_hash, salt, unique=false, ctor_calldata)
        prep.push(StarknetCall {
            to: st.udc,
            selector: get_selector_from_name("deployContract")?,
            calldata: vec![
                st.class_hash,
                plan.salt,
                Felt::ZERO,
                Felt::from(2u64),
                plan.client_pubkey,
                plan.cosigner_felt,
            ],
        });
    }
    if shortfall > 0 {
        // ERC-20 transfer(recipient, amount: u256 = [low, high])
        prep.push(StarknetCall {
            to: st.fee_token,
            selector: get_selector_from_name("transfer")?,
            calldata: vec![plan.sender, Felt::from(shortfall), Felt::ZERO],
        });
    }
    if !prep.is_empty() {
        // One multicall, serialized on the keeper nonce, confirmed before the claim.
        let _guard = rt.nonce_lock.lock().await;
        let res = keeper
            .execute_v3(prep)
            .send()
            .await
            .map_err(|e| anyhow!("keeper deploy/top-up failed: {e:?}"))?;
        wait_starknet_tx(&keeper, res.transaction_hash, "deploy/top-up").await?;
    }

    // 4 assemble: [r1, s1, r2_lo, r2_hi, s2_lo, s2_hi, v2]
    let (r2_low, r2_high) = be32_to_low_high(&co.r);
    let (s2_low, s2_high) = be32_to_low_high(&co.s);
    let signature = vec![
        plan.r1,
        plan.s1,
        Felt::from(r2_low),
        Felt::from(r2_high),
        Felt::from(s2_low),
        Felt::from(s2_high),
        Felt::from((STARKNET_V_OFFSET + co.recid) as u64),
    ];
    let rb = |(amt, price): (u64, u128)| ResourceBounds {
        max_amount: amt,
        max_price_per_unit: price,
    };
    let tx = BroadcastedInvokeTransactionV3 {
        sender_address: plan.sender,
        calldata: plan.calldata.clone(),
        signature,
        nonce: plan.nonce,
        resource_bounds: ResourceBoundsMapping {
            l1_gas: rb(plan.l1_gas),
            l1_data_gas: rb(plan.l1_data_gas),
            l2_gas: rb(plan.l2_gas),
        },
        tip: plan.tip,
        paymaster_data: vec![],
        account_deployment_data: vec![],
        nonce_data_availability_mode: DataAvailabilityMode::L1,
        fee_data_availability_mode: DataAvailabilityMode::L1,
        is_query: false,
    };

    // 6 relay (the gateway runs __validate__, i.e. both signatures, before accepting)
    let res = provider
        .add_invoke_transaction(tx)
        .await
        .context("starknet relay failed")?;
    ensure!(
        res.transaction_hash == plan.hash,
        "sequencer tx hash {:#x} != signed hash {:#x}",
        res.transaction_hash,
        plan.hash
    );
    wait_starknet_tx(&keeper, res.transaction_hash, "claim").await?;
    let _ = &plan.calls; // kept for logging/debugging
    Ok(format!("{:#x}", res.transaction_hash))
}

// ============================================================================
// Solana
// ============================================================================

pub struct SolanaPlan {
    message: SolanaMessage,
    bytes: Vec<u8>,
    client: Pubkey,
    cosigner: Pubkey,
    client_sig: SolanaSignature,
}

/// Deterministic, non-PDA multisig address. Rediscoverable from the client
/// pubkey alone, so no database. `base` = the cosigner, so only the cosigner's
/// key can create it. The client must derive the same address:
///   seed = hex(sha256("beanie-multisig-v1" || client_pubkey))[..32]
///   addr = sha256(base || seed || spl_token_program_id)   (createWithSeed)
pub fn derive_multisig(client: &Pubkey, cosigner: &Pubkey) -> Result<(Pubkey, String)> {
    let mut h = Sha256::new();
    h.update(MULTISIG_SEED_DOMAIN);
    h.update(client.as_ref());
    let seed = hex::encode(&h.finalize()[..16]); // 32 chars = MAX_SEED_LEN
    let addr = Pubkey::create_with_seed(cosigner, &seed, &spl_token::id())?;
    Ok((addr, seed))
}

/// Stablecoins only: TransferChecked, allowlisted mint, exactly one
/// instruction (a second instruction could spend the relayer, which signs as
/// fee payer), authority = the derived multisig, signers = {client, cosigner}.
fn validate_multisig_transfer(
    m: &SolanaMessage,
    relayer: &Pubkey,
    multisig: &Pubkey,
    client: &Pubkey,
    cosigner: &Pubkey,
    allowed_mints: &HashSet<String>,
) -> Result<()> {
    ensure!(m.instructions.len() == 1, "exactly one instruction allowed");
    ensure!(
        m.header.num_required_signatures == 3,
        "expected exactly 3 signers (relayer, client, cosigner)"
    );
    ensure!(
        m.account_keys.first() == Some(relayer),
        "fee payer must be the relayer"
    );
    let ix = &m.instructions[0];
    let program = m
        .account_keys
        .get(ix.program_id_index as usize)
        .context("bad program index")?;
    ensure!(*program == spl_token::id(), "not an SPL Token instruction");
    match TokenInstruction::unpack(&ix.data).map_err(|e| anyhow!("{e:?}"))? {
        TokenInstruction::TransferChecked { .. } => {}
        other => bail!("only TransferChecked is allowed, got {other:?}"),
    }
    // TransferChecked accounts: [source, mint, destination, authority, signers...]
    let acct = |pos: usize| -> Result<Pubkey> {
        let idx = *ix
            .accounts
            .get(pos)
            .ok_or_else(|| anyhow!("missing account {pos}"))?;
        m.account_keys
            .get(idx as usize)
            .copied()
            .ok_or_else(|| anyhow!("bad account index"))
    };
    let mint = acct(1)?;
    ensure!(
        allowed_mints.contains(&mint.to_string()),
        "mint {mint} is not allowed"
    );
    let authority = acct(3)?;
    ensure!(
        authority == *multisig,
        "authority {authority} != expected multisig {multisig}"
    );

    let signer_idx = ix.accounts.get(4..).unwrap_or(&[]);
    ensure!(
        signer_idx.len() == 2,
        "expected 2 multisig signers, got {}",
        signer_idx.len()
    );
    let mut found = HashSet::new();
    for &idx in signer_idx {
        let pk = *m
            .account_keys
            .get(idx as usize)
            .context("signer index out of range")?;
        ensure!(
            m.is_signer(idx as usize),
            "{pk} is not a transaction signer"
        );
        found.insert(pk);
    }
    ensure!(
        found.contains(client) && found.contains(cosigner),
        "signer set mismatch"
    );
    Ok(())
}

fn precheck_solana(rt: &ChainRuntime, relayer: &Pubkey, task: &StealthTask) -> Result<SolanaPlan> {
    let CosignerId::Ed25519(cosigner) = rt.cosigner else {
        bail!("cosigner is not ed25519")
    };
    let bytes = task
        .message_bytes
        .as_deref()
        .context("solana task missing message_bytes")?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_SOLANA_MESSAGE,
        "message_bytes size out of range"
    );
    let message: SolanaMessage = bincode::deserialize(bytes).context("bad solana message")?;
    ensure!(
        message.serialize() == bytes,
        "message is not canonically serialized"
    );
    // tx_hash for Solana = sha256(message_bytes): binds passkey + dedupe to the message.
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    ensure!(
        hex32(&task.tx_hash).context("bad tx_hash")? == digest,
        "tx_hash is not sha256(message_bytes)"
    );

    let client = Pubkey::from_str(&task.derived_address).context("bad derived_address")?;
    let (multisig, _) = derive_multisig(&client, &cosigner)?;
    validate_multisig_transfer(
        &message,
        relayer,
        &multisig,
        &client,
        &cosigner,
        &rt.cfg.allowed_targets,
    )?;
    let ClientSignature::Ed25519 { sig_hex } = &task.client_sig else {
        bail!("solana task needs an ed25519 client signature")
    };
    let client_sig = SolanaSignature::try_from(hex::decode(sig_hex)?.as_slice())
        .context("malformed client signature")?;
    ensure!(
        client_sig.verify(client.as_ref(), bytes),
        "client signature does not verify"
    );
    Ok(SolanaPlan {
        message,
        bytes: bytes.to_vec(),
        client,
        cosigner,
        client_sig,
    })
}

/// Step 3 for Solana. Signs `msg` with the in-process ed25519 key and verifies
/// the result against the registered cosigner pubkey before returning it.
fn cosign_ed25519(rt: &ChainRuntime, msg: &[u8]) -> Result<SolanaSignature> {
    let CosignerId::Ed25519(pk) = rt.cosigner else {
        bail!("cosigner is not ed25519")
    };
    let CosignerKey::Ed25519(kp) = &rt.key else {
        bail!("cosigner key is not ed25519")
    };
    let sig = kp.sign_message(msg);
    ensure!(
        sig.verify(pk.as_ref(), msg),
        "cosignature does not verify against cosigner {pk}"
    );
    Ok(sig)
}

async fn account_exists(rpc: &RpcClient, pk: &Pubkey) -> Result<bool> {
    Ok(rpc
        .get_account_with_commitment(pk, CommitmentConfig::confirmed())
        .await?
        .value
        .is_some())
}

/// Lazily creates + initializes the 2-of-2 multisig at first claim. Funds may
/// already sit in an ATA owned by the (not yet existing) multisig address: the
/// address is derivable without the account. Idempotent.
async fn ensure_multisig(
    ctx: &WorkerCtx,
    rt: &ChainRuntime,
    client: &Pubkey,
    cosigner: &Pubkey,
) -> Result<Pubkey> {
    let (ms, seed) = derive_multisig(client, cosigner)?;
    let rpc: &RpcClient = &ctx.solana_rpc;
    if account_exists(rpc, &ms).await? {
        return Ok(ms);
    }
    let relayer: &Keypair = &ctx.solana_keeper;
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(spl_token::state::Multisig::LEN)
        .await?;
    let ixs = vec![
        system_instruction::create_account_with_seed(
            &relayer.pubkey(),
            &ms,
            cosigner, // base: signs creation
            &seed,
            rent,
            spl_token::state::Multisig::LEN as u64,
            &spl_token::id(),
        ),
        spl_token::instruction::initialize_multisig2(
            &spl_token::id(),
            &ms,
            &[client, cosigner],
            2,
        )?,
    ];
    let blockhash = rpc.get_latest_blockhash().await?;
    let msg = SolanaMessage::new_with_blockhash(&ixs, Some(&relayer.pubkey()), &blockhash);
    let co = cosign_ed25519(rt, &msg.serialize())?;
    let mut tx = SolanaTransaction::new_unsigned(msg.clone());
    let pos = msg
        .account_keys
        .iter()
        .position(|k| k == cosigner)
        .context("cosigner not in msg")?;
    tx.signatures[pos] = co;
    tx.partial_sign(&[relayer], blockhash);
    if let Err(e) = rpc.send_and_confirm_transaction(&tx).await {
        // lost a race with a concurrent claim for the same client
        if account_exists(rpc, &ms).await? {
            return Ok(ms);
        }
        return Err(anyhow!(e)).context("multisig creation failed");
    }
    Ok(ms)
}

async fn relay_solana(ctx: &WorkerCtx, rt: &ChainRuntime, plan: SolanaPlan) -> Result<String> {
    // 3 cosign (after the multisig exists; creation is cosigner-gated too)
    ensure_multisig(ctx, rt, &plan.client, &plan.cosigner).await?;
    let co = cosign_ed25519(rt, &plan.bytes)?;

    // 4 assemble
    let message = plan.message;
    let n = message.header.num_required_signatures as usize;
    let pos = |pk: &Pubkey| -> Result<usize> {
        let i = message
            .account_keys
            .iter()
            .position(|k| k == pk)
            .ok_or_else(|| anyhow!("{pk} missing"))?;
        ensure!(i < n, "{pk} is not a required signer");
        Ok(i)
    };
    let (ci, ki) = (pos(&plan.client)?, pos(&plan.cosigner)?);
    let blockhash = message.recent_blockhash;
    let mut tx = SolanaTransaction::new_unsigned(message);
    tx.signatures[ci] = plan.client_sig;
    tx.signatures[ki] = co;
    let relayer: &Keypair = &ctx.solana_keeper;
    tx.partial_sign(&[relayer], blockhash);

    // 5 preflight (RPC preflight on send) + 6 relay
    let sig = ctx
        .solana_rpc
        .send_and_confirm_transaction(&tx)
        .await
        .context("solana relay failed (blockhash may have expired)")?;
    Ok(sig.to_string())
}

// ============================================================================
// Tests for the pure pieces
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_target_strips_prefix_and_zeros() {
        assert_eq!(norm_target("0x00AbC"), "abc");
        assert_eq!(norm_target(" So11111111 "), "So11111111");
    }

    #[test]
    fn low_high_split() {
        let mut b = [0u8; 32];
        b[15] = 1;
        b[31] = 2;
        assert_eq!(be32_to_low_high(&b), (2, 1));
    }

    #[test]
    fn bound_packing_layout() {
        let f = pack_bound(NAME_L1_GAS, 1, 2);
        let b = f.to_bytes_be();
        assert_eq!(&b[0..8], &NAME_L1_GAS.to_be_bytes());
        assert_eq!(&b[8..16], &1u64.to_be_bytes());
        assert_eq!(&b[16..32], &2u128.to_be_bytes());
    }

    #[test]
    fn high_s_is_flipped_once() {
        let n = secp_n();
        let high = to_be32(n - U256::from(5u64));
        let (s, flipped) = low_s(high);
        assert!(flipped);
        assert_eq!(U256::from_big_endian(&s), U256::from(5u64));
        assert!(!low_s(s).1);
    }
}

#[cfg(test)]
mod key_derivation_tests {
    use super::*;

    #[test]
    fn same_inputs_same_key_different_path_different_key() {
        let m = b"kms-material";
        let a = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "beanie/cosigner/base", m).unwrap();
        let b = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "beanie/cosigner/base", m).unwrap();
        let c = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "beanie/cosigner/starknet", m).unwrap();
        assert_eq!(a, b, "derivation must be deterministic across restarts");
        assert_ne!(a, c, "paths must separate chains");
    }

    #[test]
    fn algo_and_material_separate_keys() {
        let m = b"kms-material";
        let e = derive_key_bytes(SigAlgo::Ed25519, "p", m).unwrap();
        let s = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "p", m).unwrap();
        let s2 = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "p", b"other").unwrap();
        assert_ne!(e, s);
        assert_ne!(
            s, s2,
            "different app/KMS material must give a different key"
        );
    }

    #[test]
    fn length_prefixing_prevents_field_bleed() {
        // ("ab","c") and ("a","bc") must not collide
        let x = derive_key_bytes(SigAlgo::Ed25519, "ab", b"c").unwrap();
        let y = derive_key_bytes(SigAlgo::Ed25519, "a", b"bc").unwrap();
        assert_ne!(x, y);
    }

    #[test]
    fn derived_keys_load_for_both_algos() {
        let k = derive_key_bytes(SigAlgo::EcdsaSecp256k1, "p", b"m").unwrap();
        assert!(matches!(
            key_from_bytes(SigAlgo::EcdsaSecp256k1, &k).unwrap(),
            CosignerKey::Secp256k1(_)
        ));
        let k = derive_key_bytes(SigAlgo::Ed25519, "p", b"m").unwrap();
        assert!(matches!(
            key_from_bytes(SigAlgo::Ed25519, &k).unwrap(),
            CosignerKey::Ed25519(_)
        ));
    }
}

#[cfg(test)]
mod cosigner_tests {
    use super::*;

    fn wallet() -> LocalWallet {
        "0x0123456789012345678901234567890123456789012345678901234567890123"
            .parse()
            .unwrap()
    }

    #[test]
    fn local_cosign_recovers_to_own_address_and_is_low_s() {
        let w = wallet();
        for i in 0u8..32 {
            let digest = keccak256([i]);
            let co = sign_secp256k1_with(&w, w.address(), digest).unwrap();
            assert!(!low_s(co.s).1, "s must already be low");
            let sig = EthSignature {
                r: U256::from_big_endian(&co.r),
                s: U256::from_big_endian(&co.s),
                v: 27 + co.recid as u64,
            };
            assert_eq!(sig.recover(H256::from(digest)).unwrap(), w.address());
        }
    }

    #[test]
    fn local_cosign_fails_closed_for_wrong_expected_address() {
        let w = wallet();
        let digest = keccak256(b"x");
        assert!(sign_secp256k1_with(&w, Address::repeat_byte(7), digest).is_err());
    }

    #[test]
    fn ed25519_seed_gives_stable_pubkey_and_valid_sig() {
        let seed = [7u8; 32];
        let a = keypair_from_seed(&seed).unwrap();
        let b = keypair_from_seed(&seed).unwrap();
        assert_eq!(a.pubkey(), b.pubkey());
        let sig = a.sign_message(b"hello");
        assert!(sig.verify(a.pubkey().as_ref(), b"hello"));
    }
}

#[cfg(test)]
mod evm_tests {
    use super::*;

    const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

    fn sample_auth() -> TransferAuth {
        TransferAuth {
            from: Address::repeat_byte(0x11),
            to: Address::repeat_byte(0x22),
            value: U256::from(250_000_000u64),
            valid_after: U256::zero(),
            valid_before: U256::from(1_900_000_000u64),
            nonce: [0xab; 32],
        }
    }

    fn base_ds() -> [u8; 32] {
        eip712_domain_separator("USD Coin", "2", 8453, USDC_BASE.parse().unwrap())
    }

    /// Vectors computed independently with eth_account (encode_typed_data) and a
    /// hand-rolled keccak implementation in Python.
    #[test]
    fn domain_separator_matches_reference_vector() {
        assert_eq!(
            hex::encode(base_ds()),
            "02fa7265e7c5d81118673727957699e4d68f74cd74b7db77da710fe8a2c7834f"
        );
    }

    #[test]
    fn eip3009_digest_matches_reference_vector() {
        let d = transfer_auth_digest(base_ds(), &sample_auth());
        assert_eq!(
            hex::encode(d),
            "0bb2487de6a80695875f69f2865e851a6c2c37af4d1dfa0810bbc36616420c39"
        );
    }

    #[test]
    fn digest_binds_every_field() {
        let ds = base_ds();
        let base = transfer_auth_digest(ds, &sample_auth());
        let mut seen = vec![base];
        let mut check = |a: TransferAuth| {
            let d = transfer_auth_digest(ds, &a);
            assert!(!seen.contains(&d), "field change did not change the digest");
            seen.push(d);
        };
        check(TransferAuth {
            from: Address::repeat_byte(0x33),
            ..sample_auth()
        });
        check(TransferAuth {
            to: Address::repeat_byte(0x33),
            ..sample_auth()
        });
        check(TransferAuth {
            value: U256::from(1u64),
            ..sample_auth()
        });
        check(TransferAuth {
            valid_after: U256::from(1u64),
            ..sample_auth()
        });
        check(TransferAuth {
            valid_before: U256::from(2u64),
            ..sample_auth()
        });
        check(TransferAuth {
            nonce: [0xcd; 32],
            ..sample_auth()
        });
        // a different chain / token domain must change it too
        let other = eip712_domain_separator("USD Coin", "2", 1, USDC_BASE.parse().unwrap());
        assert!(!seen.contains(&transfer_auth_digest(other, &sample_auth())));
    }

    fn wallet() -> LocalWallet {
        "0x0123456789012345678901234567890123456789012345678901234567890123"
            .parse()
            .unwrap()
    }

    fn signed_of(d: [u8; 32]) -> [u8; 32] {
        hash_message(d).0
    }

    #[test]
    fn client_sig_resolves_parity_by_recovery() {
        let w = wallet();
        let signed = signed_of(transfer_auth_digest(base_ds(), &sample_auth()));
        let sig = w.sign_hash(H256::from(signed)).unwrap();
        let (r, s, v) = verify_client_sig(
            w.address(),
            signed,
            &format!("{:#x}", sig.r),
            &format!("{:#x}", sig.s),
        )
        .unwrap();
        assert_eq!(v as u64, sig.v);
        assert_eq!(U256::from_big_endian(&r), sig.r);
        assert_eq!(U256::from_big_endian(&s), sig.s);
    }

    #[test]
    fn client_sig_high_s_is_normalized_not_rejected() {
        let w = wallet();
        let signed = signed_of(transfer_auth_digest(base_ds(), &sample_auth()));
        let sig = w.sign_hash(H256::from(signed)).unwrap();
        let high_s = secp_n() - sig.s; // malleated twin of the same signature
        let (_, s, v) = verify_client_sig(
            w.address(),
            signed,
            &format!("{:#x}", sig.r),
            &format!("{high_s:#x}"),
        )
        .unwrap();
        assert_eq!(U256::from_big_endian(&s), sig.s, "must come back low-s");
        assert_eq!(v as u64, sig.v);
    }

    #[test]
    fn client_sig_wrong_signer_or_digest_is_rejected() {
        let w = wallet();
        let signed = signed_of(transfer_auth_digest(base_ds(), &sample_auth()));
        let sig = w.sign_hash(H256::from(signed)).unwrap();
        let (r, s) = (format!("{:#x}", sig.r), format!("{:#x}", sig.s));
        // different expected client
        assert!(verify_client_sig(Address::repeat_byte(9), signed, &r, &s).is_err());
        // signature over raw D, not EIP-191(D)
        let raw = transfer_auth_digest(base_ds(), &sample_auth());
        let raw_sig = w.sign_hash(H256::from(raw)).unwrap();
        assert!(
            verify_client_sig(
                w.address(),
                signed,
                &format!("{:#x}", raw_sig.r),
                &format!("{:#x}", raw_sig.s)
            )
            .is_err()
        );
        // zero r/s
        assert!(verify_client_sig(w.address(), signed, "0x0", &s).is_err());
    }
}

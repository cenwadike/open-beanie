//! POST /api/v1/rpc/{chain}  -- read-only JSON-RPC proxy for the frontend.
//!
//! Purpose: keep provider keys server-side while the UI reads chain state.
//! It is NOT a general RPC: single requests only, a method allowlist, calls
//! restricted to configured contracts (USDC + receiver factory), bounded log
//! ranges, per-IP rate limit, short response cache, size and time limits, etc.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use log::warn;
use serde_json::{Value, json};
use tokio::sync::Mutex;

const WINDOW: Duration = Duration::from_secs(60);
const REQ_PER_WINDOW: u32 = 256;
const MAX_BODY: usize = 8 * 1024;
const MAX_UPSTREAM_BYTES: usize = 2 * 1024 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EVM_LOG_SPAN: u64 = 2000;
const MAX_STARKNET_EVENT_SPAN: u64 = 2000;
const CACHE_MAX_ENTRIES: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Evm,
    Starknet,
    Solana,
}

pub struct Upstream {
    url: String,
    family: Family,
    /// Normalized (lowercase, no 0x, no leading zeros) contracts calls may target.
    allowed: Vec<String>,
}

fn norm(s: &str) -> String {
    s.trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
        .trim_start_matches('0')
        .to_string()
}

impl Upstream {
    pub fn new(url: String, family: Family, contracts: &[&str]) -> Self {
        Self {
            url,
            family,
            allowed: contracts.iter().map(|c| norm(c)).collect(),
        }
    }

    pub fn with_allowed(url: String, family: Family, contracts: &[String]) -> Self {
        Self {
            url,
            family,
            allowed: contracts.iter().map(|c| norm(c)).collect(),
        }
    }
    fn allows(&self, addr: &str) -> bool {
        is_hex(addr, 66) && self.allowed.iter().any(|a| *a == norm(addr))
    }
}

pub struct RpcProxy {
    http: reqwest::Client,
    /// keyed by lowercase chain name: "base", "starknet", ...
    upstreams: HashMap<String, Upstream>,
    trust_proxy: bool,
    limiter: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    cache: Mutex<HashMap<String, (Instant, Value)>>,
}

impl RpcProxy {
    pub fn new(
        http: reqwest::Client,
        upstreams: HashMap<String, Upstream>,
        trust_proxy: bool,
    ) -> Self {
        Self {
            http,
            upstreams,
            trust_proxy,
            limiter: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn allow(&self, ip: IpAddr) -> bool {
        let mut m = self.limiter.lock().await;
        let now = Instant::now();
        if m.len() > 10_000 {
            m.retain(|_, (t, _)| now.duration_since(*t) < WINDOW);
        }
        let e = m.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= WINDOW {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= REQ_PER_WINDOW
    }

    async fn forward(&self, up: &Upstream, req: &Value) -> Result<Value, &'static str> {
        let res = self
            .http
            .post(&up.url)
            .timeout(UPSTREAM_TIMEOUT)
            .json(req)
            .send()
            .await
            .map_err(|_| "upstream unreachable")?;
        if !res.status().is_success() {
            warn!("rpc proxy: upstream status {}", res.status()); // never log the URL (it carries the key)
            return Err("upstream error");
        }
        let body = res.bytes().await.map_err(|_| "upstream read failed")?;
        if body.len() > MAX_UPSTREAM_BYTES {
            return Err("upstream response too large");
        }
        let v: Value =
            serde_json::from_slice(&body).map_err(|_| "upstream returned invalid JSON")?;
        if !v.is_object() {
            return Err("upstream returned unexpected shape");
        }
        Ok(v)
    }
}

pub fn router(proxy: Arc<RpcProxy>) -> Router {
    Router::new()
        .route("/api/v1/rpc/:chain", post(handle))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(proxy)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn client_ip(p: &RpcProxy, headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    if p.trust_proxy {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    addr.ip()
}

pub async fn handle(
    State(p): State<Arc<RpcProxy>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(chain): Path<String>,
    body: Bytes,
) -> Response {
    let chain = chain.to_ascii_lowercase();
    let Some(up) = p.upstreams.get(&chain) else {
        return err(StatusCode::NOT_FOUND, "unknown chain");
    };
    if !p.allow(client_ip(&p, &headers, addr)).await {
        return err(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid JSON"),
    };
    let ttl = match validate(up, &req) {
        Ok(t) => t,
        Err(m) => return err(StatusCode::BAD_REQUEST, m),
    };

    let id = req["id"].clone();
    let key = format!("{chain}|{}|{}", req["method"], req["params"]);

    if !ttl.is_zero() {
        let c = p.cache.lock().await;
        if let Some((exp, v)) = c.get(&key) {
            if *exp > Instant::now() {
                let mut v = v.clone();
                v["id"] = id;
                return (StatusCode::OK, Json(v)).into_response();
            }
        }
    }

    match p.forward(up, &req).await {
        Ok(mut v) => {
            if !ttl.is_zero() && v.get("result").map_or(false, |r| !r.is_null()) {
                let mut c = p.cache.lock().await;
                if c.len() >= CACHE_MAX_ENTRIES {
                    let now = Instant::now();
                    c.retain(|_, (exp, _)| *exp > now);
                    if c.len() >= CACHE_MAX_ENTRIES {
                        c.clear();
                    }
                }
                c.insert(key, (Instant::now() + ttl, v.clone()));
            }
            v["id"] = id;
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(m) => err(StatusCode::BAD_GATEWAY, m),
    }
}

// ---- validation -------------------------------------------------------------

fn is_hex(s: &str, max_len: usize) -> bool {
    s.len() >= 2
        && s.len() <= max_len
        && s.starts_with("0x")
        && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_u64(v: Option<&Value>) -> Option<u64> {
    let s = v?.as_str()?;
    if !is_hex(s, 18) || s.len() == 2 {
        return None;
    }
    u64::from_str_radix(&s[2..], 16).ok()
}

fn only_keys(o: &serde_json::Map<String, Value>, ok: &[&str]) -> bool {
    o.keys().all(|k| ok.contains(&k.as_str()))
}

fn is_latest(v: &Value) -> bool {
    v.as_str() == Some("latest")
}

// ---- Solana helpers ------------------------------------------------------
//
// Unlike EVM/Starknet there is no contract allowlist: Solana receivers are
// random keypairs created per lane, so their addresses cannot be known up
// front. The guard is shape + method + option allowlists + the per-IP limit.

const MAX_SOLANA_SIGNATURES: u64 = 1000;
const MAX_SOLANA_MULTIPLE_ACCOUNTS: usize = 4;
const MAX_SOLANA_DATA_SLICE: u64 = 64;

fn is_b58(s: &str, min: usize, max: usize) -> bool {
    const ALPHABET: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    s.len() >= min && s.len() <= max && s.chars().all(|c| ALPHABET.contains(c))
}

fn is_sol_pubkey(v: &Value) -> bool {
    v.as_str().map_or(false, |s| is_b58(s, 32, 44))
}

fn is_sol_signature(v: &Value) -> bool {
    v.as_str().map_or(false, |s| is_b58(s, 64, 90))
}

/// Optional trailing options object: only `allowed` keys, commitment confirmed|finalized.
fn sol_opts<'a>(
    v: Option<&'a Value>,
    allowed: &[&str],
) -> Result<Option<&'a serde_json::Map<String, Value>>, &'static str> {
    match v {
        None => Ok(None),
        Some(Value::Object(o)) => {
            if !only_keys(o, allowed) {
                return Err("unsupported option");
            }
            if let Some(c) = o.get("commitment") {
                if !matches!(c.as_str(), Some("confirmed" | "finalized")) {
                    return Err("commitment must be confirmed or finalized");
                }
            }
            Ok(Some(o))
        }
        _ => Err("bad options"),
    }
}

/// `encoding` may be absent or "base64" (never jsonParsed/base58 for raw account data).
fn sol_base64_only(o: Option<&serde_json::Map<String, Value>>) -> bool {
    o.and_then(|o| o.get("encoding"))
        .map_or(true, |e| e.as_str() == Some("base64"))
}

fn sol_data_slice_ok(o: Option<&serde_json::Map<String, Value>>) -> bool {
    match o.and_then(|o| o.get("dataSlice")) {
        None => true,
        Some(Value::Object(d)) => {
            only_keys(d, &["offset", "length"])
                && d.get("offset")
                    .and_then(Value::as_u64)
                    .map_or(false, |n| n <= 1024)
                && d.get("length")
                    .and_then(Value::as_u64)
                    .map_or(false, |n| n <= MAX_SOLANA_DATA_SLICE)
        }
        _ => false,
    }
}

/// Returns the cache TTL for an allowed request, or why it was rejected.
fn validate(up: &Upstream, req: &Value) -> Result<Duration, &'static str> {
    let o = req.as_object().ok_or("batch requests are not supported")?;
    if !only_keys(o, &["jsonrpc", "id", "method", "params"]) {
        return Err("unexpected field");
    }
    if o.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("jsonrpc must be 2.0");
    }
    match o.get("id") {
        Some(Value::Number(_)) => {}
        Some(Value::String(s)) if s.len() <= 64 => {}
        _ => return Err("invalid id"),
    }
    let method = o
        .get("method")
        .and_then(Value::as_str)
        .ok_or("missing method")?;
    let params = o
        .get("params")
        .and_then(Value::as_array)
        .ok_or("params must be an array")?;
    let secs = Duration::from_secs;

    match (up.family, method) {
        // ---- EVM
        (Family::Evm, "eth_blockNumber") => {
            if !params.is_empty() {
                return Err("bad params");
            }
            Ok(secs(2))
        }
        (Family::Evm, "eth_chainId") => {
            if !params.is_empty() {
                return Err("bad params");
            }
            Ok(secs(60))
        }
        (Family::Evm, "eth_call") => {
            let [call, tag] = params.as_slice() else {
                return Err("bad params");
            };
            let c = call.as_object().ok_or("bad call object")?;
            if !only_keys(c, &["to", "data"]) {
                return Err("unsupported call fields");
            }
            let to = c.get("to").and_then(Value::as_str).ok_or("missing to")?;
            let data = c
                .get("data")
                .and_then(Value::as_str)
                .ok_or("missing data")?;
            if !up.allows(to) || !is_hex(data, 2 + 2 * 2048) {
                return Err("call not permitted");
            }
            if !is_latest(tag) {
                return Err("only latest is supported");
            }
            Ok(secs(3))
        }
        (Family::Evm, "eth_getCode") => {
            let [addr, tag] = params.as_slice() else {
                return Err("bad params");
            };
            if !addr.as_str().map_or(false, |a| is_hex(a, 42)) || !is_latest(tag) {
                return Err("bad params");
            }
            Ok(secs(10))
        }
        (Family::Evm, "eth_getLogs") => {
            let [f] = params.as_slice() else {
                return Err("bad params");
            };
            let f = f.as_object().ok_or("bad filter")?;
            if !only_keys(f, &["address", "fromBlock", "toBlock", "topics"]) {
                return Err("unsupported filter fields");
            }
            let addr = f
                .get("address")
                .and_then(Value::as_str)
                .ok_or("address required")?;
            if !up.allows(addr) {
                return Err("address not permitted");
            }
            let from = hex_u64(f.get("fromBlock")).ok_or("fromBlock must be hex")?;
            let to = hex_u64(f.get("toBlock")).ok_or("toBlock must be hex")?;
            if to < from || to - from > MAX_EVM_LOG_SPAN {
                return Err("block range too large");
            }
            match f.get("topics") {
                None => {}
                Some(Value::Array(t))
                    if t.len() <= 4
                        && t.iter().all(|x| {
                            x.is_null() || x.as_str().map_or(false, |h| is_hex(h, 66))
                        }) => {}
                _ => return Err("bad topics"),
            }
            Ok(secs(5))
        }

        // ---- Starknet
        (Family::Starknet, "starknet_blockNumber") => {
            if !params.is_empty() {
                return Err("bad params");
            }
            Ok(secs(2))
        }
        (Family::Starknet, "starknet_chainId") => {
            if !params.is_empty() {
                return Err("bad params");
            }
            Ok(secs(60))
        }
        (Family::Starknet, "starknet_call") => {
            let [call, tag] = params.as_slice() else {
                return Err("bad params");
            };
            let c = call.as_object().ok_or("bad call object")?;
            if !only_keys(c, &["contract_address", "entry_point_selector", "calldata"]) {
                return Err("unsupported call fields");
            }
            let to = c
                .get("contract_address")
                .and_then(Value::as_str)
                .ok_or("missing contract_address")?;
            let sel = c
                .get("entry_point_selector")
                .and_then(Value::as_str)
                .ok_or("missing selector")?;
            let data = c
                .get("calldata")
                .and_then(Value::as_array)
                .ok_or("missing calldata")?;
            if !up.allows(to)
                || !is_hex(sel, 66)
                || data.len() > 16
                || !data
                    .iter()
                    .all(|x| x.as_str().map_or(false, |h| is_hex(h, 66)))
            {
                return Err("call not permitted");
            }
            if !is_latest(tag) {
                return Err("only latest is supported");
            }
            Ok(secs(3))
        }
        (Family::Starknet, "starknet_getClassHashAt") => {
            let [tag, addr] = params.as_slice() else {
                return Err("bad params");
            };
            if !is_latest(tag) || !addr.as_str().map_or(false, |a| is_hex(a, 66)) {
                return Err("bad params");
            }
            Ok(secs(10))
        }
        (Family::Starknet, "starknet_getEvents") => {
            let [f] = params.as_slice() else {
                return Err("bad params");
            };
            let f = f.as_object().ok_or("bad filter")?;
            if !only_keys(
                f,
                &[
                    "from_block",
                    "to_block",
                    "address",
                    "keys",
                    "chunk_size",
                    "continuation_token",
                ],
            ) {
                return Err("unsupported filter fields");
            }
            let addr = f
                .get("address")
                .and_then(Value::as_str)
                .ok_or("address required")?;
            if !up.allows(addr) {
                return Err("address not permitted");
            }
            let blk = |k: &str| f.get(k)?.get("block_number")?.as_u64();
            let (from, to) = (
                blk("from_block").ok_or("from_block required")?,
                blk("to_block").ok_or("to_block required")?,
            );
            if to < from || to - from > MAX_STARKNET_EVENT_SPAN {
                return Err("block range too large");
            }
            if f.get("chunk_size")
                .and_then(Value::as_u64)
                .map_or(true, |n| n == 0 || n > 100)
            {
                return Err("chunk_size must be 1..=100");
            }
            if let Some(t) = f.get("continuation_token") {
                if !t.as_str().map_or(false, |s| s.len() <= 128) {
                    return Err("bad continuation_token");
                }
            }
            match f.get("keys") {
                None => {}
                Some(Value::Array(ks))
                    if ks.len() <= 4
                        && ks.iter().all(|k| {
                            k.as_array().map_or(false, |a| {
                                a.len() <= 4
                                    && a.iter()
                                        .all(|x| x.as_str().map_or(false, |h| is_hex(h, 66)))
                            })
                        }) => {}
                _ => return Err("bad keys"),
            }
            Ok(secs(5))
        }

        // ---- Solana
        (Family::Solana, "getSlot" | "getBlockHeight" | "getLatestBlockhash") => {
            if params.len() > 1 {
                return Err("bad params");
            }
            sol_opts(params.first(), &["commitment"])?;
            Ok(secs(2))
        }
        (Family::Solana, "getTokenAccountBalance") => {
            if params.is_empty() || params.len() > 2 || !is_sol_pubkey(&params[0]) {
                return Err("bad params");
            }
            sol_opts(params.get(1), &["commitment"])?;
            Ok(secs(3))
        }
        (Family::Solana, "getAccountInfo") => {
            if params.is_empty() || params.len() > 2 || !is_sol_pubkey(&params[0]) {
                return Err("bad params");
            }
            let o = sol_opts(params.get(1), &["commitment", "encoding", "dataSlice"])?;
            if !sol_base64_only(o) || !sol_data_slice_ok(o) {
                return Err("unsupported encoding or dataSlice");
            }
            Ok(secs(3))
        }
        (Family::Solana, "getMultipleAccounts") => {
            if params.is_empty() || params.len() > 2 {
                return Err("bad params");
            }
            let keys = params[0].as_array().ok_or("bad params")?;
            if keys.is_empty()
                || keys.len() > MAX_SOLANA_MULTIPLE_ACCOUNTS
                || !keys.iter().all(is_sol_pubkey)
            {
                return Err("bad params");
            }
            let o = sol_opts(params.get(1), &["commitment", "encoding", "dataSlice"])?;
            if !sol_base64_only(o) || !sol_data_slice_ok(o) {
                return Err("unsupported encoding or dataSlice");
            }
            Ok(secs(3))
        }
        (Family::Solana, "getSignaturesForAddress") => {
            if params.is_empty() || params.len() > 2 || !is_sol_pubkey(&params[0]) {
                return Err("bad params");
            }
            let o = sol_opts(params.get(1), &["limit", "commitment", "before", "until"])?;
            if let Some(o) = o {
                if let Some(l) = o.get("limit") {
                    if !l
                        .as_u64()
                        .map_or(false, |n| (1..=MAX_SOLANA_SIGNATURES).contains(&n))
                    {
                        return Err("limit must be 1..=1000");
                    }
                }
                for k in ["before", "until"] {
                    if let Some(s) = o.get(k) {
                        if !is_sol_signature(s) {
                            return Err("bad signature cursor");
                        }
                    }
                }
            }
            Ok(secs(3))
        }
        (Family::Solana, "getTransaction") => {
            if params.len() != 2 || !is_sol_signature(&params[0]) {
                return Err("bad params");
            }
            let o = sol_opts(
                params.get(1),
                &["encoding", "commitment", "maxSupportedTransactionVersion"],
            )?
            .ok_or("options required")?;
            if !matches!(
                o.get("encoding").and_then(Value::as_str),
                Some("jsonParsed" | "json")
            ) {
                return Err("encoding must be jsonParsed or json");
            }
            if o.get("maxSupportedTransactionVersion")
                .and_then(Value::as_u64)
                != Some(0)
            {
                return Err("maxSupportedTransactionVersion must be 0");
            }
            // A finalized transaction never changes; a confirmed one is re-read soon.
            if o.get("commitment").and_then(Value::as_str) == Some("finalized") {
                Ok(secs(300))
            } else {
                Ok(secs(5))
            }
        }

        _ => Err("method not permitted"),
    }
}

// auth.rs
//
// WebAuthn verification (webauthn-rs) + rate limiting.
//
// Design: every action that needs a passkey (create, claim, payment) goes through
// a generic two-step ceremony bound to an action-specific `binding` string:
//
//   POST /api/v1/webauthn/register/start   (once per browser, ever)
//   POST /api/v1/webauthn/register/finish
//   POST /api/v1/webauthn/auth/start       { credential_id, binding }
//   POST /api/v1/webauthn/auth/finish      -> { verified_token }
//
// The business routes (create_route.rs, stealth_route.rs) never touch
// WebAuthn headers/parsing directly. They just call
// `state.auth.consume_verified(token, expected_binding)` and get back a
// verified credential_id, or a rejection. This replaces the old hand-rolled
// clientDataJSON/authenticatorData parsing entirely.
//
// Storage: in-memory only (HashMap behind a Mutex), per your call — no DB.
// This means: registered passkeys and in-flight ceremonies are lost on
// restart, and this does not work across multiple server instances without
// a shared store. Accepted tradeoff, not silently hidden.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::models::{AppState, err};

const REG_CEREMONY_TTL: Duration = Duration::from_secs(120);
const AUTH_CEREMONY_TTL: Duration = Duration::from_secs(120);
const VERIFIED_TOKEN_TTL: Duration = Duration::from_secs(60);

// ---------- State ----------

pub struct AuthState {
    webauthn: Webauthn,
    /// credential_id (base64url) -> Passkey (public key + sign counter).
    /// This is the one piece of state WebAuthn signature verification cannot
    /// exist without — see conversation notes. Everything else here is
    /// disposable ceremony bookkeeping.
    passkeys: Mutex<HashMap<String, Passkey>>,
    reg_ceremonies: Mutex<HashMap<String, (PasskeyRegistration, Instant)>>,
    auth_ceremonies: Mutex<HashMap<String, (PasskeyAuthentication, String, Instant)>>,
    /// verified_token -> (credential_id, binding, expires_at). Single-use.
    verified: Mutex<HashMap<String, (String, String, Instant)>>,
}

impl AuthState {
    pub fn new(rp_id: &str, rp_origin: &str) -> Self {
        let origin = Url::parse(rp_origin).expect("invalid RP origin URL");
        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .expect("invalid WebAuthn RP configuration")
            .rp_name("Beanie")
            .build()
            .expect("failed to build WebAuthn instance");

        Self {
            webauthn,
            passkeys: Mutex::new(HashMap::new()),
            reg_ceremonies: Mutex::new(HashMap::new()),
            auth_ceremonies: Mutex::new(HashMap::new()),
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn insert_reg_ceremony(&self, token: String, state: PasskeyRegistration) {
        let mut map = self.reg_ceremonies.lock().unwrap();
        map.retain(|_, (_, exp)| *exp > Instant::now());
        map.insert(token, (state, Instant::now() + REG_CEREMONY_TTL));
    }

    fn take_reg_ceremony(&self, token: &str) -> Option<PasskeyRegistration> {
        let mut map = self.reg_ceremonies.lock().unwrap();
        let (state, exp) = map.remove(token)?;
        if exp < Instant::now() {
            return None;
        }
        Some(state)
    }

    fn insert_auth_ceremony(&self, token: String, state: PasskeyAuthentication, binding: String) {
        let mut map = self.auth_ceremonies.lock().unwrap();
        map.retain(|_, (_, _, exp)| *exp > Instant::now());
        map.insert(token, (state, binding, Instant::now() + AUTH_CEREMONY_TTL));
    }

    fn take_auth_ceremony(&self, token: &str) -> Option<(PasskeyAuthentication, String)> {
        let mut map = self.auth_ceremonies.lock().unwrap();
        let (state, binding, exp) = map.remove(token)?;
        if exp < Instant::now() {
            return None;
        }
        Some((state, binding))
    }

    fn store_passkey(&self, credential_id: String, passkey: Passkey) {
        self.passkeys.lock().unwrap().insert(credential_id, passkey);
    }

    fn get_passkey(&self, credential_id: &str) -> Option<Passkey> {
        self.passkeys.lock().unwrap().get(credential_id).cloned()
    }

    fn store_verified(&self, token: String, credential_id: String, binding: String) {
        let mut map = self.verified.lock().unwrap();
        map.retain(|_, (_, _, exp)| *exp > Instant::now());
        map.insert(
            token,
            (credential_id, binding, Instant::now() + VERIFIED_TOKEN_TTL),
        );
    }

    /// Single-use: consumes the token, checks it's fresh and bound to
    /// exactly the action the caller expects, returns the verified
    /// credential_id on success.
    pub fn consume_verified(&self, token: &str, expected_binding: &str) -> Option<String> {
        let mut map = self.verified.lock().unwrap();
        map.retain(|_, (_, _, exp)| *exp > Instant::now());
        let (credential_id, binding, exp) = map.remove(token)?;
        if exp < Instant::now() {
            return None;
        }
        if !binding.eq_ignore_ascii_case(&expected_binding) {
            return None;
        }
        Some(credential_id)
    }
}

fn base64url_cred_id(id: &CredentialID) -> String {
    // CredentialID derefs to &[u8]; webauthn-rs re-exports base64 URL encoding
    // helpers under webauthn_rs::prelude::Base64UrlSafeData in most 0.5.x
    // versions — using a direct encode here to avoid depending on that.
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(id.as_ref())
}

// ---------- Handlers ----------

#[derive(Serialize)]
pub struct RegStartResp {
    session_token: String,
    options: CreationChallengeResponse,
}

pub async fn register_start(State(state): State<AppState>) -> Response {
    let user_id = Uuid::new_v4();
    match state
        .auth
        .webauthn
        .start_passkey_registration(user_id, "beanie-user", "Beanie", None)
    {
        Ok((ccr, reg_state)) => {
            let token = Uuid::new_v4().to_string();
            state.auth.insert_reg_ceremony(token.clone(), reg_state);
            Json(RegStartResp {
                session_token: token,
                options: ccr,
            })
            .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not start passkey registration: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub struct RegFinishReq {
    session_token: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Serialize)]
pub struct RegFinishResp {
    credential_id: String,
}

pub async fn register_finish(
    State(state): State<AppState>,
    Json(payload): Json<RegFinishReq>,
) -> Response {
    let reg_state = match state.auth.take_reg_ceremony(&payload.session_token) {
        Some(s) => s,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "Unknown or expired registration session",
            );
        }
    };

    match state
        .auth
        .webauthn
        .finish_passkey_registration(&payload.credential, &reg_state)
    {
        Ok(passkey) => {
            let credential_id = base64url_cred_id(passkey.cred_id());
            state.auth.store_passkey(credential_id.clone(), passkey);
            Json(RegFinishResp { credential_id }).into_response()
        }
        Err(e) => err(
            StatusCode::UNAUTHORIZED,
            &format!("Passkey registration verification failed: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub struct AuthStartReq {
    credential_id: String,
    /// Action-specific string this ceremony will be locked to, e.g.
    /// "create:Base:0xabc..." or "claim:Starknet:0xdef...:0x123...".
    binding: String,
}

#[derive(Serialize)]
pub struct AuthStartResp {
    session_token: String,
    options: RequestChallengeResponse,
}

pub async fn auth_start(
    State(state): State<AppState>,
    Json(payload): Json<AuthStartReq>,
) -> Response {
    let passkey = match state.auth.get_passkey(&payload.credential_id) {
        Some(p) => p,
        None => {
            return err(
                StatusCode::NOT_FOUND,
                "Unknown credential_id — register first",
            );
        }
    };

    match state.auth.webauthn.start_passkey_authentication(&[passkey]) {
        Ok((rcr, auth_state)) => {
            let token = Uuid::new_v4().to_string();
            state
                .auth
                .insert_auth_ceremony(token.clone(), auth_state, payload.binding);
            Json(AuthStartResp {
                session_token: token,
                options: rcr,
            })
            .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not start passkey verification: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub struct AuthFinishReq {
    session_token: String,
    credential: PublicKeyCredential,
}

#[derive(Serialize)]
pub struct AuthFinishResp {
    verified_token: String,
}

pub async fn auth_finish(
    State(state): State<AppState>,
    Json(payload): Json<AuthFinishReq>,
) -> Response {
    let (auth_state, binding) = match state.auth.take_auth_ceremony(&payload.session_token) {
        Some(s) => s,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "Unknown or expired verification session",
            );
        }
    };

    match state
        .auth
        .webauthn
        .finish_passkey_authentication(&payload.credential, &auth_state)
    {
        Ok(result) => {
            let credential_id = base64url_cred_id(result.cred_id());
            let verified_token = Uuid::new_v4().to_string();
            state
                .auth
                .store_verified(verified_token.clone(), credential_id, binding);
            Json(AuthFinishResp { verified_token }).into_response()
        }
        Err(e) => err(
            StatusCode::UNAUTHORIZED,
            &format!("Passkey verification failed: {e}"),
        ),
    }
}

// ---------- Rate limiter (single call site, no more double-counting) ----------

pub struct RateLimiter {
    window: Duration,
    ip_limit: u32,
    address_limit: u32,
    credential_limit: u32,
    ip_hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    address_hits: Mutex<HashMap<String, (Instant, u32)>>,
    credential_hits: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(ip_limit: u32, address_limit: u32, credential_limit: u32, window: Duration) -> Self {
        Self {
            window,
            ip_limit,
            address_limit,
            credential_limit,
            ip_hits: Mutex::new(HashMap::new()),
            address_hits: Mutex::new(HashMap::new()),
            credential_hits: Mutex::new(HashMap::new()),
        }
    }

    /// The only rate-limit entry point. Called once per handler, after the
    /// credential_id has already been through `AuthState::consume_verified`
    /// — so this bucket is now keyed on a proven identity, not a claimed one.
    pub fn check(
        &self,
        ip: IpAddr,
        derived_address: &str,
        credential_id: &str,
    ) -> Result<(), &'static str> {
        let now = Instant::now();

        {
            let mut ips = self.ip_hits.lock().unwrap();
            let entry = ips.entry(ip).or_insert((now, 0));
            if now.duration_since(entry.0) > self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.ip_limit {
                return Err("IP rate limit exceeded, try again later");
            }
            entry.1 += 1;
        }

        {
            let mut credentials = self.credential_hits.lock().unwrap();
            let entry = credentials
                .entry(credential_id.to_lowercase())
                .or_insert((now, 0));
            if now.duration_since(entry.0) > self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.credential_limit {
                return Err("Passkey credential rate limit exceeded");
            }
            entry.1 += 1;
        }

        {
            let mut addresses = self.address_hits.lock().unwrap();
            let entry = addresses
                .entry(derived_address.to_lowercase())
                .or_insert((now, 0));
            if now.duration_since(entry.0) > self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.address_limit {
                return Err("Target address execution limit exceeded");
            }
            entry.1 += 1;
        }

        Ok(())
    }
}

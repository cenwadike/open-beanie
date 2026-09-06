use axum::{
    extract::{FromRef, FromRequestParts},
    http::{StatusCode, request::Parts},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;

use crate::models::{AppState, err};

#[derive(Debug, Deserialize)]
pub struct RawClientDataJson {
    #[serde(rename = "type")]
    pub type_field: String,
    pub challenge: String,
    pub origin: String,
}

#[allow(unused)]
pub struct PasskeyAuth {
    pub credential_id: String,
    client_data: RawClientDataJson,
    auth_data_bytes: Vec<u8>,
}

impl<S> FromRequestParts<S> for PasskeyAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        // 1. Extract Headers
        let cred_id = parts
            .headers
            .get("X-Passkey-Credential-Id")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                err(
                    StatusCode::UNAUTHORIZED,
                    "Missing X-Passkey-Credential-Id header",
                )
            })?;

        let client_data_b64 = parts
            .headers
            .get("X-Passkey-Client-Data")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                err(
                    StatusCode::UNAUTHORIZED,
                    "Missing X-Passkey-Client-Data header",
                )
            })?;

        let auth_data_b64 = parts
            .headers
            .get("X-Passkey-Auth-Data")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                err(
                    StatusCode::UNAUTHORIZED,
                    "Missing X-Passkey-Auth-Data header",
                )
            })?;

        let expected_tx_hash = parts
            .headers
            .get("X-Passkey-Tx-Hash")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "Missing X-Passkey-Tx-Hash header"))?;

        // 2. Decode Raw Components
        let client_data_bytes = URL_SAFE_NO_PAD.decode(client_data_b64).map_err(|_| {
            err(
                StatusCode::BAD_REQUEST,
                "Invalid Base64URL client_data_json",
            )
        })?;

        let auth_data_bytes = URL_SAFE_NO_PAD.decode(auth_data_b64).map_err(|_| {
            err(
                StatusCode::BAD_REQUEST,
                "Invalid Base64URL authenticator_data",
            )
        })?;

        // 3. Parse ClientDataJSON
        let client_data: RawClientDataJson = serde_json::from_slice(&client_data_bytes)
            .map_err(|_| err(StatusCode::BAD_REQUEST, "Malformed clientDataJSON payload"))?;

        // 4. Inlined Challenge Validation
        let mut hasher = Sha256::new();
        hasher.update(expected_tx_hash.as_bytes());
        let computed_digest = hasher.finalize();
        let expected_challenge_b64 = URL_SAFE_NO_PAD.encode(computed_digest);

        if client_data.challenge != expected_challenge_b64 {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "WebAuthn challenge mismatch for transaction payload",
            ));
        }

        // 5. Validate Type & Origin
        if client_data.type_field != "webauthn.get" {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Invalid WebAuthn operation type",
            ));
        }

        if client_data.origin != "https://beanie.io" {
            return Err(err(StatusCode::UNAUTHORIZED, "WebAuthn origin mismatch"));
        }

        // 6. Check User Verification (UV) Flag
        if auth_data_bytes.len() < 37 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Authenticator data buffer too short",
            ));
        }
        if (auth_data_bytes[32] & 0x04) == 0 {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "User verification (UV) flag required",
            ));
        }

        // 7. Enforce Rate Limiter Bucket
        if let Err(msg) = app_state.limiter.check_credential(cred_id) {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, msg));
        }

        Ok(PasskeyAuth {
            credential_id: cred_id.to_string(),
            client_data,
            auth_data_bytes,
        })
    }
}

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

    /// Primary checking method enforcing IP, Target Address, and Passkey Credential ID limits
    pub fn check(
        &self,
        ip: IpAddr,
        derived_address: &str,
        credential_id: &str,
    ) -> Result<(), &'static str> {
        let now = Instant::now();

        // 1. IP Bucket Check
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

        // 2. Passkey Credential ID Bucket Check (Anti-Sybil / Anti-IP Rotation)
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

        // 3. Derived Address Bucket Check
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

    /// Standalone passkey check for middleware extraction phase
    fn check_credential(&self, credential_id: &str) -> Result<(), &'static str> {
        let now = Instant::now();
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

        Ok(())
    }
}

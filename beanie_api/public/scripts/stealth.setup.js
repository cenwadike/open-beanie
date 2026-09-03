// stealth-setup.js
//
// Zero-Backend-State Setup Flow: Authenticates WebAuthn PRF and pre-generates
// client-side spending public keys without transmitting state or metadata.

import { ec } from "https://cdn.jsdelivr.net/npm/starknet@6/dist/index.js";
import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";

const RP_ID = "beanie.io";
const STARK_CURVE_ORDER = ec.starkCurve.CURVE.n;
const SECP256K1_ORDER = BigInt("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141");

export class StealthSetupError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function supportsWebAuthnPRF() {
  return (
    typeof window !== "undefined" &&
    window.PublicKeyCredential !== undefined &&
    typeof navigator.credentials?.create === "function"
  );
}

const _credentialIdCache = new Map();

async function getStoredCredentialId(laneId) {
  return _credentialIdCache.get(laneId) ?? null;
}

async function storeCredentialId(laneId, credentialId) {
  _credentialIdCache.set(laneId, credentialId);
}

function bufferToBase64Url(buffer) {
  return btoa(String.fromCharCode(...new Uint8Array(buffer)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function base64UrlToBuffer(base64url) {
  const base64 = base64url.replace(/-/g, "+").replace(/_/g, "/");
  const padded = base64.padEnd(base64.length + ((4 - (base64.length % 4)) % 4), "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}

async function registerCredential(laneId) {
  const challenge = crypto.getRandomValues(new Uint8Array(32));
  const userId = crypto.getRandomValues(new Uint8Array(16));

  let credential;
  try {
    credential = await navigator.credentials.create({
      publicKey: {
        challenge,
        rp: { name: "Beanie", id: RP_ID },
        user: {
          id: userId,
          name: `beanie-privacy-${laneId}`,
          displayName: "Beanie Privacy Key",
        },
        pubKeyCredParams: [{ type: "public-key", alg: -7 }],
        authenticatorSelection: { residentKey: "preferred", userVerification: "required" },
        extensions: { prf: {} },
      },
    });
  } catch (err) {
    throw new StealthSetupError(
      "REGISTRATION_FAILED",
      `Passkey registration failed or was cancelled: ${err.message}`
    );
  }

  const prfResults = credential.getClientExtensionResults()?.prf;
  if (!prfResults?.enabled) {
    throw new StealthSetupError(
      "PRF_NOT_SUPPORTED",
      "Your device created a passkey, but it doesn't support the PRF extension."
    );
  }

  const credentialId = bufferToBase64Url(credential.rawId);
  await storeCredentialId(laneId, credentialId);
  return credentialId;
}

async function deriveLaneSalt(laneId) {
  const encoder = new TextEncoder();
  const data = encoder.encode(`beanie-stealth-salt-v1:${laneId}`);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return new Uint8Array(digest);
}

async function evaluatePRF(credentialId, salt) {
  const challenge = crypto.getRandomValues(new Uint8Array(32));

  let assertion;
  try {
    assertion = await navigator.credentials.get({
      publicKey: {
        challenge,
        rpId: RP_ID,
        allowCredentials: [{ id: base64UrlToBuffer(credentialId), type: "public-key" }],
        userVerification: "required",
        extensions: { prf: { eval: { first: salt } } },
      },
    });
  } catch (err) {
    throw new StealthSetupError(
      "PRF_EVAL_FAILED",
      `Key derivation failed: ${err.message}`
    );
  }

  const prfResult = assertion.getClientExtensionResults()?.prf?.results?.first;
  if (!prfResult) {
    throw new StealthSetupError("PRF_EVAL_EMPTY", "No derived key material returned.");
  }

  return new Uint8Array(prfResult);
}

function bytesToScalar(bytes, curveOrder) {
  const hex = Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
  const n = BigInt("0x" + hex);
  return n % curveOrder;
}

async function hkdf(ikm, info) {
  const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: new Uint8Array(32),
      info: new TextEncoder().encode(info),
    },
    key,
    256
  );
  return new Uint8Array(bits);
}

async function derivePublicKeys(beanMasterSecret) {
  // ---- 1. Starknet Spending Key ----
  const starkSpendPriv = await hkdf(beanMasterSecret, "spend-v1:starknet");
  const G_stark = ec.starkCurve.ProjectivePoint.BASE;
  const starkPoint = G_stark.multiply(bytesToScalar(starkSpendPriv, STARK_CURVE_ORDER));
  const starkSpendingPubkey = "0x" + starkPoint.x.toString(16);

  // ---- 2. EVM Spending Key ----
  const evmSpendPriv = await hkdf(beanMasterSecret, "spend-v1:evm");
  const evmSpendScalar = bytesToScalar(evmSpendPriv, SECP256K1_ORDER);
  const evmSpendWallet = new ethers.Wallet("0x" + evmSpendScalar.toString(16).padStart(64, "0"));

  return {
    starknet: {
      spending_pubkey: starkSpendingPubkey,
    },
    evm: {
      spending_address: evmSpendWallet.address,
      spending_pubkey: evmSpendWallet.signingKey.publicKey,
    },
  };
}

export async function setupStealthForLane(laneId, statusCallback = () => { }) {
  if (!supportsWebAuthnPRF()) {
    throw new StealthSetupError(
      "UNSUPPORTED_PLATFORM",
      "Private payments require a browser/OS with WebAuthn PRF support."
    );
  }

  statusCallback("Checking for existing passkey...");
  let credentialId = await getStoredCredentialId(laneId);

  if (!credentialId) {
    statusCallback("Setting up passkey...");
    credentialId = await registerCredential(laneId);
  }

  statusCallback("Deriving keys...");
  const salt = await deriveLaneSalt(laneId);
  const beanMasterSecret = await evaluatePRF(credentialId, salt);
  const keys = await derivePublicKeys(beanMasterSecret);

  statusCallback("Done.");
  return keys;
}
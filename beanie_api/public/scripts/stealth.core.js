// stealth.core.js
//
// Canonical private-lane derivation. The ONLY place stealth account math
// lives; lane creation (lane.js) and scan/claim (stealth.js) both use it.
//
// MODEL
//   laneSalt(laneId)  = SHA-256("beanie-stealth-salt-v1:" + laneId)
//   laneSecret        = passkey PRF(laneSalt)        (32 bytes, one per lane)
//   spend key         = HKDF(laneSecret, chain + laneId + index) -> curve scalar
//
// Every lane has an independent secret, so exposing one lane's secret does not
// expose another's. The salt is not secret; the authenticator holds the secret.
// Lane ids are random (see generateLaneId) and are kept in the lane backup.
//
// One stealth account per lane, on the SETTLEMENT chain only. Other chains
// reach it via CCTP.
//
// Chain parameters come from chains.js and are validated with stealthReady();
// with placeholder config this module refuses to derive (fails closed) because
// an address derived from dummy parameters cannot be recovered.

import { ec as starkEc, CallData, hash } from "./starknet.js";
import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";
import { chainByKey, stealthReady } from "./chains.js";

const SECP256K1_ORDER = BigInt("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141");
const enc = new TextEncoder();

export const bytesToHex = (bytes) =>
  Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** Best-effort zeroing of secret material. */
export function wipe(bytes) {
  try {
    if (bytes && typeof bytes.fill === "function") bytes.fill(0);
  } catch {
    /* ignore */
  }
}

export async function laneSalt(laneId) {
  const digest = await crypto.subtle.digest("SHA-256", enc.encode(`beanie-stealth-salt-v1:${laneId}`));
  return new Uint8Array(digest);
}

/**
 * Unguessable lane id: sha256(seed || 0x00 || 16 random bytes || timestamp).
 * 69 chars, within the API's 1..128 limit.
 */
export async function generateLaneId(seed = "") {
  const rand = crypto.getRandomValues(new Uint8Array(16));
  const parts = [enc.encode(String(seed)), new Uint8Array([0]), rand, enc.encode(String(Date.now()))];
  const buf = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let off = 0;
  for (const p of parts) {
    buf.set(p, off);
    off += p.length;
  }
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", buf));
  return `lane_${bytesToHex(digest)}`;
}

async function hkdf(ikm, info, length = 64) {
  const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: new Uint8Array(32), info: enc.encode(info) },
    key,
    length * 8
  );
  return new Uint8Array(bits);
}

// 64 bytes reduced mod (n-1), +1: uniform enough (bias ~2^-256) and never zero.
function bytesToScalar(bytes, order) {
  return (BigInt(`0x${bytesToHex(bytes)}`) % (order - 1n)) + 1n;
}

function requireReady(chainKey) {
  const chain = chainByKey(chainKey);
  if (!chain) throw new Error(`Unknown chain "${chainKey}".`);
  if (!stealthReady(chain)) {
    throw new Error(`Private lanes are not enabled on ${chain.name} yet.`);
  }
  return chain;
}

const feltHex = (v) => `0x${BigInt(v).toString(16).padStart(64, "0")}`;

async function derive(laneSecret, laneId, index, chainKey, withPrivate) {
  if (!(laneSecret instanceof Uint8Array) || laneSecret.length < 32) {
    throw new Error("Invalid lane secret.");
  }
  if (!Number.isInteger(index) || index < 0) throw new Error("Invalid derivation index.");
  const chain = requireReady(chainKey);
  const info = `beanie-spend-v1:${chain.key}:${laneId}:${index}`;

  if (chain.kind === "starknet") {
    const order = starkEc.starkCurve.CURVE.n;
    const scalar = bytesToScalar(await hkdf(laneSecret, info), order);
    const privateKey = `0x${scalar.toString(16).padStart(64, "0")}`;
    const publicKey = starkEc.starkCurve.getStarkKey(privateKey);
    const address = feltHex(
      hash.calculateContractAddressFromHash(
        publicKey,
        chain.stealth.classHash,
        CallData.compile({ client_pubkey: publicKey, cosigner_pubkey: chain.stealth.cosignerPubKey }),
        0
      )
    );
    return { chain: chain.key, kind: "starknet", address, publicKey, ...(withPrivate ? { privateKey } : {}) };
  }

  if (chain.kind === "evm") {
    const scalar = bytesToScalar(await hkdf(laneSecret, info), SECP256K1_ORDER);
    const privateKey = `0x${scalar.toString(16).padStart(64, "0")}`;
    const clientAddress = new ethers.Wallet(privateKey).address;
    const salt = `0x${bytesToHex(await hkdf(laneSecret, `beanie-create2-salt-v1:${chain.key}:${laneId}:${index}`, 32))}`;

    const ctorArgs = ethers.AbiCoder.defaultAbiCoder().encode(
      ["address", "address", "address"],
      [chain.stealth.entryPoint, clientAddress, chain.stealth.cosigner]
    );
    const initCodeHash = ethers.keccak256(ethers.concat([chain.stealth.initCode, ctorArgs]));
    const address = ethers.getCreate2Address(chain.stealth.factory, salt, initCodeHash).toLowerCase();
    return {
      chain: chain.key,
      kind: "evm",
      address,
      publicKey: clientAddress.toLowerCase(),
      ...(withPrivate ? { privateKey } : {}),
    };
  }

  throw new Error(`Private lanes are not supported on ${chain.name}.`);
}

/** Public result only: { chain, kind, address, publicKey }. Safe to cache. */
export const deriveStealthAddress = ({ laneSecret, laneId, index = 0, chain }) =>
  derive(laneSecret, laneId, index, chain, false);

/** As above plus `privateKey` (hex). Only for claiming; wipe/drop it after use. */
export const deriveStealthSigner = ({ laneSecret, laneId, index = 0, chain }) =>
  derive(laneSecret, laneId, index, chain, true);

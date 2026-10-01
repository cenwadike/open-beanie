// identity.js
//
// Address canonicalization, per-chain merchant identity and the CCTP route.
// Dependency-free on purpose (own keccak256), so the checkout page loads no
// third-party code for address math.
//
// Mirrors the backend (create_workers.rs `run_announce_worker`):
//   EVM leg      : merchant = parse::<Address>(s)  else  keccak256(s.as_bytes())[12..32]
//   Starknet leg : merchant = Felt::from_hex(s)    else  derive_felt_from_foreign_address(s)
//   route        : same chain -> all zeros; else chain NAME ("BASE", "STARKNET", ...)
//                  left-aligned in 32 bytes / short-string felt, recipient as 32 BE bytes
//                  (Starknet: u256 split into low/high 128-bit halves, order [chain, low, high]).
// `s` is the backend's canonical string for the announced `address`.

import { BACKEND_FELT_FORMAT, SOLANA_CCTP_RECIPIENT, chainByKey, wire } from "./chains.js";
import { base58Decode, deriveForeignPubkey, usdcAta } from "./solana.js";

export const STARK_PRIME = (1n << 251n) + 17n * (1n << 192n) + 1n;
const EVM_LIMIT = 1n << 160n;
const MASK128 = (1n << 128n) - 1n;
const ZERO32 = `0x${"0".repeat(64)}`;

// ---- keccak256 (original Keccak padding, as Ethereum uses; NOT SHA3-256) ----

const M64 = (1n << 64n) - 1n;
const rotl = (x, n) => (((x << BigInt(n)) | (x >> BigInt(64 - n))) & M64);
const RC = [
    0x0000000000000001n, 0x0000000000008082n, 0x800000000000808an, 0x8000000080008000n,
    0x000000000000808bn, 0x0000000080000001n, 0x8000000080008081n, 0x8000000000008009n,
    0x000000000000008an, 0x0000000000000088n, 0x0000000080008009n, 0x000000008000000an,
    0x000000008000808bn, 0x800000000000008bn, 0x8000000000008089n, 0x8000000000008003n,
    0x8000000000008002n, 0x8000000000000080n, 0x000000000000800an, 0x800000008000000an,
    0x8000000080008081n, 0x8000000000008080n, 0x0000000080000001n, 0x8000000080008008n,
];
const ROTC = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44];
const PILN = [10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1];

function keccakF(st) {
    const bc = new Array(5);
    for (let round = 0; round < 24; round++) {
        for (let i = 0; i < 5; i++) bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        for (let i = 0; i < 5; i++) {
            const t = bc[(i + 4) % 5] ^ rotl(bc[(i + 1) % 5], 1);
            for (let j = 0; j < 25; j += 5) st[j + i] ^= t;
        }
        let t = st[1];
        for (let i = 0; i < 24; i++) {
            const j = PILN[i];
            const b0 = st[j];
            st[j] = rotl(t, ROTC[i]);
            t = b0;
        }
        for (let j = 0; j < 25; j += 5) {
            for (let i = 0; i < 5; i++) bc[i] = st[j + i];
            for (let i = 0; i < 5; i++) st[j + i] ^= (bc[(i + 1) % 5] ^ M64) & bc[(i + 2) % 5];
        }
        st[0] ^= RC[round];
    }
}

export function keccak256(input) {
    const rate = 136;
    const padded = new Uint8Array(Math.ceil((input.length + 1) / rate) * rate);
    padded.set(input);
    padded[input.length] ^= 0x01;
    padded[padded.length - 1] ^= 0x80;
    const st = new Array(25).fill(0n);
    for (let off = 0; off < padded.length; off += rate) {
        for (let i = 0; i < rate / 8; i++) {
            let lane = 0n;
            for (let b = 7; b >= 0; b--) lane = (lane << 8n) | BigInt(padded[off + i * 8 + b]);
            st[i] ^= lane;
        }
        keccakF(st);
    }
    const out = new Uint8Array(32);
    for (let i = 0; i < 4; i++) for (let b = 0; b < 8; b++) out[i * 8 + b] = Number((st[i] >> BigInt(8 * b)) & 0xffn);
    return out;
}

const toHex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
const keccakTail20 = (str) => `0x${toHex(keccak256(new TextEncoder().encode(str))).slice(24)}`; // bytes 12..32

// ---- canonical forms (null when invalid) ------------------------------------

export function canonicalEvm(value) {
    const t = String(value ?? "").trim();
    return /^0x[0-9a-fA-F]{40}$/.test(t) ? t.toLowerCase() : null;
}

export function canonicalFelt(value) {
    const hex = String(value ?? "").trim().replace(/^0x/i, "");
    if (!/^[0-9a-fA-F]{1,64}$/.test(hex)) return null;
    const n = BigInt(`0x${hex}`);
    // >= 2^160 keeps an EVM address from being accepted as a Starknet one.
    if (n < EVM_LIMIT || n >= STARK_PRIME) return null;
    return `0x${hex.toLowerCase().padStart(64, "0")}`;
}

export function canonicalSolana(value) {
    const t = String(value ?? "").trim();
    if (!/^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(t)) return null;
    // 32-44 base58 chars can decode to 33 bytes; a public key is exactly 32.
    return base58Decode(t)?.length === 32 ? t : null;
}

export function canonicalAddress(chainKey, value) {
    const chain = chainByKey(chainKey);
    if (!chain) return null;
    if (chain.kind === "evm") return canonicalEvm(value);
    if (chain.kind === "starknet") return canonicalFelt(value);
    return canonicalSolana(value);
}

export const isValidAddress = (chainKey, value) => canonicalAddress(chainKey, value) !== null;

// ---- merchant identity ------------------------------------------------------

/** The exact STRING the backend hashes for a Starknet felt (see BACKEND_FELT_FORMAT). */
export function backendFeltString(canonicalFeltValue) {
    const n = BigInt(canonicalFeltValue);
    const h = n.toString(16);
    if (BACKEND_FELT_FORMAT === "min") return `0x${h}`;
    if (BACKEND_FELT_FORMAT === "pad62") return `0x${h.padStart(62, "0")}`;
    return `0x${h.padStart(64, "0")}`;
}

/**
 * The merchant identity `chainKey`'s worker ends up with for `identity`, the
 * canonical address the lane was announced with (EVM, Starknet or Solana form).
 * `s` is the string the backend hashes: the address itself (EVM lowercase,
 * Solana base58) or backendFeltString for a felt.
 *   EVM leg      : the address itself if EVM-form, else keccak256(s)[12..32]
 *   Starknet leg : EVM/felt parse natively; a Solana address goes through
 *                  derive_felt_from_foreign_address = keccak256(s)[12..32] as a felt
 *   Solana leg   : a Solana address natively, else derive_pubkey_from_foreign_address = sha256(s)
 * Anything else throws instead of guessing: a wrong identity means a receiver
 * address the backend never registers.
 */
export async function merchantIdentity(chainKey, identity) {
    const chain = chainByKey(chainKey);
    const evm = canonicalEvm(identity);
    const felt = evm ? null : canonicalFelt(identity);
    const sol = evm || felt ? null : canonicalSolana(identity);
    if (!evm && !felt && !sol) throw new Error("Merchant identity must be a valid EVM, Starknet or Solana address.");
    const s = evm ?? (felt ? backendFeltString(felt) : sol);

    if (chain?.kind === "evm") return evm ?? keccakTail20(s);
    if (chain?.kind === "starknet") {
        if (sol) return `0x${BigInt(keccakTail20(s)).toString(16)}`;
        return `0x${BigInt(evm ?? felt).toString(16)}`;
    }
    if (chain?.kind === "solana") return sol ?? (await deriveForeignPubkey(s));
    throw new Error(`Merchant identity for ${chainKey} cannot be derived client-side`);
}

// ---- CCTP route (part of the receiver address) ------------------------------

export async function cctpRoute(sourceChain, targetChain, targetRecipient) {
    if (!targetChain || !targetRecipient) throw new Error("Settlement chain and recipient are required");
    const source = wire(sourceChain);
    const target = wire(targetChain);

    if (source === target) {
        return { chain32: ZERO32, recipient32: ZERO32, chainFelt: "0x0", low: "0x0", high: "0x0" };
    }

    const targetCfg = chainByKey(target);
    if (!targetCfg) throw new Error(`Unknown settlement chain ${target}`);
    const recipient = canonicalAddress(target, targetRecipient);
    if (!recipient) throw new Error(`Invalid ${target} settlement recipient`);

    const nameHex = Array.from(new TextEncoder().encode(target), (b) => b.toString(16).padStart(2, "0")).join("");

    let recipient32;
    if (targetCfg.kind === "solana") {
        // CCTP mints to a TOKEN ACCOUNT on Solana (see SOLANA_CCTP_RECIPIENT).
        const key = SOLANA_CCTP_RECIPIENT === "ata" ? await usdcAta(recipient, targetCfg.usdc) : recipient;
        recipient32 = `0x${toHex(base58Decode(key))}`;
    } else {
        recipient32 = `0x${recipient.replace(/^0x/, "").padStart(64, "0")}`;
    }
    const r = BigInt(recipient32);

    return {
        chain32: `0x${nameHex.padEnd(64, "0")}`,
        recipient32,
        chainFelt: `0x${nameHex}`,
        low: `0x${(r & MASK128).toString(16)}`,
        high: `0x${(r >> 128n).toString(16)}`,
    };
}
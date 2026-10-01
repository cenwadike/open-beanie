// solana.js
//
// Dependency-free Solana primitives: base58, program-derived addresses,
// associated token accounts, and the legacy `Message` for ONE SPL
// TransferChecked, which is what the backend's /pay `solana` payload carries.
// Like identity.js, no third-party code runs for address math.

export const TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
export const ATA_PROGRAM = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

const ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const INDEX = new Map([...ALPHABET].map((c, i) => [c, i]));

// ---- base58 -----------------------------------------------------------------

/** Uint8Array, or null on any character outside the alphabet. */
export function base58Decode(value) {
    const s = String(value ?? "");
    if (!s) return null;
    let n = 0n;
    for (const ch of s) {
        const v = INDEX.get(ch);
        if (v === undefined) return null;
        n = n * 58n + BigInt(v);
    }
    const bytes = [];
    while (n > 0n) {
        bytes.push(Number(n & 0xffn));
        n >>= 8n;
    }
    let zeros = 0;
    while (zeros < s.length && s[zeros] === "1") zeros++;
    return Uint8Array.from([...new Array(zeros).fill(0), ...bytes.reverse()]);
}

export function base58Encode(bytes) {
    let n = 0n;
    for (const b of bytes) n = (n << 8n) | BigInt(b);
    let out = "";
    while (n > 0n) {
        out = ALPHABET[Number(n % 58n)] + out;
        n /= 58n;
    }
    let zeros = 0;
    while (zeros < bytes.length && bytes[zeros] === 0) zeros++;
    return "1".repeat(zeros) + out;
}

/** 32 raw bytes of a public key, or throws. */
export function pubkeyBytes(address, label = "address") {
    const b = base58Decode(address);
    if (!b || b.length !== 32) throw new Error(`Invalid Solana ${label}.`);
    return b;
}

// ---- bytes ------------------------------------------------------------------

export function concatBytes(...parts) {
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let off = 0;
    for (const p of parts) {
        out.set(p, off);
        off += p.length;
    }
    return out;
}

export const bytesEqual = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

export function toBase64(bytes) {
    let s = "";
    for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s);
}

const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));

// ---- PDA / ATA --------------------------------------------------------------

const P = (1n << 255n) - 19n;
const mod = (a) => ((a % P) + P) % P;

function modPow(base, exp) {
    let result = 1n;
    let b = mod(base);
    let e = exp;
    while (e > 0n) {
        if (e & 1n) result = (result * b) % P;
        b = (b * b) % P;
        e >>= 1n;
    }
    return result;
}

const D = mod(-121665n * modPow(121666n, P - 2n)); // edwards25519 d

/** True when the 32 bytes decompress to a point on ed25519 (so they can NOT be a PDA). */
export function isOnCurve(bytes) {
    let y = 0n;
    for (let i = 31; i >= 0; i--) y = (y << 8n) | BigInt(bytes[i]);
    y = mod(y & ((1n << 255n) - 1n)); // top bit is the x sign; dalek reduces non-canonical y
    const y2 = (y * y) % P;
    const u = mod(y2 - 1n);
    const v = mod(D * y2 + 1n);
    const x2 = (u * modPow(v, P - 2n)) % P; // x^2 = u / v
    if (x2 === 0n) return true;
    return modPow(x2, (P - 1n) / 2n) === 1n; // Euler criterion
}

const enc = new TextEncoder();

export async function findProgramAddress(seeds, programId) {
    const program = typeof programId === "string" ? pubkeyBytes(programId, "program id") : programId;
    for (let bump = 255; bump >= 0; bump--) {
        const hash = await sha256(concatBytes(...seeds, Uint8Array.of(bump), program, enc.encode("ProgramDerivedAddress")));
        if (!isOnCurve(hash)) return { address: hash, bump };
    }
    throw new Error("No viable program address.");
}

/** Associated token account (base58) of `owner` for `mint`. */
export async function usdcAta(owner, mint) {
    const { address } = await findProgramAddress(
        [pubkeyBytes(owner, "owner"), pubkeyBytes(TOKEN_PROGRAM), pubkeyBytes(mint, "mint")],
        ATA_PROGRAM
    );
    return base58Encode(address);
}

// ---- legacy message: one SPL TransferChecked --------------------------------

function shortvec(n) {
    const out = [];
    for (; ;) {
        const b = n & 0x7f;
        n >>= 7;
        if (n) out.push(b | 0x80);
        else {
            out.push(b);
            return out;
        }
    }
}

function u64le(value) {
    let v = BigInt(value);
    if (v < 0n || v >= 1n << 64n) throw new Error("Amount out of range.");
    const out = new Uint8Array(8);
    for (let i = 0; i < 8; i++) {
        out[i] = Number(v & 0xffn);
        v >>= 8n;
    }
    return out;
}

/**
 * Message layout (all legacy):
 *   keys: [feePayer(w,s), owner(r,s), source(w), destination(w), mint(r), tokenProgram(r)]
 *   one instruction: TransferChecked(source, mint, destination, owner)
 * The fee payer is the keeper; the owner (the payer's wallet) is the only
 * signature the browser can supply.
 */
export function buildTransferCheckedMessage({ feePayer, owner, source, destination, mint, amount, decimals, blockhash }) {
    const keys = [feePayer, owner, source, destination, mint, TOKEN_PROGRAM].map((k, i) => pubkeyBytes(k, `key ${i}`));
    if (new Set([feePayer, owner, source, destination, mint, TOKEN_PROGRAM]).size !== 6) {
        throw new Error("Solana transfer accounts must all be distinct.");
    }
    const hash = pubkeyBytes(blockhash, "blockhash");
    const data = concatBytes(Uint8Array.of(12), u64le(amount), Uint8Array.of(decimals)); // 12 = TransferChecked
    const ixAccounts = [2, 4, 3, 1]; // source, mint, destination, owner
    return concatBytes(
        Uint8Array.of(2, 1, 2), // 2 signatures, 1 readonly signed, 2 readonly unsigned
        Uint8Array.from(shortvec(keys.length)),
        ...keys,
        hash,
        Uint8Array.from(shortvec(1)),
        Uint8Array.of(5), // program id index: tokenProgram
        Uint8Array.from(shortvec(ixAccounts.length)),
        Uint8Array.from(ixAccounts),
        Uint8Array.from(shortvec(data.length)),
        data
    );
}

/** Wire-format transaction with empty signature slots, for a wallet to sign. */
export const unsignedTransaction = (message, signers = 2) =>
    concatBytes(Uint8Array.from(shortvec(signers)), new Uint8Array(64 * signers), message);

/** { signatures: Uint8Array[], message: Uint8Array } from a wire-format transaction. */
export function parseTransaction(bytes) {
    let n = 0;
    let shift = 0;
    let i = 0;
    for (; ;) {
        const b = bytes[i++];
        if (b === undefined) throw new Error("Malformed transaction.");
        n |= (b & 0x7f) << shift;
        if (!(b & 0x80)) break;
        shift += 7;
    }
    if (i + n * 64 > bytes.length) throw new Error("Malformed transaction.");
    const signatures = [];
    for (let k = 0; k < n; k++) signatures.push(bytes.slice(i + k * 64, i + (k + 1) * 64));
    return { signatures, message: bytes.slice(i + n * 64) };
}

// ---- identity derivation (mirrors derive_pubkey_from_foreign_address) -------

/** sha256(address string) as a pubkey: the backend's Solana merchant for a non-Solana identity. */
export async function deriveForeignPubkey(addressString) {
    return base58Encode(await sha256(enc.encode(String(addressString))));
}

// ---- factory program PDAs + the ReceiverAnnounced event --------------------

export function hexToBytes(hex) {
    const h = String(hex).replace(/^0x/i, "");
    if (!/^([0-9a-fA-F]{2})*$/.test(h)) throw new Error("Invalid hex.");
    return Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
}

/**
 * The two program accounts an announced receiver owns, both derivable ONLY
 * once the receiver is known (the receiver itself is a random keypair):
 *   receiver_config      [CONFIG_SEED,  merchant, receiver, cctp_chain, cctp_recipient]
 *   pending_registration [PENDING_SEED, merchant, receiver, cctp_chain, cctp_recipient]
 */
export async function receiverPdas({ program, merchant, receiver, chain32, recipient32 }) {
    const tail = [pubkeyBytes(merchant, "merchant"), pubkeyBytes(receiver, "receiver"), hexToBytes(chain32), hexToBytes(recipient32)];
    const [config, pending] = await Promise.all([
        findProgramAddress([enc.encode(program.seeds.config), ...tail], program.id),
        findProgramAddress([enc.encode(program.seeds.pending), ...tail], program.id),
    ]);
    return { config: base58Encode(config.address), pending: base58Encode(pending.address) };
}

let announcedDiscriminator = null;

/**
 * Finds the receiver in an Anchor `ReceiverAnnounced` event for a KNOWN
 * (merchant, route). The event is 8 discriminator bytes + 32-byte fields; rather
 * than trust a field order, every slot is tried as the receiver and accepted
 * only if BOTH program accounts derived from it are also in the event, next to
 * the expected merchant and route. Returns the receiver (base58) or null.
 */
export async function receiverFromAnnouncedEvent(data, { program, merchant, chain32, recipient32 }) {
    announcedDiscriminator ??= (await sha256(enc.encode("event:ReceiverAnnounced"))).slice(0, 8);
    if (data.length < 8 + 32 * 6 || (data.length - 8) % 32 !== 0) return null;
    if (!bytesEqual(data.slice(0, 8), announcedDiscriminator)) return null;

    const slots = [];
    for (let o = 8; o < data.length; o += 32) slots.push(base58Encode(data.slice(o, o + 32)));
    const has = (b) => slots.includes(base58Encode(b));
    if (!has(pubkeyBytes(merchant)) || !has(hexToBytes(chain32)) || !has(hexToBytes(recipient32))) return null;

    for (const candidate of new Set(slots)) {
        let pdas;
        try {
            pdas = await receiverPdas({ program, merchant, receiver: candidate, chain32, recipient32 });
        } catch {
            continue;
        }
        if (slots.includes(pdas.config) && slots.includes(pdas.pending)) return candidate;
    }
    return null;
}

export const fromBase64 = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
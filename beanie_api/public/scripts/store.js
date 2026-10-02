// store.js
//
// Local persistence: lanes, deposit history, per-receiver scan cursors, and
// backup export/import. Everything here is PUBLIC data (lane ids, addresses).
// No key material is ever stored.
//
// Lane record (v2):
//   { v, id, privacy, targetChain, targetRecipient, merchantAddress|null,
//     index, prfCredentialId|null, webhookUrl|null, createdAt, needsVerify?,
//     stealthConfig?, receivers: [{ chain, address, merchant, startBlock|null }] }

import { chainByKey, wire } from "./chains.js";
import { canonicalAddress, canonicalEvm } from "./identity.js";

const K = {
    lanes: "beanie.lanes.v2",
    history: "beanie.history.v2",
    cursors: "beanie.cursors.v2",
    seen: "beanie.history.seen.v2",
};
const MAX_HISTORY = 500;
const LANE_ID = /^[A-Za-z0-9_-]{1,128}$/;

function read(key, fallback) {
    try {
        const raw = localStorage.getItem(key);
        return raw ? JSON.parse(raw) : fallback;
    } catch {
        return fallback;
    }
}
function write(key, value) {
    try {
        localStorage.setItem(key, JSON.stringify(value));
        return true;
    } catch {
        return false;
    }
}

// Hex addresses are case-insensitive; Solana base58 is not and must keep its case.
export const receiverKey = (chain, address) =>
    `${wire(chain)}:${chainByKey(chain)?.kind === "solana" ? String(address) : String(address).toLowerCase()}`;

// ---- lanes -----------------------------------------------------------------

function isLane(l) {
    return Boolean(
        l &&
        typeof l.id === "string" &&
        chainByKey(l.targetChain) &&
        Array.isArray(l.receivers) &&
        l.receivers.every((r) => r && chainByKey(r.chain) && typeof r.address === "string")
    );
}

export function getLanes() {
    const v = read(K.lanes, []);
    return Array.isArray(v) ? v.filter(isLane) : [];
}
export const getLane = (id) => getLanes().find((l) => l.id === id) ?? null;

export function saveLane(lane) {
    const lanes = getLanes().filter((l) => l.id !== lane.id);
    lanes.unshift(lane);
    return write(K.lanes, lanes);
}

export function markVerified(id) {
    const lanes = getLanes();
    const lane = lanes.find((l) => l.id === id);
    if (!lane) return false;
    delete lane.needsVerify;
    return write(K.lanes, lanes);
}

// ---- history + cursors -----------------------------------------------------

export function getHistory() {
    const v = read(K.history, []);
    return Array.isArray(v) ? v : [];
}

export function getCursor(chain, address) {
    const n = read(K.cursors, {})[receiverKey(chain, address)];
    return Number.isInteger(n) && n >= 0 ? n : null;
}

/**
 * Persists newly found deposits and advances the cursor. Entries are
 * de-duplicated by (receiver, tx, amount, ordinal-within-tx), so two tabs or a
 * retry after a crash cannot double-count. Returns the entries that were new,
 * or null if storage refused the write (the cursor is then NOT advanced).
 */
export function recordScan(receiver, deposits, nextBlock, laneId = null) {
    const key = receiverKey(receiver.chain, receiver.address);
    const history = getHistory();
    const seen = new Set(history.map((h) => h.id));
    const ordinals = new Map();
    const added = [];

    for (const d of deposits) {
        const base = `${key}:${d.tx}:${d.amount}`;
        const n = ordinals.get(base) ?? 0;
        ordinals.set(base, n + 1);
        const id = `${base}:${n}`;
        if (seen.has(id)) continue;
        added.push({
            id,
            chain: wire(receiver.chain),
            address: receiver.address,
            laneId,
            amount: String(d.amount),
            tx: d.tx,
            block: d.block,
            time: Date.now(), // detection time; logs carry no timestamp
        });
    }

    if (added.length) {
        const merged = [...added.slice().reverse(), ...history].slice(0, MAX_HISTORY);
        if (!write(K.history, merged)) return null;
    }
    const cursors = read(K.cursors, {});
    cursors[key] = nextBlock;
    if (!write(K.cursors, cursors)) return null;
    return added;
}

export function setCursor(chain, address, block) {
    const cursors = read(K.cursors, {});
    cursors[receiverKey(chain, address)] = block;
    write(K.cursors, cursors);
}

// ---- unseen-deposit badge --------------------------------------------------

const seenAt = () => Number(localStorage.getItem(K.seen) || 0);
export const markSeen = () => {
    try {
        localStorage.setItem(K.seen, String(Date.now()));
    } catch {
        /* ignore */
    }
};
export const unseenCount = () => {
    const t = seenAt();
    return getHistory().filter((h) => h.time > t).length;
};

// ---- backup ----------------------------------------------------------------

export function exportBackup() {
    return JSON.stringify({ app: "beanie", version: 2, exportedAt: Date.now(), lanes: getLanes() }, null, 2);
}

const optString = (v) => (typeof v === "string" && v.length <= 2048 ? v : null);
const optInt = (v) => (Number.isInteger(v) && v >= 0 ? v : null);
const feltString = (v) => typeof v === "string" && /^0x[0-9a-f]{1,64}$/i.test(v) ? v.toLowerCase() : null;
const addressString = (chain, v) => canonicalAddress(chain, v);

function sanitizeStealthConfig(raw, chain) {
    if (!raw || typeof raw !== "object") return null;
    const kind = chainByKey(chain)?.kind;
    if (kind === "evm") {
        const factory = addressString(chain, raw.factory);
        const cosigner = addressString(chain, raw.cosigner);
        const chainId = optInt(Number(raw.chainId));
        const usdc = addressString(chain, raw.usdc);
        const name = optString(raw.eip712?.name);
        const version = optString(raw.eip712?.version);
        if (!factory || !cosigner || !chainId || !usdc || !name || !version) return null;
        return { factory, cosigner, chainId, usdc, eip712: { name, version }, maxAuthWindowSecs: optInt(raw.maxAuthWindowSecs) ?? 86400 };
    }
    if (kind === "starknet") {
        const classHash = feltString(raw.classHash);
        const cosigner = canonicalEvm(raw.cosigner);
        const chainId = feltString(raw.chainId);
        const usdc = addressString(chain, raw.usdc);
        const maxFeeFri = optString(raw.maxFeeFri);
        const resourceBounds = raw.resourceBounds;
        const validBounds = resourceBounds && ["l1_gas", "l2_gas", "l1_data_gas"].every((key) =>
            [resourceBounds[key]?.max_amount, resourceBounds[key]?.max_price_per_unit]
                .every((value) => typeof value === "string" && /^(0x[0-9a-f]+|[0-9]+)$/i.test(value))
        );
        if (!classHash || !cosigner || !chainId || !usdc || !maxFeeFri || !validBounds) return null;
        return {
            classHash,
            cosigner,
            chainId,
            usdc,
            maxFeeFri,
            resourceBounds: Object.fromEntries(["l1_gas", "l2_gas", "l1_data_gas"].map((key) => [key, {
                max_amount: resourceBounds[key].max_amount,
                max_price_per_unit: resourceBounds[key].max_price_per_unit,
            }])),
        };
    }
    if (kind === "solana") {
        const cosigner = addressString(chain, raw.cosigner);
        const relayer = addressString(chain, raw.relayer);
        const usdc = addressString(chain, raw.usdc);
        return cosigner && relayer && usdc ? { cosigner, relayer, usdc } : null;
    }
    return null;
}

function sanitizeLane(raw) {
    if (!raw || typeof raw.id !== "string" || !LANE_ID.test(raw.id)) return null;
    const target = wire(raw.targetChain);
    if (!chainByKey(target)) return null;
    const targetRecipient = canonicalAddress(target, raw.targetRecipient);
    if (!targetRecipient) return null;
    if (!Array.isArray(raw.receivers) || raw.receivers.length === 0 || raw.receivers.length > 8) return null;

    const receivers = [];
    for (const r of raw.receivers) {
        const chain = wire(r?.chain);
        const address = canonicalAddress(chain, r?.address);
        if (!chainByKey(chain) || !address) return null;
        receivers.push({ chain, address, merchant: optString(r.merchant), startBlock: optInt(r.startBlock) });
    }

    return {
        v: 2,
        id: raw.id,
        privacy: Boolean(raw.privacy),
        targetChain: target,
        targetRecipient,
        merchantAddress: optString(raw.merchantAddress),
        index: optInt(raw.index) ?? 0,
        prfCredentialId: optString(raw.prfCredentialId),
        webhookUrl: optString(raw.webhookUrl),
        createdAt: Number.isFinite(raw.createdAt) ? raw.createdAt : Date.now(),
        stealthConfig: sanitizeStealthConfig(raw.stealthConfig, target),
        receivers,
        // A backup file is untrusted input: until the addresses are re-derived
        // locally (lane.verifyLane) the lane must not be shared.
        needsVerify: true,
    };
}

/** Merges lanes from a backup file. Existing lanes are never overwritten. */
export function importBackup(text) {
    let data;
    try {
        data = JSON.parse(text);
    } catch {
        throw new Error("That file is not valid JSON.");
    }
    if (data?.app !== "beanie" || !Array.isArray(data.lanes)) {
        throw new Error("That file is not a Beanie backup.");
    }
    const existing = new Set(getLanes().map((l) => l.id));
    let added = 0;
    let skipped = 0;
    for (const raw of data.lanes) {
        const lane = sanitizeLane(raw);
        if (!lane || existing.has(lane.id)) {
            skipped++;
            continue;
        }
        if (saveLane(lane)) {
            existing.add(lane.id);
            added++;
        } else {
            skipped++;
        }
    }
    return { added, skipped };
}
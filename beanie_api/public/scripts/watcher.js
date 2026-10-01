// watcher.js
//
// Polling-only deposit watcher. The backend has no WebSocket or status route,
// so deposits are found by scanning USDC Transfer logs into each receiver
// (onchain.scanDeposits) from a persisted cursor. Log-based, not balance-based,
// because the keeper sweeps quickly and a balance can be zero before and after.
//
// Safe to run in several tabs: navigator.locks lets one tab poll at a time, and
// store.recordScan de-duplicates anyway.

import { getCursor, getLanes, recordScan, setCursor } from "./store.js";
import { headBlock, scanDeposits } from "./onchain.js";

const TICK_MS = 20000;
const MAX_WINDOWS_PER_RECEIVER = 8; // catch-up bound per tick
const MAX_BACKOFF_MS = 5 * 60 * 1000;

const listeners = new Set();
const chainState = new Map(); // chain -> { failures, nextAt }
let timer = null;
let polling = false;

/** cb(deposit, lane) runs for every newly recorded deposit. Returns an unsubscribe fn. */
export function onDeposit(cb) {
    listeners.add(cb);
    return () => listeners.delete(cb);
}

function emit(deposit, lane) {
    for (const cb of listeners) {
        try {
            cb(deposit, lane);
        } catch (e) {
            console.error("[watcher] listener failed", e);
        }
    }
}

async function scanReceiver(lane, receiver) {
    let cursor = getCursor(receiver.chain, receiver.address);
    if (cursor == null) {
        // First time: start where the lane was created, else from now.
        cursor = receiver.startBlock ?? (await headBlock(receiver.chain));
        setCursor(receiver.chain, receiver.address, cursor);
    }

    for (let i = 0; i < MAX_WINDOWS_PER_RECEIVER; i++) {
        const { deposits, nextBlock } = await scanDeposits(receiver.chain, receiver.address, cursor);
        if (nextBlock === cursor) return; // caught up
        const added = recordScan(receiver, deposits, nextBlock, lane.id);
        if (added === null) throw new Error("Could not persist scan progress (storage unavailable).");
        cursor = nextBlock;
        for (const d of added) emit(d, lane);
    }
}

async function scanChain(chain, jobs) {
    const st = chainState.get(chain) ?? { failures: 0, nextAt: 0 };
    if (Date.now() < st.nextAt) return;
    try {
        for (const { lane, receiver } of jobs) await scanReceiver(lane, receiver);
        chainState.set(chain, { failures: 0, nextAt: 0 });
    } catch (e) {
        const failures = st.failures + 1;
        const delay = Math.min(TICK_MS * 2 ** failures, MAX_BACKOFF_MS);
        chainState.set(chain, { failures, nextAt: Date.now() + delay });
        console.warn(`[watcher] ${chain} scan failed (retry in ${Math.round(delay / 1000)}s):`, e?.message || e);
    }
}

async function runPoll() {
    if (polling) return;
    polling = true;
    try {
        const byChain = new Map();
        for (const lane of getLanes()) {
            for (const receiver of lane.receivers) {
                if (!byChain.has(receiver.chain)) byChain.set(receiver.chain, []);
                byChain.get(receiver.chain).push({ lane, receiver });
            }
        }
        await Promise.all([...byChain].map(([chain, jobs]) => scanChain(chain, jobs)));
    } finally {
        polling = false;
    }
}

/** One poll, single-instance across tabs when the Web Locks API exists. */
export async function pollNow() {
    if (navigator.locks?.request) {
        await navigator.locks.request("beanie-watcher", { ifAvailable: true }, async (lock) => {
            if (lock) await runPoll();
        });
    } else {
        await runPoll();
    }
}

export function start() {
    if (timer) return;
    const tick = () => {
        if (!document.hidden) pollNow().catch((e) => console.warn("[watcher]", e));
    };
    document.addEventListener("visibilitychange", tick);
    timer = setInterval(tick, TICK_MS);
    tick();
}

export function stop() {
    if (timer) clearInterval(timer);
    timer = null;
}
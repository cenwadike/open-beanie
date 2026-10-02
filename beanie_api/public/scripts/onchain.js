// onchain.js
//
// Read-only chain access: receiver prediction, balances, existence checks and
// deposit discovery. Reads go through the internal endpoint (/api/v1/rpc/:chain)
// so no provider key ever reaches the browser; if the proxy is unreachable they
// fall back to the public RPC in chains.js.
//
// Receiver prediction decides where money goes, so it is confirmed by TWO
// independent sources (proxy + direct public RPC) and refused if they differ.
// Deposits are found from Transfer logs, not balance deltas, because the keeper
// sweeps quickly and a balance can be zero both before and after a poll.

import {
    RPC_PROXY_ENABLED, RPC_PROXY_PATH, SOLANA_RECEIVER_KIND, STARKNET_TRANSFER_SELECTOR, STRICT_PREDICTION, chainByKey,
} from "./chains.js";
import { TOKEN_PROGRAM, fromBase64, receiverFromAnnouncedEvent, receiverPdas, usdcAta } from "./solana.js";
import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";

const EVM_PREDICT_SELECTOR = "0xf05b69dd"; // predictReceiverAddress(address,bytes32,bytes32)
const STARKNET_PREDICT_SELECTOR = "0x28d4d0fe094b456bae50b2d871903c993ba153ec519b7f4f1c71252fa4304cf";
const STARKNET_BALANCEOF_SELECTOR = "0x2e4263afad30923c891518314c3c95dbe830a16874e8abc5777a9a20b54c76";
const STEALTH_FACTORY = new ethers.Interface([
    "function getAddress(address client,address cosigner,bytes32 salt) view returns (address)",
]);
const EVM_TRANSFER_TOPIC = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

const EVM_LOG_WINDOW = 2000; // proxy allows up to 5000
const STARKNET_EVENT_WINDOW = 500; // proxy allows up to 2000
const SOLANA_MAX_SIG_PAGES = 10; // 1000 signatures each
const SOLANA_TX_CONCURRENCY = 5;
const TIMEOUT_MS = 15000;

const pad32 = (hex) => hex.replace(/^0x/i, "").toLowerCase().padStart(64, "0");
const toHexNum = (n) => `0x${BigInt(n).toString(16)}`;

function requireChain(chainKey) {
    const chain = chainByKey(chainKey);
    if (!chain) throw new Error(`Unknown chain ${chainKey}`);
    return chain;
}

// ---- transport --------------------------------------------------------------

/** The node answered with a JSON-RPC error (as opposed to a transport/proxy failure). */
export class RpcError extends Error {
    constructor(message, code) {
        super(message);
        this.name = "RpcError";
        this.code = code;
    }
}

async function postJson(url, body) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
    try {
        return await fetch(url, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(body),
            signal: controller.signal,
        });
    } finally {
        clearTimeout(timer);
    }
}

async function unwrap(res, method) {
    let json;
    try {
        json = await res.json();
    } catch {
        throw new Error(`${method}: invalid response (HTTP ${res.status})`);
    }
    if (json?.error && typeof json.error === "object") {
        throw new RpcError(json.error.message || `${method} failed`, json.error.code);
    }
    if (!res.ok) throw new Error(`${method}: HTTP ${res.status}${json?.error ? ` ${json.error}` : ""}`);
    if (!json || !("result" in json)) throw new Error(`${method}: malformed response`);
    return json.result;
}

/** Direct JSON-RPC to a node URL (used for the public fallback and cross-checks). */
export async function rpcCall(url, method, params) {
    const res = await postJson(url, { jsonrpc: "2.0", id: Date.now(), method, params });
    return unwrap(res, method);
}

async function proxyCall(chain, method, params) {
    const res = await postJson(`${RPC_PROXY_PATH}/${chain.key.toLowerCase()}`, {
        jsonrpc: "2.0",
        id: Date.now(),
        method,
        params,
    });
    return unwrap(res, method);
}

/**
 * Chain read. Default: internal proxy, falling back to the public RPC on a
 * transport failure (a real JSON-RPC error is never masked).
 *   direct    : skip the proxy
 *   proxyOnly : never fall back (used by the cross-check)
 */
export async function rpc(chainKey, method, params, { direct = false, proxyOnly = false } = {}) {
    const chain = requireChain(chainKey);
    if (direct || !RPC_PROXY_ENABLED) return rpcCall(chain.rpc, method, params);
    try {
        return await proxyCall(chain, method, params);
    } catch (e) {
        if (e instanceof RpcError || proxyOnly) throw e;
        return rpcCall(chain.rpc, method, params);
    }
}

// ---- receiver prediction ----------------------------------------------------
async function predictVia(chain, merchant, route, opts) {
    if (chain.kind === "evm") {
        const data = EVM_PREDICT_SELECTOR + pad32(merchant) + route.chain32.slice(2) + route.recipient32.slice(2);
        const out = await rpc(chain.key, "eth_call", [{ to: chain.factory, data }, "latest"], opts);
        if (!/^0x[0-9a-fA-F]{64}$/.test(String(out))) throw new Error("unexpected predict result");
        return `0x${String(out).slice(-40)}`;
    }
    if (chain.kind === "starknet") {
        const out = await rpc(
            chain.key,
            "starknet_call",
            [
                {
                    contract_address: chain.factory,
                    entry_point_selector: STARKNET_PREDICT_SELECTOR,
                    calldata: [merchant, route.chainFelt, route.low, route.high],
                },
                "latest",
            ],
            opts
        );
        if (!out?.[0]) throw new Error("empty predict result");
        return out[0];
    }
    throw new Error(`${chain.name} receivers cannot be predicted client-side`);
}

const sameAddress = (kind, a, b) => (kind === "evm" ? a.toLowerCase() === b.toLowerCase() : BigInt(a) === BigInt(b));

export async function predictReceiver(chainKey, merchant, route) {
    const chain = requireChain(chainKey);
    if (!RPC_PROXY_ENABLED) return predictVia(chain, merchant, route, { direct: true });

    const [viaProxy, viaDirect] = await Promise.allSettled([
        predictVia(chain, merchant, route, { proxyOnly: true }),
        predictVia(chain, merchant, route, { direct: true }),
    ]);

    if (viaProxy.status === "fulfilled" && viaDirect.status === "fulfilled") {
        if (!sameAddress(chain.kind, viaProxy.value, viaDirect.value)) {
            throw new Error(`The ${chain.name} receiver differs between RPC sources; refusing to continue.`);
        }
        return viaDirect.value;
    }
    if (STRICT_PREDICTION) {
        const why = (viaProxy.status === "rejected" ? viaProxy.reason : viaDirect.reason)?.message;
        throw new Error(`Could not confirm the ${chain.name} receiver with two independent RPC sources (${why}). Try again.`);
    }
    if (viaDirect.status === "fulfilled") return viaDirect.value;
    if (viaProxy.status === "fulfilled") return viaProxy.value;
    throw viaDirect.reason;
}

async function stealthAddressVia(chain, factory, client, cosigner, salt, opts) {
    const data = STEALTH_FACTORY.encodeFunctionData("getAddress", [client, cosigner, salt]);
    const result = await rpc(chain.key, "eth_call", [{ to: factory, data }, "latest"], opts);
    const [address] = STEALTH_FACTORY.decodeFunctionResult("getAddress", result);
    return address.toLowerCase();
}

/** Two-source-confirmed CREATE2 address from the current StealthAccountFactory. */
export async function predictStealthAccount(chainKey, client, cosigner, salt, factory) {
    const chain = requireChain(chainKey);
    if (chain.kind !== "evm") throw new Error(`${chain.name} does not use an EVM stealth factory.`);
    const targetFactory = factory || chain.stealth?.factory;
    if (!targetFactory) throw new Error(`The ${chain.name} StealthAccountFactory address is not configured.`);
    if (!/^0x[0-9a-f]{64}$/i.test(String(salt))) throw new Error("Invalid private-lane salt.");
    if (!RPC_PROXY_ENABLED) {
        return stealthAddressVia(chain, targetFactory, client, cosigner, salt, { direct: true });
    }

    const [viaProxy, viaDirect] = await Promise.allSettled([
        stealthAddressVia(chain, targetFactory, client, cosigner, salt, { proxyOnly: true }),
        stealthAddressVia(chain, targetFactory, client, cosigner, salt, { direct: true }),
    ]);
    if (viaProxy.status === "fulfilled" && viaDirect.status === "fulfilled") {
        if (!sameAddress(chain.kind, viaProxy.value, viaDirect.value)) {
            throw new Error(`The ${chain.name} stealth account differs between RPC sources; refusing to continue.`);
        }
        return viaDirect.value;
    }
    if (STRICT_PREDICTION) {
        const why = (viaProxy.status === "rejected" ? viaProxy.reason : viaDirect.reason)?.message;
        throw new Error(`Could not confirm the ${chain.name} stealth account with two independent RPC sources (${why}). Try again.`);
    }
    if (viaDirect.status === "fulfilled") return viaDirect.value;
    if (viaProxy.status === "fulfilled") return viaProxy.value;
    throw viaDirect.reason;
}

/** Account nonce for a Starknet contract; an explicitly undeployed address has nonce zero. */
export async function starknetAccountNonce(chainKey, address) {
    const chain = requireChain(chainKey);
    if (chain.kind !== "starknet") throw new Error(`${chain.name} does not have a Starknet account nonce.`);
    try {
        const nonce = await rpc(chain.key, "starknet_getNonce", ["latest", address], { direct: true });
        return BigInt(nonce);
    } catch (e) {
        if (e instanceof RpcError && (e.code === 20 || /contract.?not.?found/i.test(e.message))) return 0n;
        throw e;
    }
}

// ---- balances / existence ---------------------------------------------------

export async function tokenBalance(chainKey, address) {
    const chain = requireChain(chainKey);
    if (chain.kind === "evm") {
        const data = `0x70a08231${pad32(address)}`;
        return BigInt((await rpc(chain.key, "eth_call", [{ to: chain.usdc, data }, "latest"])) || "0x0");
    }
    if (chain.kind === "starknet") {
        const out = await rpc(chain.key, "starknet_call", [
            { contract_address: chain.usdc, entry_point_selector: STARKNET_BALANCEOF_SELECTOR, calldata: [address] },
            "latest",
        ]);
        return (BigInt(out?.[1] || "0") << 128n) + BigInt(out?.[0] || "0");
    }
    if (chain.kind === "solana") {
        // `address` is an owner wallet; its USDC lives in the associated token account.
        const ata = await usdcAta(address, chain.usdc);
        try {
            const res = await rpc(chain.key, "getTokenAccountBalance", [ata, { commitment: "confirmed" }]);
            return BigInt(res?.value?.amount || "0");
        } catch (e) {
            if (e instanceof RpcError && /could not find account/i.test(e.message)) return 0n; // no ATA yet
            throw e;
        }
    }
    throw new Error(`Balance lookup for ${chain.name} is not supported`);
}

export async function contractExists(chainKey, address) {
    const chain = requireChain(chainKey);
    try {
        if (chain.kind === "evm") {
            const code = await rpc(chain.key, "eth_getCode", [address, "latest"]);
            return typeof code === "string" && code !== "0x";
        }
        if (chain.kind === "starknet") {
            await rpc(chain.key, "starknet_getClassHashAt", ["latest", address]);
            return true;
        }
        if (chain.kind === "solana") {
            const res = await rpc(chain.key, "getAccountInfo", [address, { encoding: "base64", commitment: "confirmed" }]);
            return res?.value != null;
        }
    } catch {
        /* treated as "not deployed" */
    }
    return false;
}

export async function headBlock(chainKey) {
    const chain = requireChain(chainKey);
    if (chain.kind === "evm") return parseInt(await rpc(chain.key, "eth_blockNumber", []), 16);
    if (chain.kind === "starknet") return Number(await rpc(chain.key, "starknet_blockNumber", []));
    // Solana "block" = FINALIZED slot, so a cursor never sits ahead of what scans can see.
    if (chain.kind === "solana") return Number(await rpc(chain.key, "getSlot", [{ commitment: "finalized" }]));
    throw new Error(`Block height for ${chain.name} is not supported`);
}

// ---- deposit discovery ------------------------------------------------------

/**
 * USDC transfers into `receiver` from `fromBlock` (inclusive), at most one
 * window per call. Returns { deposits: [{ amount, tx, block }], nextBlock };
 * persist `nextBlock` as the next call's `fromBlock`.
 */
export async function scanDeposits(chainKey, receiver, fromBlock) {
    const chain = requireChain(chainKey);
    const head = await headBlock(chainKey);
    if (fromBlock > head) return { deposits: [], nextBlock: fromBlock };

    if (chain.kind === "evm") {
        const to = Math.min(head, fromBlock + EVM_LOG_WINDOW - 1);
        const logs = await rpc(chain.key, "eth_getLogs", [
            {
                address: chain.usdc,
                fromBlock: toHexNum(fromBlock),
                toBlock: toHexNum(to),
                topics: [EVM_TRANSFER_TOPIC, null, `0x${pad32(receiver)}`],
            },
        ]);
        return {
            deposits: logs.map((l) => ({
                amount: BigInt(l.data).toString(),
                tx: l.transactionHash,
                block: parseInt(l.blockNumber, 16),
            })),
            nextBlock: to + 1,
        };
    }

    if (chain.kind === "starknet") {
        const to = Math.min(head, fromBlock + STARKNET_EVENT_WINDOW - 1);
        const deposits = [];
        let token;
        do {
            const page = await rpc(chain.key, "starknet_getEvents", [
                {
                    from_block: { block_number: fromBlock },
                    to_block: { block_number: to },
                    address: chain.usdc,
                    keys: [[STARKNET_TRANSFER_SELECTOR], [], [receiver]],
                    chunk_size: 100,
                    ...(token ? { continuation_token: token } : {}),
                },
            ]);
            for (const ev of page.events || []) {
                const [low = "0x0", high = "0x0"] = ev.data || [];
                deposits.push({
                    amount: ((BigInt(high) << 128n) + BigInt(low)).toString(),
                    tx: ev.transaction_hash,
                    block: ev.block_number,
                });
            }
            token = page.continuation_token;
        } while (token);
        return { deposits, nextBlock: to + 1 };
    }

    if (chain.kind === "solana") return solanaDeposits(chain, receiver, fromBlock, head);

    throw new Error(`Deposit scanning for ${chain.name} is not supported`);
}

/**
 * Solana deposits = SPL `transfer` / `transferChecked` instructions (the ones
 * the token program logs as "Instruction: Transfer" / "TransferChecked") whose
 * DESTINATION is the receiver's USDC token account. The log lines carry no
 * amount or destination, so the amounts come from the same instructions in
 * parsed form, top-level and inner (CPI) alike.
 *
 * Flow: list finalized signatures touching the token account (newest first,
 * each carries its slot), keep those in [fromBlock, head], parse each. The
 * keeper's sweep has the account as SOURCE and is ignored. `block` is the slot
 * and the cursor is a slot too. Like EVM logs this is per transfer, so two
 * transfers in one transaction are two deposits.
 *
 * It never advances past signatures it could not page through or read:
 * it throws, and the watcher retries with backoff.
 */
async function solanaDeposits(chain, receiver, fromBlock, head) {
    const account = SOLANA_RECEIVER_KIND === "token-account" ? receiver : await usdcAta(receiver, chain.usdc);

    const hits = [];
    let before;
    let reached = false;
    for (let page = 0; page < SOLANA_MAX_SIG_PAGES && !reached; page++) {
        const batch = await rpc(chain.key, "getSignaturesForAddress", [
            account,
            { limit: 1000, commitment: "finalized", ...(before ? { before } : {}) },
        ]);
        for (const s of batch) {
            if (s.slot < fromBlock) {
                reached = true;
                break;
            }
            if (!s.err && s.slot <= head) hits.push(s);
        }
        if (batch.length < 1000) reached = true;
        else before = batch[batch.length - 1].signature;
    }
    if (!reached) {
        throw new Error("Too many Solana transactions since the last scan to page through safely.");
    }
    hits.reverse(); // oldest first

    const deposits = [];
    for (let i = 0; i < hits.length; i += SOLANA_TX_CONCURRENCY) {
        const chunk = hits.slice(i, i + SOLANA_TX_CONCURRENCY);
        const found = await Promise.all(chunk.map((s) => solanaTransfersInto(chain, account, s.signature)));
        chunk.forEach((s, j) => {
            for (const t of found[j]) deposits.push({ amount: t.amount, from: t.authority, tx: s.signature, block: s.slot });
        });
    }
    return { deposits, nextBlock: head + 1 };
}

/** [{ amount, authority }] for every USDC transfer into `account` in one finalized transaction. */
async function solanaTransfersInto(chain, account, signature) {
    const tx = await rpc(chain.key, "getTransaction", [
        signature,
        { encoding: "jsonParsed", commitment: "finalized", maxSupportedTransactionVersion: 0 },
    ]);
    if (!tx?.meta) throw new Error(`Solana transaction ${signature} is not readable yet.`);
    if (tx.meta.err) return [];

    const instructions = [
        ...tx.transaction.message.instructions,
        ...(tx.meta.innerInstructions ?? []).flatMap((g) => g.instructions),
    ];
    const out = [];
    for (const ix of instructions) {
        if (ix.programId !== TOKEN_PROGRAM || !ix.parsed) continue;
        const { type, info } = ix.parsed;
        if (info?.destination !== account || info.source === account) continue;
        if (type === "transfer") {
            out.push({ amount: String(info.amount), authority: info.authority ?? info.multisigAuthority ?? null });
        } else if (type === "transferChecked" && info.mint === chain.usdc) {
            out.push({ amount: String(info.tokenAmount?.amount), authority: info.authority ?? info.multisigAuthority ?? null });
        }
    }
    return out;
}

// ---- Solana receivers: discovery + verification -----------------------------
//
// A Solana receiver is a random server-side keypair, so unlike EVM/Starknet it
// cannot be predicted. What CAN be derived are the program accounts it owns
// ([CONFIG_SEED|PENDING_SEED, merchant, receiver, cctp_chain, cctp_recipient]),
// and that is what is checked here. An existing account proves the receiver was
// announced for exactly this (merchant, route). It does NOT prove who holds the
// receiver's key: announce_merchant has no authority over `merchant`.

function solanaProgram() {
    const chain = requireChain("SOLANA");
    if (!chain.program?.id) throw new Error("Solana factory program is not configured.");
    return chain.program;
}

const hasProgramAccount = (res, programId) => (res?.value ?? []).some((a) => a && a.owner === programId);

/** True when receiver_config or pending_registration for this tuple exists on-chain, owned by the program. */
export async function verifySolanaReceiver({ merchant, receiver, route }) {
    const program = solanaProgram();
    const pdas = await receiverPdas({ program, merchant, receiver, chain32: route.chain32, recipient32: route.recipient32 });
    const res = await rpc("SOLANA", "getMultipleAccounts", [
        [pdas.config, pdas.pending],
        { encoding: "base64", dataSlice: { offset: 0, length: 0 }, commitment: "confirmed" },
    ]);
    return hasProgramAccount(res, program.id);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const DISCOVERY_POLL_MS = 4000;
const DISCOVERY_TX_PER_ROUND = 8;

/**
 * Finds the receiver the backend announced for (merchant, route) by reading
 * ReceiverAnnounced events from the factory program's recent transactions
 * (slot >= fromSlot), then confirms its program accounts exist. Returns the
 * receiver pubkey, or null on timeout. Nothing the server says is trusted.
 */
export async function discoverSolanaReceiver({ merchant, route, fromSlot, timeoutMs = 45000 }) {
    const program = solanaProgram();
    const seen = new Set();
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
        try {
            const sigs = await rpc("SOLANA", "getSignaturesForAddress", [program.id, { limit: 100, commitment: "confirmed" }]);
            const fresh = sigs.filter((s) => !s.err && s.slot >= fromSlot && !seen.has(s.signature)).slice(0, DISCOVERY_TX_PER_ROUND);
            for (const s of fresh) {
                const tx = await rpc("SOLANA", "getTransaction", [
                    s.signature,
                    { encoding: "jsonParsed", commitment: "confirmed", maxSupportedTransactionVersion: 0 },
                ]);
                if (!tx?.meta) continue; // not readable yet: look again next round
                seen.add(s.signature);
                for (const line of tx.meta.logMessages ?? []) {
                    if (!line.startsWith("Program data: ")) continue;
                    let data;
                    try {
                        data = fromBase64(line.slice("Program data: ".length));
                    } catch {
                        continue;
                    }
                    const receiver = await receiverFromAnnouncedEvent(data, {
                        program, merchant, chain32: route.chain32, recipient32: route.recipient32,
                    });
                    if (receiver && (await verifySolanaReceiver({ merchant, receiver, route }))) return receiver;
                }
            }
        } catch (e) {
            console.warn("[solana] receiver discovery:", e?.message || e);
        }
        await sleep(DISCOVERY_POLL_MS);
    }
    return null;
}
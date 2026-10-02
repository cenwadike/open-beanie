// api.js
//
// The only module that talks to /api/v1. Every call goes through `request`,
// which gives one error type, one timeout, and one place that reads the error
// body (the body may use `error` or `message`; both are handled).
//
// The API never returns receiver addresses, and this frontend never trusts
// the server for them: addresses are computed client-side from the on-chain
// factory (see onchain.predictReceiver).

import { apiChain } from "./chains.js";

const TIMEOUT_MS = 20000;

export class ApiError extends Error {
    constructor(status, message, retryAfter = null) {
        super(message);
        this.name = "ApiError";
        this.status = status;
        this.retryAfter = retryAfter;
    }
}

async function request(path, body, method = "POST") {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);

    let res;
    try {
        res = await fetch(path, {
            method,
            ...(body === undefined ? {} : {
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(body),
            }),
            signal: controller.signal,
        });
    } catch (e) {
        if (e?.name === "AbortError") throw new ApiError(0, "The request timed out. Please try again.");
        throw new ApiError(0, "Could not reach the server. Check your connection.");
    } finally {
        clearTimeout(timer);
    }

    const text = await res.text();
    let data = null;
    try {
        data = text ? JSON.parse(text) : null;
    } catch {
        /* non-JSON body */
    }

    if (!res.ok) {
        const detail = (data && (data.error ?? data.message)) || text || `HTTP ${res.status}`;
        const message = res.status === 429 ? `Rate limit reached: ${detail}` : String(detail).slice(0, 300);
        throw new ApiError(res.status, message, res.headers.get("retry-after"));
    }
    if (text && data === null) throw new ApiError(res.status, `Expected JSON from ${path}`);
    return data ?? {};
}

// ---- WebAuthn ceremony -----------------------------------------------------
export const webauthn = {
    registerStart: () => request("/api/v1/webauthn/register/start"),
    registerFinish: (payload) => request("/api/v1/webauthn/register/finish", payload),
    authStart: (payload) => request("/api/v1/webauthn/auth/start", payload),
    authFinish: (payload) => request("/api/v1/webauthn/auth/finish", payload),
};

// ---- Business routes -------------------------------------------------------

export function getStealthCosigners() {
    return request("/api/v1/stealth/cosigners", undefined, "GET");
}

/**
 * One call announces receivers on all six chains (the backend fans out).
 * `chain` + `address` is the single proven identity; `targetRecipient` is the
 * payout address on `targetChain`. Returns { status, message } only.
 */
export function createLane({ chain, address, laneId, verifiedToken, targetChain, targetRecipient, webhookUrl }) {
    return request("/api/v1/create", {
        chain: apiChain(chain),
        address,
        lane_id: laneId,
        verified_token: verifiedToken,
        target_chain: apiChain(targetChain),
        target_recipient: targetRecipient,
        ...(webhookUrl ? { webhook_url: webhookUrl } : {}),
    });
}

/** `signature` is the per-chain payload object; it is JSON-encoded here. */
export function submitPayment({
    chain,
    merchantAddress,
    receiverAddress,
    destinationChain,
    txRef,
    fromAddress,
    amountRaw,
    webhookUrl,
    signature,
}) {
    return request("/api/v1/pay", {
        chain: apiChain(chain),
        merchant_address: merchantAddress,
        receiver_address: receiverAddress,
        destination_chain: apiChain(destinationChain),
        tx_hash: txRef,
        from_address: fromAddress,
        amount_raw: amountRaw,
        webhook_url: webhookUrl ?? null,
        signature: JSON.stringify(signature),
    });
}

/** `txHash` is the chain's signing hash, exactly as the passkey binding used it. */
export function submitStealthClaim({
    chain,
    txHash,
    derivedAddress,
    clientSig,
    verifiedToken,
    calls,
    auth3009,
    starknet,
    messageBytes,
}) {
    return request("/api/v1/stealth/claim", {
        chain: apiChain(chain),
        tx_hash: txHash,
        derived_address: derivedAddress,
        client_sig: clientSig,
        verified_token: verifiedToken,
        calls: calls ?? [],
        ...(auth3009 ? { auth3009 } : {}),
        ...(starknet ? { starknet } : {}),
        ...(messageBytes ? { message_bytes: messageBytes } : {}),
    });
}
// src/webhook.ts
//
// Mirrors webhook.rs, restructured for the resolved design: signing is
// decoupled from on-chain tx identity entirely. The webhook is a
// notification, not the economic event — that's settled on-chain and
// independently verifiable via the tx hash the payload carries. A single
// CRE-secrets-held keypair is sufficient; HMAC is correct here because
// your own backend is both signer and verifier, same reasoning as the
// create-lane token codec elsewhere in this project.
//
// Note: this runs in QuickJS via Javy, not Node and not a browser — there
// is no `crypto.subtle` and no `node:crypto` global. We use @noble/hashes
// (pure JS, no host crypto dependency) for the HMAC instead.

import {
    type Runtime,
    type HTTPSendRequester,
    HTTPClient,
    consensusIdenticalAggregation,
} from "@chainlink/cre-sdk"
import { hmac } from "@noble/hashes/hmac.js"
import { sha256 } from "@noble/hashes/sha2.js"
import type { Config } from "./config"

export function signPayload(runtime: Runtime<Config>, payload: string): string {
    const secret = runtime.getSecret({ id: "WEBHOOK_SIGNING_KEY" }).result()
    const encoder = new TextEncoder()
    const sig = hmac(sha256, encoder.encode(secret.value), encoder.encode(payload))
    return Buffer.from(sig).toString("hex")
}

type WebhookRequest = {
    url: string
    payload: Record<string, unknown>
    signature: string
}

// Runs inside runtime.runInNodeMode() under the hood via the high-level
// httpClient.sendRequest(runtime, fn, aggregation) overload below — this
// is what a plain `Runtime` (not `NodeRuntime`) is actually compatible with.
const postWebhook = (sendRequester: HTTPSendRequester, req: WebhookRequest) => {
    return sendRequester
        .sendRequest({
            url: req.url,
            method: "POST",
            headers: {
                "Content-Type": "application/json",
                "X-Signature": req.signature,
                "Idempotency-Key": String(req.payload.sweepTxHash ?? ""),
            },
            body: new TextEncoder().encode(JSON.stringify(req.payload)),
        })
        .result()
}

export function deliverWebhook(
    httpClient: HTTPClient,
    runtime: Runtime<Config>,
    url: string,
    payload: Record<string, unknown>,
    signature: string,
): void {
    httpClient
        .sendRequest(runtime, postWebhook, consensusIdenticalAggregation())({ url, payload, signature })
        .result()
}
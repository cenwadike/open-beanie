// src/webhook.ts
import {
    type TeeRuntime,
    HTTPClient,
} from "@chainlink/cre-sdk"
import { hmac } from "@noble/hashes/hmac.js"
import { sha256 } from "@noble/hashes/sha2.js"
import type { Config } from "./config"

export function signPayload(runtime: TeeRuntime<Config>, payload: string): string {
    // Secrets are fetched dynamically inside the enclave via TeeRuntime
    const secret = runtime.getSecret({ id: "WEBHOOK_SIGNING_KEY" }).result()
    const encoder = new TextEncoder()
    const sig = hmac(sha256, encoder.encode(secret.value), encoder.encode(payload))
    return Buffer.from(sig).toString("hex")
}

export function deliverWebhook(
    httpClient: HTTPClient,
    runtime: TeeRuntime<Config>,
    url: string,
    payload: Record<string, unknown>,
    signature: string,
): void {
    // Pass TeeRuntime directly to HTTPClient.sendRequest overload
    httpClient.sendRequest(runtime, {
        url,
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            "X-Signature": signature,
            "Idempotency-Key": String(payload.sweepTxHash ?? ""),
        },
        body: new TextEncoder().encode(JSON.stringify(payload)),
    }).result()
}
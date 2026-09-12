// src/batch.ts
//
// Mirrors the call-building half of evm_keeper.rs's multicall_sweep —
// building the Call3[] array. Submitting it lives in index.ts, since that
// step needs the CRE runtime, not just plain data.

import { encodeAbiParameters, type Address } from "viem"
import type { Config } from "./config"
import { SWEEP_SELECTOR, REGISTER_MERCHANT_SELECTOR, REGISTER_MERCHANT_PARAMS } from "./constants"

export type Call3 = { target: Address; allowFailure: boolean; callData: `0x${string}` }

// NOTE: assumes same-chain settlement (zero CCTP params) for every
// registration. If any tenant needs cross-chain settlement, this must read
// the committed params from announceReceiver's event data instead of
// hardcoding zeros — not wired up here, flagged rather than guessed at.
export function buildCallBatch(
    activeAddresses: string[],
    needsRegisterSet: Set<string>,
    merchantMap: Map<string, string>,
    cfg: Config,
): Call3[] {
    const calls: Call3[] = []

    for (const receiver of activeAddresses) {
        const merchant = merchantMap.get(receiver)

        if (needsRegisterSet.has(receiver) && merchant) {
            const encodedArgs = encodeAbiParameters(REGISTER_MERCHANT_PARAMS, [
                merchant as Address,
                `0x${"0".repeat(64)}`,
                `0x${"0".repeat(64)}`,
            ])
            calls.push({
                target: cfg.factoryAddress as Address,
                allowFailure: true,
                callData: `${REGISTER_MERCHANT_SELECTOR}${encodedArgs.slice(2)}` as `0x${string}`,
            })
        }

        calls.push({
            target: receiver as Address,
            allowFailure: true,
            callData: SWEEP_SELECTOR,
        })
    }

    return calls
}

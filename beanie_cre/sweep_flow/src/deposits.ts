// src/deposits.ts
//
// Mirrors evm_keeper.rs's fetch_deposits_since_block, plus the eth_getCode
// branch check from transfer_workers.rs's register-vs-sweep decision.

import type { Config } from "./config"
import { getLogsChunked, rpcBatchCall, type HttpRequester } from "./rpc"
import { TRANSFER_SIG } from "./constants"

/** Set of receiver addresses (lowercase) that received a transfer this window. */
export function scanDeposits(
    requester: HttpRequester,
    cfg: Config,
    receivers: string[],
    tipBlock: number,
): Set<string> {
    if (receivers.length === 0) return new Set()

    const fromBlock = Math.max(0, tipBlock - cfg.depositScanBlocks)
    const active = new Set<string>()

    // Topic OR-lists are typically capped ~1000 values per provider — chunk
    // the receiver set if it ever exceeds that (not expected at current scale).
    for (let i = 0; i < receivers.length; i += 1000) {
        const chunk = receivers.slice(i, i + 1000)
        const paddedAddrs = chunk.map((a) => `0x${"0".repeat(24)}${a.slice(2).toLowerCase()}`)

        const logs = getLogsChunked(
            requester, cfg.rpcUrl, cfg.tokenAddress,
            [TRANSFER_SIG, null, paddedAddrs],
            fromBlock, tipBlock, cfg.logChunkBlocks,
        )
        for (const log of logs) {
            if (log.topics.length < 3) continue
            active.add(`0x${log.topics[2].slice(-40)}`.toLowerCase())
        }
    }
    return active
}


/** Returns the subset of addresses with no deployed contract yet (i.e. need
 * registerMerchant bundled ahead of their sweep). One batched HTTP call for
 * the whole set, instead of one eth_getCode round trip per address. */
export function needsRegistrationBatch(
    requester: HttpRequester,
    cfg: Config,
    addresses: string[],
): Set<string> {
    if (addresses.length === 0) return new Set()

    const codes = rpcBatchCall<string>(
        requester,
        cfg.rpcUrl,
        addresses.map((addr) => ({ method: "eth_getCode", params: [addr, "latest"] })),
    )

    const result = new Set<string>()
    codes.forEach((code, i) => {
        if (code === "0x" || code === "0x0") result.add(addresses[i])
    })
    return result
}

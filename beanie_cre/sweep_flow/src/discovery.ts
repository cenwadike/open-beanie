// src/discovery.ts
//
// Mirrors evm_keeper.rs's discover_merchants and discover_webhook_urls.
// Both are re-scanned from their deploy block on every tick, full history
// — registration volume is low relative to token transfers, so there is
// no watermark here, same "recompute instead of remember" principle used
// everywhere else in this system. Revisit with an incremental cursor only
// once registration volume genuinely makes a full rescan slow.

import type { Config } from "./config"
import { getLogsChunked, type HttpRequester } from "./rpc"
import { MERCHANT_REGISTERED_SIG, RECEIVER_ANNOUNCED_SIG, WEBHOOK_URL_SET_SIG } from "./constants"

/** receiver address (lowercase) -> merchant address */
export function discoverMerchants(
    requester: HttpRequester,
    cfg: Config,
    tipBlock: number,
): Map<string, string> {
    const logs = getLogsChunked(
        requester, cfg.rpcUrl, cfg.factoryAddress,
        [[MERCHANT_REGISTERED_SIG, RECEIVER_ANNOUNCED_SIG]],
        cfg.registryStartBlock, tipBlock, cfg.logChunkBlocks,
    )

    const map = new Map<string, string>()
    for (const log of logs) {
        if (log.topics.length < 2) continue
        const merchant = `0x${log.topics[1].slice(-40)}`

        if (log.topics[0] === RECEIVER_ANNOUNCED_SIG && log.topics.length >= 3) {
            const receiver = `0x${log.topics[2].slice(-40)}`
            map.set(receiver.toLowerCase(), merchant)
        } else if (log.topics[0] === MERCHANT_REGISTERED_SIG && log.data.length >= 66) {
            const receiver = `0x${log.data.slice(-40)}`
            map.set(receiver.toLowerCase(), merchant)
        }
    }
    return map
}

/** merchant address (lowercase) -> webhook URL */
export function discoverWebhookUrls(
    requester: HttpRequester,
    cfg: Config,
    tipBlock: number,
): Map<string, string> {
    const logs = getLogsChunked(
        requester, cfg.rpcUrl, cfg.webhookRegistryAddress,
        [WEBHOOK_URL_SET_SIG],
        cfg.webhookRegistryStartBlock, tipBlock, cfg.logChunkBlocks,
    )

    const map = new Map<string, string>()
    for (const log of logs) {
        if (log.topics.length < 2) continue
        const merchant = `0x${log.topics[1].slice(-40)}`.toLowerCase()

        // data is ABI-encoded string: offset(32 bytes) + length(32 bytes) + bytes.
        // Hand-decoded here rather than via a full ABI decoder — verify against
        // real testnet logs before trusting this on an event with unusual URL
        // encoding (non-ASCII, very long strings).
        const lengthHex = log.data.slice(66, 130)
        const length = parseInt(lengthHex, 16)
        const strBytes = log.data.slice(130, 130 + length * 2)
        const url = Buffer.from(strBytes, "hex").toString("utf8")

        map.set(merchant, url)
    }
    return map
}

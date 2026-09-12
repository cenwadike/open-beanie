// cspell:ignore chainlink
// src/index.ts
//
// The one file that touches the CRE runtime directly. Every other module
// in this project is plain TypeScript, callable and testable outside a
// CRE execution entirely — that separation is deliberate: it's the
// difference between "logic that might have a bug" and "logic that also
// requires a live CRE environment to even exercise."

import { bytesToHex, encodeAbiParameters } from "viem"

import { configSchema, type Config } from "./config"
import { CALL3_PARAMS } from "./constants"
import type { HttpRequester } from "./rpc"
import { rpcCall } from "./rpc"
import { discoverMerchants, discoverWebhookUrls } from "./discovery"
import { scanDeposits, needsRegistrationBatch } from "./deposits"
import { buildCallBatch } from "./batch"
import { signPayload, deliverWebhook } from "./webhook"
import { consensusIdenticalAggregation, CronCapability, EVMClient, getNetwork, handler, hexToBase64, HTTPClient, NodeRuntime, Runner, Runtime } from "@chainlink/cre-sdk"

type CycleResult = {
    calls: ReturnType<typeof buildCallBatch>
    webhookMap: [string, string][]
    activeReceivers: string[]
    merchantMap: [string, string][]
}

const onCronTick = (runtime: Runtime<Config>): string => {
    const cfg = runtime.config
    const network = getNetwork({
        chainFamily: "evm",
        chainSelectorName: cfg.chainSelectorName,
        isTestnet: cfg.isTestnet,
    })
    if (!network) throw new Error(`Unknown chain: ${cfg.chainSelectorName}`)

    // ── Detection phase — runs under DON consensus, all nodes must agree ────
    const result: CycleResult = runtime
        .runInNodeMode(
            (nodeRuntime: NodeRuntime<Config>) => {
                const http = new HTTPClient()
                const requester: HttpRequester = {
                    sendRequest: (req) => http.sendRequest(nodeRuntime, req as never),
                }

                const tipHex = rpcCall<string>(requester, cfg.rpcUrl, "eth_blockNumber", [])
                const tip = parseInt(tipHex, 16)

                const merchantMap = discoverMerchants(requester, cfg, tip)
                const webhookMap = discoverWebhookUrls(requester, cfg, tip)
                const activeSet = scanDeposits(requester, cfg, [...merchantMap.keys()], tip)

                if (activeSet.size === 0) {
                    return { calls: [], webhookMap: [], activeReceivers: [], merchantMap: [] }
                }

                const needsRegisterSet = needsRegistrationBatch(requester, cfg, [...activeSet])

                const calls = buildCallBatch([...activeSet], needsRegisterSet, merchantMap, cfg)

                return {
                    calls,
                    webhookMap: [...webhookMap.entries()],
                    activeReceivers: [...activeSet],
                    merchantMap: [...merchantMap.entries()],
                }
            },
            consensusIdenticalAggregation(),
        )()
        .result()

    if (result.calls.length === 0) {
        runtime.log("no active receivers this tick")
        return "no-op"
    }

    // ── Submission phase — one signed write for the whole cycle's batch ─────
    const encodedPayload = encodeAbiParameters(CALL3_PARAMS, [
        result.calls.map((c) => ({
            target: c.target,
            allowFailure: c.allowFailure,
            callData: c.callData,
        })),
    ])

    const reportResponse = runtime
        .report({
            encodedPayload: hexToBase64(encodedPayload),
            encoderName: "evm",
            signingAlgo: "ecdsa",
            hashingAlgo: "keccak256",
        })
        .result()

    const evmClient = new EVMClient(network.chainSelector.selector)
    const writeResult = evmClient
        .writeReport(runtime, {
            receiver: cfg.creKeeperReceiverAddress,
            report: reportResponse, // confirmed: pass .result() directly, no nested field
        })
        .result()

    const txHash = bytesToHex(writeResult.txHash || new Uint8Array(32))

    // ── Notification phase — best-effort, decoupled from settlement correctness ──
    const webhookMap = new Map(result.webhookMap)
    const merchantMap = new Map(result.merchantMap)
    const httpClient = new HTTPClient()

    for (const receiver of result.activeReceivers) {
        const merchant = merchantMap.get(receiver)
        const url = merchant ? webhookMap.get(merchant) : undefined
        if (!url) continue

        const payload = {
            chain: cfg.chainSelectorName,
            receiver,
            merchant,
            sweepTxHash: txHash,
            timestamp: Date.now(),
        }
        const sig = signPayload(runtime, JSON.stringify(payload))
        deliverWebhook(httpClient, runtime, url, payload, sig)
    }

    return `batch executed, tx ${txHash}`
}

const initWorkflow = (config: Config) => {
    const cron = new CronCapability()
    return [handler(cron.trigger({ schedule: config.schedule }), onCronTick)]
}

export { configSchema, initWorkflow, Config }


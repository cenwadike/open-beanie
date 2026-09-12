// src/workflow.ts
import { bytesToHex, encodeAbiParameters } from "viem"
import { configSchema, type Config } from "./config"
import { CALL3_PARAMS } from "./constants"
import type { HttpRequester } from "./rpc"
import { rpcCall } from "./rpc"
import { discoverMerchants, discoverWebhookUrls } from "./discovery"
import { scanDeposits, needsRegistrationBatch } from "./deposits"
import { buildCallBatch } from "./batch"
import { signPayload, deliverWebhook } from "./webhook"
import {
    CronCapability,
    EVMClient,
    getNetwork,
    handlerInTee,
    hexToBase64,
    HTTPClient,
    TeeRuntime,
} from "@chainlink/cre-sdk"

const onCronTick = (runtime: TeeRuntime<Config>): string => {
    const cfg = runtime.config
    const network = getNetwork({
        chainFamily: "evm",
        chainSelectorName: cfg.chainSelectorName,
        isTestnet: cfg.isTestnet,
    })
    if (!network) throw new Error(`Unknown chain: ${cfg.chainSelectorName}`)

    // ── Step 1: Execution inside TEE Enclave ─────────────────────────────────
    const httpClient = new HTTPClient()
    const requester: HttpRequester = {
        // Pass TeeRuntime directly to http.sendRequest
        sendRequest: (req) => httpClient.sendRequest(runtime, req as never),
    }

    const tipHex = rpcCall<string>(requester, cfg.rpcUrl, "eth_blockNumber", [])
    const tip = parseInt(tipHex, 16)

    const merchantMap = discoverMerchants(requester, cfg, tip)
    const webhookMap = discoverWebhookUrls(requester, cfg, tip)
    const activeSet = scanDeposits(requester, cfg, [...merchantMap.keys()], tip)

    if (activeSet.size === 0) {
        return "no-op"
    }

    const needsRegisterSet = needsRegistrationBatch(requester, cfg, [...activeSet])
    const calls = buildCallBatch([...activeSet], needsRegisterSet, merchantMap, cfg)

    // ── Step 2: Cross back to Workflow DON for On-Chain Consensus ───────────
    const donRuntime = runtime.usingTheDons()

    const encodedPayload = encodeAbiParameters(CALL3_PARAMS, [
        calls.map((c) => ({
            target: c.target,
            allowFailure: c.allowFailure,
            callData: c.callData,
        })),
    ])

    const reportResponse = donRuntime
        .report({
            encodedPayload: hexToBase64(encodedPayload),
            encoderName: "evm",
            signingAlgo: "ecdsa",
            hashingAlgo: "keccak256",
        })
        .result()

    const evmClient = new EVMClient(network.chainSelector.selector)
    const writeResult = evmClient
        .writeReport(donRuntime, {
            receiver: cfg.creKeeperReceiverAddress,
            report: reportResponse,
        })
        .result()

    const txHash = bytesToHex(writeResult.txHash || new Uint8Array(32))

    // ── Step 3: Best-effort Webhook Delivery from inside TEE ─────────────────
    for (const receiver of activeSet) {
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
        // Fetch secret inside the enclave dynamically
        const sig = signPayload(runtime, JSON.stringify(payload))
        deliverWebhook(httpClient, runtime, url, payload, sig)
    }

    return `batch executed, tx ${txHash}`
}

const initWorkflow = (config: Config) => {
    const cron = new CronCapability()
    // Use handlerInTee with TeeConstraints ({} accepts any registered TEE)
    return [handlerInTee(cron.trigger({ schedule: config.schedule }), onCronTick, {})]
}

export { configSchema, initWorkflow, Config }
// lane.js
//
// Lane creation and verification. No DOM here.
//
// TRUST MODEL
// Receiver addresses are ALWAYS computed locally from the on-chain factory
// (onchain.predictReceiver). The API response is never used as a source of
// addresses, so a compromised or spoofed server cannot substitute its own.
//
// Solana receivers are the exception: they are random server-side keypairs and
// cannot be predicted. They are found AFTER announcing, from the factory
// program's own on-chain ReceiverAnnounced event, and accepted only if the
// program accounts derived from (merchant, receiver, route) exist. If that
// cannot be done the lane simply has no Solana receiver; nothing is guessed.
//
// Private lanes hold ONE stealth account (on the settlement chain), derived
// from the passkey PRF. Other chains CCTP into it.

import * as api from "./api.js";
import {
    CHAINS,
    canDiscover,
    canSettle,
    chainByKey,
    laneChains,
    stealthReady,
    wire,
} from "./chains.js";
import { canonicalAddress, cctpRoute, merchantIdentity } from "./identity.js";
import { contractExists, discoverSolanaReceiver, headBlock, predictReceiver, verifySolanaReceiver } from "./onchain.js";
import { usdcAta } from "./solana.js";
import { getVerifiedToken, getVerifiedTokenWithPrf } from "./passkey.js";
import { markVerified, saveLane } from "./store.js";
import { deriveStealthAddress, generateLaneId, laneSalt, wipe } from "./stealth.core.js";
import { evaluatePrf } from "./passkey.js";

export class LaneError extends Error {
    constructor(message) {
        super(message);
        this.name = "LaneError";
    }
}

/** Predicts the receiver on every chain we can predict, exactly as the backend will register them. */
async function computeReceivers(target, targetRecipient, identityAddress) {
    const legs = laneChains();
    if (!canSettle(chainByKey(target) ?? {})) {
        throw new LaneError(`${chainByKey(target)?.name ?? target} is not available as a settlement chain yet.`);
    }

    return Promise.all(
        legs.map(async (leg) => {
            const route = await cctpRoute(leg.key, target, targetRecipient);
            // On the settlement leg the merchant is the payout recipient itself;
            // on every other leg the backend derives an identity from `address`.
            const merchant =
                leg.key === target ? targetRecipient : await merchantIdentity(leg.key, identityAddress);

            let predicted;
            try {
                predicted = await predictReceiver(leg.key, merchant, route);
            } catch (e) {
                throw new LaneError(`Could not compute the ${leg.name} receiver: ${e?.message || e}`);
            }
            const address = canonicalAddress(leg.key, predicted);
            if (!address || /^0x0+$/.test(address)) {
                throw new LaneError(`The ${leg.name} factory returned an invalid receiver address.`);
            }
            return { chain: leg.key, address, merchant };
        })
    );
}

/** (merchant, route) the backend announces on the Solana leg, as the program will see them. */
async function solanaLeg(target, targetRecipient, identityAddress) {
    const merchant = target === "SOLANA" ? targetRecipient : await merchantIdentity("SOLANA", identityAddress);
    const route = await cctpRoute("SOLANA", target, targetRecipient);
    return { merchant, route };
}

async function snapshotStartBlocks(receivers) {
    const heads = await Promise.all(
        receivers.map((r) => headBlock(r.chain).catch(() => null))
    );
    return receivers.map((r, i) => ({
        ...r,
        // Rewind slightly so a deposit racing creation is still found.
        startBlock: heads[i] == null ? null : Math.max(0, heads[i] - 10),
    }));
}

/**
 * Creates a lane end to end.
 *   wallet : merchant payout wallet on `targetChain` (required for public lanes)
 * Returns the saved lane record.
 */
export async function createLane({ wallet, targetChain, webhookUrl = null, privacy = false, onStep = () => { } }) {
    const target = wire(targetChain);
    const targetCfg = chainByKey(target);
    if (!targetCfg) throw new LaneError("Choose a settlement chain.");

    let identityAddress;
    let targetRecipient;
    let merchantAddress = null;
    let verifiedToken;
    let prfCredentialId = null;
    let laneId;

    if (privacy) {
        if (!stealthReady(targetCfg)) {
            throw new LaneError(`Private lanes are not enabled on ${targetCfg.name} yet.`);
        }
        laneId = await generateLaneId(String(wallet || ""));
        onStep("Confirm passkey…");
        const r = await getVerifiedTokenWithPrf(`create-lane:${laneId}`, await laneSalt(laneId), { maxUses: 1 });
        verifiedToken = r.verifiedToken;
        prfCredentialId = r.prfCredentialId;
        try {
            const account = await deriveStealthAddress({ laneSecret: r.prfOutput, laneId, index: 0, chain: target });
            targetRecipient = account.address;
        } finally {
            wipe(r.prfOutput);
        }
        identityAddress = targetRecipient;
    } else {
        const canon = canonicalAddress(target, wallet);
        if (!canon) throw new LaneError(`Enter a valid ${targetCfg.name} address.`);
        merchantAddress = canon;
        targetRecipient = canon;
        identityAddress = canon;
        laneId = await generateLaneId(canon);
    }

    onStep("Computing addresses…");
    const predicted = await computeReceivers(target, targetRecipient, identityAddress);
    const receivers = await snapshotStartBlocks(predicted);

    if (!privacy) {
        onStep("Confirm passkey…");
        ({ verifiedToken } = await getVerifiedToken(`create-lane:${laneId}`, { maxUses: 1 }));
    }

    const solanaOn = canDiscover(CHAINS.SOLANA);
    let solanaFrom = null;
    if (solanaOn) solanaFrom = await headBlock("SOLANA").catch(() => null);

    onStep("Announcing…");
    await api.createLane({
        chain: target,
        address: identityAddress,
        laneId,
        verifiedToken,
        targetChain: target,
        targetRecipient,
        webhookUrl,
    });

    const warnings = [];
    if (target === "SOLANA") {
        // The program requires the merchant's token account to exist (register_merchant,
        // sweep) and CCTP needs it at receiveMessage.
        try {
            const ata = await usdcAta(targetRecipient, CHAINS.SOLANA.usdc);
            if (!(await contractExists("SOLANA", ata))) {
                warnings.push("The payout wallet has no USDC token account yet. Create it (receive any USDC once) before the first payment.");
            }
        } catch {
            /* advisory only */
        }
    }
    if (solanaOn) {
        onStep("Finding Solana receiver…");
        try {
            if (solanaFrom == null) throw new Error("Solana RPC unavailable");
            const leg = await solanaLeg(target, targetRecipient, identityAddress);
            const address = await discoverSolanaReceiver({ ...leg, fromSlot: solanaFrom });
            if (address) {
                receivers.push({ chain: "SOLANA", address, merchant: leg.merchant, startBlock: Math.max(0, solanaFrom - 10) });
            } else {
                warnings.push("The Solana receiver was not found on-chain in time, so this lane has no Solana option.");
            }
        } catch (e) {
            warnings.push(`Solana receiver skipped: ${e?.message || e}`);
        }
    }

    const lane = {
        v: 2,
        id: laneId,
        privacy,
        targetChain: target,
        targetRecipient,
        merchantAddress,
        index: 0,
        prfCredentialId,
        webhookUrl,
        createdAt: Date.now(),
        receivers,
    };
    if (!saveLane(lane)) {
        throw new LaneError(
            "The lane was announced but could not be saved in this browser (storage full or blocked). " +
            `Write down lane id ${laneId} before leaving this page.`
        );
    }
    return Object.assign(lane, { warnings }); // not persisted
}

/**
 * Re-derives everything locally and compares with the stored lane. Used for
 * lanes that came from a backup file (untrusted input). Private lanes prompt
 * for the passkey. Returns true and clears `needsVerify` on success; throws
 * with a clear message on any mismatch.
 */
export async function verifyLane(lane) {
    const target = wire(lane.targetChain);
    let targetRecipient = lane.targetRecipient;
    let identityAddress = lane.merchantAddress;

    if (lane.privacy) {
        const secret = await evaluatePrf(await laneSalt(lane.id), lane.prfCredentialId || undefined);
        try {
            const account = await deriveStealthAddress({
                laneSecret: secret,
                laneId: lane.id,
                index: lane.index || 0,
                chain: target,
            });
            if (account.address !== lane.targetRecipient) {
                throw new LaneError("This passkey does not derive the lane's payout account. Wrong passkey, or a tampered backup.");
            }
            targetRecipient = account.address;
            identityAddress = account.address;
        } finally {
            wipe(secret);
        }
    } else {
        const canon = canonicalAddress(target, lane.merchantAddress);
        if (!canon || canon !== lane.targetRecipient) {
            throw new LaneError("Lane payout address does not match its merchant address.");
        }
        identityAddress = canon;
    }

    const recomputed = await computeReceivers(target, targetRecipient, identityAddress);
    for (const r of lane.receivers) {
        if (chainByKey(r.chain)?.kind === "solana") {
            // Not predictable: check the program accounts for (merchant, receiver, route) exist.
            const leg = await solanaLeg(target, targetRecipient, identityAddress);
            if (!(await verifySolanaReceiver({ ...leg, receiver: r.address }))) {
                throw new LaneError("The Solana receiver is not announced on-chain for this merchant and route.");
            }
            continue;
        }
        const match = recomputed.find((x) => x.chain === wire(r.chain));
        if (!match || match.address !== r.address) {
            throw new LaneError(`The ${chainByKey(r.chain)?.name ?? r.chain} receiver does not match the on-chain factory's answer.`);
        }
    }
    markVerified(lane.id);
    return true;
}

/** Link a merchant shares with payers. Contains only public data. */
export function laneShareUrl(lane) {
    const url = new URL("/pay", window.location.origin);
    url.searchParams.set("lane", lane.id);
    url.searchParams.set("target", lane.targetChain);
    url.searchParams.set("merchant", lane.targetRecipient);
    if (lane.privacy) url.searchParams.set("privacy", "1");
    for (const r of lane.receivers) url.searchParams.append("r", `${r.chain}:${r.address}`);
    return url.toString();
}
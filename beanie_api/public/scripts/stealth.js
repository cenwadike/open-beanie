// stealth.js  (load with <script type="module" src="/scripts/stealth.js">)
//
// Private-lane page: balances, recovery/verification, and claiming.
//
// Balances need NO passkey prompt: a lane's payout account address is public
// and cached in the lane record. The passkey is used only to (a) verify a lane
// restored from a backup, and (b) sign a claim.
//
// Claims are built locally in stealth.claim.js in the exact formats validated
// by the backend: EIP-3009 digest, Starknet INVOKE V3 hash, or Solana message hash.
//
// A builder receives { lane, signer, destination, balance } and returns
//   { txHash, clientSig, calls, auth3009?, starknet?, messageBytes? }
// where txHash is exactly the hash the claim binding and signature use.

import * as api from "./api.js";
import { CHAINS, apiChain, chainByKey, chainIcon, chainLabel, wire } from "./chains.js";
import { canonicalAddress } from "./identity.js";
import { resolveStealthConfig, verifyLane } from "./lane.js";
import { tokenBalance } from "./onchain.js";
import { evaluatePrf, getVerifiedToken } from "./passkey.js";
import { deriveStealthSigner, laneSalt, wipe } from "./stealth.core.js";
import { CLAIM_BUILDERS } from "./stealth.claim.js";
import * as store from "./store.js";
import { $, describeError, downloadText, escapeHtml, formatUsdc, notify, short } from "./ui.js";
import { ApiError } from "./api.js";

let rows = []; // { lane, balance: bigint|null }
let selected = null;
let filter = "ALL";
let busy = false;

const setStatus = (msg) => {
    const el = $("#scan-status");
    if (el) el.textContent = msg;
};

const privateLanes = () => store.getLanes().filter((l) => l.privacy);
const visibleRows = () => rows.filter((r) => filter === "ALL" || r.lane.targetChain === filter);

/* ---------- scan (public, no prompts) ---------- */

async function refresh() {
    setStatus("Checking balances…");
    const lanes = privateLanes();
    rows = await Promise.all(
        lanes.map(async (lane) => {
            try {
                return { lane, balance: await tokenBalance(lane.targetChain, lane.targetRecipient) };
            } catch {
                return { lane, balance: null };
            }
        })
    );
    render();
    const failed = rows.filter((r) => r.balance === null).length;
    setStatus(failed ? `Some balances could not be read (${failed}). Try again shortly.` : "Up to date");
}

function render() {
    const list = $("#matches-list");
    if (!list) return;
    const shown = visibleRows();

    const total = shown.reduce((s, r) => s + (r.balance ?? 0n), 0n);
    const totalEl = $("#shieldedBalance");
    if (totalEl) totalEl.textContent = `${formatUsdc(total)} USDC`;

    if (!shown.length) {
        const scope = filter === "ALL" ? "all chains" : chainLabel(filter);
        list.innerHTML = `<div class="history-empty">No private lanes on ${escapeHtml(scope)}. Create one, or import a backup.</div>`;
    } else {
        list.innerHTML = "";
        for (const r of shown) {
            const row = document.createElement("div");
            row.className = `history-row${selected?.lane.id === r.lane.id ? " selected" : ""}`;
            row.innerHTML = `
        <span class="mini-icon">${chainIcon(r.lane.targetChain)}</span>
        <span class="mono">${escapeHtml(short(r.lane.targetRecipient))}${r.lane.needsVerify ? " · unverified" : ""}</span>
        <strong>${r.balance === null ? "—" : `${escapeHtml(formatUsdc(r.balance))} USDC`}</strong>`;
            row.addEventListener("click", () => {
                selected = r;
                render();
            });
            list.append(row);
        }
    }
    const btn = $("#unshieldBtn");
    if (btn) btn.disabled = busy || !(selected?.balance > 0n);
}

/* ---------- claim ---------- */

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function pollClaimBalance(lane, previousBalance, reference) {
    for (let attempt = 0; attempt < 5; attempt++) {
        await delay(4000);
        try {
            const balance = await tokenBalance(lane.targetChain, lane.targetRecipient);
            rows = rows.map((row) => row.lane.id === lane.id ? { ...row, balance } : row);
            if (selected?.lane.id === lane.id) selected = { lane, balance };
            render();
            if (balance < previousBalance) {
                if (selected?.lane.id === lane.id) {
                    setStatus(`Claim reference ${short(reference)} processed. Remaining balance: ${formatUsdc(balance)} USDC.`);
                }
                return;
            }
        } catch {
            // The queue has no status route; keep checking the chain balance.
        }
    }
    if (selected?.lane.id === lane.id) {
        setStatus(`Claim reference ${short(reference)} is queued. The balance has not changed yet.`);
    }
}

function claimErrorMessage(err) {
    if (!(err instanceof ApiError)) return describeError(err);
    if (err.status === 400) return `Claim rejected: ${err.message.replace(/^Claim rejected:\s*/i, "")}`;
    if (err.status === 401) return "The passkey approval expired or did not match this claim. Please try again.";
    if (err.status === 429) return err.retryAfter
        ? `Too many claim attempts. Try again in ${err.retryAfter} seconds.`
        : "Too many claim attempts. Try again later.";
    if (err.status === 503) return "Claims are temporarily unavailable for this chain. Your funds remain in the private lane.";
    return describeError(err);
}

async function claim() {
    if (busy) return;
    if (!selected) return setStatus("Select a lane first.");
    const { lane, balance } = selected;
    const destination = canonicalAddress(lane.targetChain, $("#destination-address")?.value || "");
    if (!destination) return setStatus(`Enter a valid ${chainLabel(lane.targetChain)} destination address.`);
    if (!(balance > 0n)) return setStatus("Nothing to claim on this lane.");

    const chain = chainByKey(lane.targetChain);
    const builder = CLAIM_BUILDERS[chain?.kind];
    if (!builder) {
        return setStatus(
            `Claiming on ${chainLabel(lane.targetChain)} is not supported yet.`
        );
    }

    busy = true;
    render();
    let secret = null;
    let signer = null;
    try {
        const config = await resolveStealthConfig(chain, lane.stealthConfig ?? null);
        setStatus("Confirm your passkey to unlock this lane…");
        secret = await evaluatePrf(await laneSalt(lane.id), lane.prfCredentialId || undefined);
        signer = await deriveStealthSigner({
            laneSecret: secret,
            laneId: lane.id,
            index: lane.index || 0,
            chain: lane.targetChain,
            config,
        });
        if (signer.address !== lane.targetRecipient) {
            throw new Error("This passkey does not match the lane. Use the passkey the lane was created with.");
        }

        // Order matters: build -> hash -> token (bound to the hash) -> post.
        setStatus("Building claim…");
        const built = await builder({ lane, signer, destination, balance, config });
        if (!built?.txHash || !built?.clientSig || !built?.derivedAddress) throw new Error("Claim builder returned an incomplete result.");

        setStatus("Confirm again to authorize this claim…");
        const binding = `claim:${apiChain(lane.targetChain)}:${built.derivedAddress}:${built.txHash}`;
        const { verifiedToken } = await getVerifiedToken(binding, { maxUses: 1 });

        setStatus("Submitting…");
        const res = await api.submitStealthClaim({
            chain: lane.targetChain,
            txHash: built.txHash,
            derivedAddress: built.derivedAddress,
            clientSig: built.clientSig,
            verifiedToken,
            calls: built.calls,
            auth3009: built.auth3009,
            starknet: built.starknet,
            messageBytes: built.messageBytes,
        });
        const reference = res?.transaction_hash || built.txHash;
        setStatus(`Claim queued. Claim reference: ${short(reference)}. Checking balance…`);
        notify("Claim queued", "success", `Claim reference ${short(reference)}`);
        void pollClaimBalance(lane, balance, reference);
    } catch (err) {
        console.error("[stealth] claim failed", err);
        setStatus(`Claim failed: ${claimErrorMessage(err)}`);
    } finally {
        wipe(secret);
        signer = null;
        busy = false;
        render();
    }
}

/* ---------- wiring ---------- */

$("#unshieldBtn")?.addEventListener("click", claim);

$("#verifyBtn")?.addEventListener("click", async () => {
    if (!selected) return setStatus("Select a lane first.");
    try {
        setStatus("Verifying lane…");
        await verifyLane(selected.lane);
        setStatus("Lane verified.");
        render();
    } catch (err) {
        setStatus(`Verification failed: ${describeError(err)}`);
    }
});

$("#refreshBtn")?.addEventListener("click", () => refresh().catch((e) => setStatus(describeError(e))));

$("#exportBackupBtn")?.addEventListener("click", () =>
    downloadText(`beanie-backup-${new Date().toISOString().slice(0, 10)}.json`, store.exportBackup())
);
$("#importBackupInput")?.addEventListener("change", async (e) => {
    const file = e.target.files?.[0];
    if (!file) return;
    try {
        if (file.size > 2_000_000) throw new Error("That file is too large to be a Beanie backup.");
        const { added, skipped } = store.importBackup(await file.text());
        setStatus(`Imported ${added} lane(s), skipped ${skipped}. Select each one and press Verify.`);
        await refresh();
    } catch (err) {
        setStatus(`Import failed: ${describeError(err)}`);
    }
    e.target.value = "";
});

// chain filter (same markup as the history modal)
const menu = $("#stealthChainMenu");
function updateChainControl() {
    const icon = $("#stealthChainIcon");
    const name = $("#stealthChainName");
    if (icon) icon.innerHTML = filter === "ALL" ? "🔗" : chainIcon(filter);
    if (name) name.textContent = filter === "ALL" ? "All chains" : chainLabel(filter);
}
$("#stealthChainSelect")?.addEventListener("click", (e) => {
    e.stopPropagation();
    menu?.classList.toggle("open");
});
menu?.addEventListener("click", (e) => {
    const option = e.target.closest(".history-chain-option");
    if (!option) return;
    filter = option.dataset.chain === "all" ? "ALL" : wire(option.dataset.chain);
    menu.querySelectorAll(".history-chain-option").forEach((o) =>
        o.setAttribute("aria-selected", String(o.dataset.chain === option.dataset.chain))
    );
    updateChainControl();
    menu.classList.remove("open");
    render();
});
document.addEventListener("click", (e) => {
    if (!e.target.closest("#stealthChainSelect") && !e.target.closest("#stealthChainMenu")) menu?.classList.remove("open");
});

updateChainControl();
refresh().catch((e) => setStatus(`Scan error: ${describeError(e)}`));
// beanie.js  (load with <script type="module" src="/scripts/beanie.js">)
//
// Controller for the lane-creation page: form, lane list, history modal,
// backup/restore. Business logic lives in lane.js; chain access in onchain.js;
// polling in watcher.js.

import { CHAINS, chainIcon, chainLabel, stealthReady, wire } from "./chains.js";
import { canonicalAddress } from "./identity.js";
import { createLane, laneShareUrl, verifyLane } from "./lane.js";
import * as store from "./store.js";
import * as watcher from "./watcher.js";
import {
  $, copyText, describeError, downloadText, escapeHtml, formatUsdc, notify, sanitizeWebhookUrl, short,
} from "./ui.js";

const OPTION_TO_CHAIN = { base: "BASE", starknet: "STARKNET" };

/* ---------- create ---------- */

const form = $("#receiverForm");
const submitBtn = $("#createReceiverBtn") || $("#submitBtn");
const defaultLabel = submitBtn?.textContent || "Create Payment Link";
let busy = false;

const privacyToggle = $("#privacyToggle");
const privacyOn = () => Boolean(privacyToggle?.checked);

async function handleSubmit(event) {
  event?.preventDefault();
  if (busy) return;

  const walletEl = $("#wallet") || $("#merchantAddress");
  const settlementEl = $("#settlementChain") || $("#targetChain");
  const privacy = privacyOn();

  const choice = settlementEl?.value || "";
  const target = OPTION_TO_CHAIN[choice] || wire(choice);
  walletEl?.classList.remove("invalid");

  if (!CHAINS[target]) {
    notify("Choose a settlement chain", "error", "Base and Starknet are supported right now.");
    return;
  }
  const wallet = walletEl?.value?.trim() || "";
  if (!privacy && !canonicalAddress(target, wallet)) {
    walletEl?.classList.add("invalid");
    notify("Check your wallet address", "error", `Enter a valid ${chainLabel(target)} address.`);
    return;
  }
  if (privacy && !stealthReady(CHAINS[target])) {
    notify("Private lanes unavailable", "error", `Privacy is not enabled on ${chainLabel(target)} yet. Use a public lane.`);
    return;
  }
  const webhook = sanitizeWebhookUrl($("#webhookUrl")?.value || "");
  if (!webhook.ok) {
    notify("Check your webhook URL", "error", webhook.error);
    return;
  }

  busy = true;
  if (submitBtn) submitBtn.disabled = true;
  const step = (label) => submitBtn && (submitBtn.textContent = label);
  step(privacy ? "Setting up private lane…" : "Creating…");

  try {
    const lane = await createLane({ wallet, targetChain: target, webhookUrl: webhook.value, privacy, onStep: step });

    if (lane.privacy) {
      // The lane id is what lets a new device rediscover this lane. Make sure
      // the user leaves with a copy.
      downloadText(`beanie-backup-${new Date().toISOString().slice(0, 10)}.json`, store.exportBackup());
      notify("Backup downloaded", "info", "Keep this file safe: it is how you recover private lanes on a new device.");
    }
    notify(
      privacy ? "Private payment lane ready" : "Payment lane created",
      "success",
      privacy ? "Payouts go to a passkey-derived account; your wallet stays off-chain." : "Receivers announced."
    );
    renderLanes();
    watcher.pollNow().catch(() => { });
    window.location.href = laneShareUrl(lane);
  } catch (err) {
    console.error("[beanie] create failed:", err);
    notify("Could not create payment lane", "error", describeError(err));
    busy = false;
    if (submitBtn) {
      submitBtn.disabled = false;
      submitBtn.textContent = defaultLabel;
    }
  }
}

form?.addEventListener("submit", handleSubmit);

/* ---------- privacy segmented control ---------- */

const openBtn = $("#privacyOpen");
const stealthBtn = $("#privacyStealth");
openBtn?.addEventListener("click", () => {
  openBtn.setAttribute("aria-pressed", "true");
  stealthBtn?.setAttribute("aria-pressed", "false");
  if (privacyToggle) privacyToggle.checked = false;
});
stealthBtn?.addEventListener("click", () => {
  stealthBtn.setAttribute("aria-pressed", "true");
  openBtn?.setAttribute("aria-pressed", "false");
  if (privacyToggle) privacyToggle.checked = true;
});

const selectEl = $("#settlementChain");
selectEl?.addEventListener("change", () => {
  const target = OPTION_TO_CHAIN[selectEl.value] || wire(selectEl.value);
  const summary = $("#routeSummary");
  if (summary) summary.textContent = CHAINS[target] ? `USDC lands on ${chainLabel(target)}` : "USDC lands on xxxxx";
  const walletEl = $("#wallet");
  if (walletEl?.value.trim() && CHAINS[target]) {
    walletEl.classList.toggle("invalid", !canonicalAddress(target, walletEl.value.trim()));
  }
});

/* ---------- lane list ---------- */

function renderLanes() {
  const lanes = store.getLanes();
  const subtitle = $("#receiversSubtitle");
  if (subtitle) subtitle.textContent = lanes.length ? `${lanes.length} lane${lanes.length > 1 ? "s" : ""}` : "No lanes yet";

  const list = $("#receiversList");
  if (!list) return;
  list.innerHTML = lanes.length ? "" : `<p class="receiver-empty">No lanes yet — create a payment lane to see it here.</p>`;

  for (const lane of lanes) {
    const label = lane.privacy ? "Private lane" : short(lane.merchantAddress || lane.targetRecipient);
    const row = document.createElement("div");
    row.className = "receiver-row";
    row.style.gridTemplateColumns = "24px 1fr auto auto";
    row.innerHTML = `
      <span class="mini-icon">${chainIcon(lane.targetChain)}</span>
      <span>
        <div>${escapeHtml(label)}</div>
        <div class="mono">settles on ${escapeHtml(chainLabel(lane.targetChain))}${lane.privacy ? " · private" : ""} · ${lane.receivers.length} networks${lane.needsVerify ? " · unverified" : ""}</div>
      </span>`;

    if (lane.needsVerify) {
      const verify = document.createElement("button");
      verify.type = "button";
      verify.className = "receiver-link";
      verify.textContent = "Verify";
      verify.addEventListener("click", async () => {
        verify.disabled = true;
        try {
          await verifyLane(lane);
          notify("Lane verified", "success", "Addresses match the on-chain factory.");
        } catch (e) {
          notify("Verification failed", "error", describeError(e));
        }
        renderLanes();
      });
      row.append(verify, document.createElement("span"));
    } else {
      const url = laneShareUrl(lane);
      const link = document.createElement("a");
      link.className = "receiver-link";
      link.href = url;
      link.target = "_blank";
      link.rel = "noreferrer";
      link.textContent = "Pay page";
      const copy = document.createElement("button");
      copy.type = "button";
      copy.className = "receiver-copy";
      copy.textContent = "⧉";
      copy.addEventListener("click", () => copyText(url, "Pay link copied"));
      row.append(link, copy);
    }
    list.append(row);
  }
}

/* ---------- history ---------- */

let historyFilter = "ALL";

function renderChainMenu() {
  const menu = $("#historyChainMenu");
  if (!menu) return;
  menu.innerHTML = ["ALL", ...Object.keys(CHAINS)]
    .map(
      (key) => `
    <button class="history-chain-option" type="button" data-chain="${escapeHtml(key)}" aria-selected="${key === historyFilter}">
      <span class="mini-icon">${key === "ALL" ? "🔗" : chainIcon(key)}</span>
      <span>${key === "ALL" ? "All chains" : escapeHtml(chainLabel(key))}</span>
    </button>`
    )
    .join("");
}

function updateChainControl() {
  const icon = $("#historyChainIcon");
  const name = $("#historyChainName");
  if (icon) icon.innerHTML = historyFilter === "ALL" ? "🔗" : chainIcon(historyFilter);
  if (name) name.textContent = historyFilter === "ALL" ? "All chains" : chainLabel(historyFilter);
}

function renderHistory() {
  const rows = store.getHistory().filter((r) => historyFilter === "ALL" || r.chain === historyFilter);
  const scope = historyFilter === "ALL" ? "all chains" : chainLabel(historyFilter);
  const total = rows.reduce((sum, r) => sum + BigInt(r.amount || "0"), 0n);

  const panel = $("#balancePanel");
  if (panel) {
    panel.innerHTML = `
      <div class="balance-card"><div>
        <span class="balance-label">Total received on ${escapeHtml(scope)}</span>
        <div class="balance-value">${escapeHtml(formatUsdc(total))} USDC</div>
      </div></div>`;
  }

  const list = $("#historyList");
  if (list) {
    list.innerHTML = rows.length
      ? rows
        .map(
          (r) => `
        <div class="history-row">
          <span class="mini-icon">${chainIcon(r.chain)}</span>
          <span class="mono">${escapeHtml(short(r.address))}</span>
          <strong>${escapeHtml(formatUsdc(r.amount))} USDC</strong>
          <span>${escapeHtml(new Date(r.time).toLocaleString())}</span>
        </div>`
        )
        .join("")
      : `<p class="history-empty">History is empty — no deposits detected yet on ${escapeHtml(scope)}.</p>`;
  }

  const subtitle = $("#historySubtitle");
  if (subtitle) subtitle.textContent = rows.length ? `${rows.length} deposit${rows.length === 1 ? "" : "s"} received` : "Nothing yet";
}

function notifyDot() {
  const btn = $("#historyBtn");
  if (!btn) return;
  let dot = btn.querySelector("#historyDot");
  if (!dot) {
    dot = document.createElement("span");
    dot.id = "historyDot";
    dot.className = "notify-dot";
    btn.append(dot);
  }
  dot.hidden = store.unseenCount() === 0;
}

function openHistory() {
  updateChainControl();
  renderChainMenu();
  renderHistory();
  $("#historyModal")?.classList.add("open");
  store.markSeen();
  notifyDot();
}

$("#historyBtn")?.addEventListener("click", openHistory);
$("#closeHistoryModal")?.addEventListener("click", () => $("#historyModal")?.classList.remove("open"));
$("#historyModal")?.addEventListener("click", (e) => {
  if (e.target.id === "historyModal") $("#historyModal")?.classList.remove("open");
});
$("#historyChainSelect")?.addEventListener("click", (e) => {
  e.stopPropagation();
  renderChainMenu();
  $("#historyChainMenu")?.classList.toggle("open");
});
$("#historyChainMenu")?.addEventListener("click", (e) => {
  const option = e.target.closest(".history-chain-option");
  if (!option) return;
  historyFilter = option.dataset.chain;
  updateChainControl();
  renderChainMenu();
  $("#historyChainMenu")?.classList.remove("open");
  renderHistory();
});
document.addEventListener("click", (e) => {
  if (!e.target.closest("#historyChainSelect") && !e.target.closest("#historyChainMenu")) {
    $("#historyChainMenu")?.classList.remove("open");
  }
});

$("#receiversBtn")?.addEventListener("click", () => {
  renderLanes();
  $("#receiversModal")?.classList.add("open");
});
$("#closeReceiversModal")?.addEventListener("click", () => $("#receiversModal")?.classList.remove("open"));
$("#receiversModal")?.addEventListener("click", (e) => {
  if (e.target.id === "receiversModal") $("#receiversModal")?.classList.remove("open");
});

/* ---------- backup / restore (optional elements) ---------- */

$("#exportBackupBtn")?.addEventListener("click", () => {
  downloadText(`beanie-backup-${new Date().toISOString().slice(0, 10)}.json`, store.exportBackup());
});
$("#importBackupInput")?.addEventListener("change", async (e) => {
  const file = e.target.files?.[0];
  if (!file) return;
  try {
    if (file.size > 2_000_000) throw new Error("That file is too large to be a Beanie backup.");
    const { added, skipped } = store.importBackup(await file.text());
    notify("Backup imported", "success", `${added} lane(s) added, ${skipped} skipped. Verify them before sharing.`);
    renderLanes();
  } catch (err) {
    notify("Import failed", "error", describeError(err));
  }
  e.target.value = "";
});

/* ---------- init ---------- */

$("#shareBtn")?.classList.toggle("is-hidden", store.getLanes().length === 0);
renderLanes();
notifyDot();

watcher.onDeposit((d) => {
  notify("Deposit detected", "success", `${formatUsdc(d.amount)} USDC on ${chainLabel(d.chain)}`);
  notifyDot();
  if ($("#historyModal")?.classList.contains("open")) renderHistory();
});
watcher.start();
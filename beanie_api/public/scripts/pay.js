// pay.js  (load with <script type="module" src="/scripts/pay.js">)
//
// Payer-facing page. Shows the receiver addresses from the link, and offers a
// gasless "Pay with Beanie" flow. It never asks the server for addresses.
//
// Link format (all public data):
//   /pay?lane=<id>&target=<CHAIN>&merchant=<recipient>&r=<CHAIN>:<receiver>&r=...

import * as api from "./api.js";
import { CHAINS, KEEPER_STARKNET_ADDRESS, SOLANA_RECEIVER_KIND, USDC_DECIMALS, canGasless, chainIcon, wire } from "./chains.js";
import { awaitSolanaSettlement, signEvmTransfer, signSolanaTransfer, signStarknetTransfer } from "./gasless.js";
import { canonicalAddress } from "./identity.js";
import { getLane } from "./store.js";
import { copyText, describeError, escapeHtml, notify, parseDecimalToRawUnits } from "./ui.js";

const laneCard = document.querySelector("#laneCard");
const shareBtn = document.querySelector("#payShareBtn");

const poweredTag = (withEmbed) => `
  <footer class="powered-tag-wrapper">
    ${withEmbed
    ? `<button type="button" class="embed-tag-btn" aria-label="Copy as widget" title="Copy as widget">
             <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="8 6 2 12 8 18"></polyline><polyline points="16 6 22 12 16 18"></polyline></svg>
           </button>`
    : ""
  }
    <div class="powered-tag" aria-label="Powered by Beanie">
      <span class="powered-tag__label">powered by</span>
      <span class="powered-tag__mark">bean<span class="powered-tag__dot">:</span>ie</span>
    </div>
  </footer>`;

/* ---------- link parsing ---------- */

function parseLink() {
  const params = new URLSearchParams(window.location.search);
  const laneId = params.get("lane") || "";

  let routes = params
    .getAll("r")
    .map((pair) => {
      const i = pair.indexOf(":");
      if (i === -1) return null;
      const chain = wire(pair.slice(0, i));
      const address = canonicalAddress(chain, pair.slice(i + 1));
      return CHAINS[chain] && address ? { chain, address } : null;
    })
    .filter(Boolean);

  let target = wire(params.get("target"));
  let merchant = params.get("merchant") || "";

  // Merchant's own browser only: fall back to the locally stored lane.
  if (!routes.length && laneId) {
    const lane = getLane(laneId);
    if (lane && !lane.needsVerify) {
      routes = lane.receivers.map((r) => ({ chain: r.chain, address: r.address }));
      target = target || lane.targetChain;
      merchant = merchant || lane.targetRecipient;
    }
  }

  if (!CHAINS[target]) target = "";
  if (target && !canonicalAddress(target, merchant)) merchant = "";
  return { params, laneId, routes, target, merchant };
}

/* ---------- theming + embed ---------- */

function applyTheme(params) {
  const toRgb = (input) => {
    const m = /^#?([0-9a-f]{6})$/i.exec(input || "");
    return m ? [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16)).join(", ") : null;
  };
  const primary = toRgb(params.get("primaryColor"));
  if (primary) document.documentElement.style.setProperty("--primary-rgb", primary);
  const secondary = toRgb(params.get("secondaryColor"));
  if (secondary) document.documentElement.style.setProperty("--secondary-rgb", secondary);
}

function rgbVarToHex(name, fallback) {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const parts = raw.split(",").map((n) => parseInt(n.trim(), 10));
  if (parts.length !== 3 || parts.some(Number.isNaN)) return fallback;
  return "#" + parts.map((n) => n.toString(16).padStart(2, "0")).join("");
}

function buildEmbedSnippet(link) {
  const attrs = [];
  if (link.laneId) attrs.push(["data-lane", link.laneId]);
  attrs.push(["data-routes", link.routes.map((r) => `${r.chain}:${r.address}`).join(",")]);
  if (link.target) attrs.push(["data-target", link.target]);
  if (link.merchant) attrs.push(["data-merchant", link.merchant]);
  attrs.push(["data-primary-color", rgbVarToHex("--primary-rgb", "#b8c99a")]);
  attrs.push(["data-secondary-color", rgbVarToHex("--secondary-rgb", "#6248b0")]);
  const lines = attrs.map(([k, v]) => `  ${k}="${String(v).replace(/"/g, "&quot;")}"`).join("\n");
  return `<div\n  data-beanie-checkout\n${lines}\n></div>\n<script src="${window.location.origin}/embed.js"></script>`;
}

function openEmbedOverlay(link) {
  let overlay = document.getElementById("embedOverlay");
  if (!overlay) {
    overlay = document.createElement("div");
    overlay.id = "embedOverlay";
    overlay.className = "embed-overlay";
    overlay.hidden = true;
    overlay.innerHTML = `
      <div class="embed-modal" role="dialog" aria-modal="true" aria-label="Copy as widget">
        <div class="embed-modal-head"><strong>Copy as widget</strong>
          <button type="button" class="embed-close" aria-label="Close">×</button></div>
        <pre class="embed-snippet" id="embedSnippetText"></pre>
        <button type="button" class="copy-btn" id="embedCopyBtn">Copy</button>
        <p class="status-hint">Uses the routing and colors you're viewing right now.</p>
      </div>`;
    document.body.append(overlay);
    const close = () => (overlay.hidden = true);
    overlay.addEventListener("click", (e) => e.target === overlay && close());
    overlay.querySelector(".embed-close").addEventListener("click", close);
    document.addEventListener("keydown", (e) => e.key === "Escape" && !overlay.hidden && close());
  }
  const snippet = buildEmbedSnippet(link);
  overlay.querySelector("#embedSnippetText").textContent = snippet;
  const copyBtn = overlay.querySelector("#embedCopyBtn");
  copyBtn.textContent = "Copy";
  copyBtn.onclick = async () => {
    if (await copyText(snippet, "Widget copied to clipboard")) {
      copyBtn.textContent = "Copied";
      setTimeout(() => (overlay.hidden = true), 500);
    }
  };
  overlay.hidden = false;
}

/* ---------- QR (generated locally: the address never goes to a third party) ---------- */

let qrLibPromise = null;
function loadQrLib() {
  if (window.qrcode) return Promise.resolve(window.qrcode);
  qrLibPromise ??= new Promise((resolve, reject) => {
    const s = document.createElement("script");
    s.src = "https://cdnjs.cloudflare.com/ajax/libs/qrcode-generator/1.4.4/qrcode.min.js";
    s.crossOrigin = "anonymous";
    s.onload = () => (window.qrcode ? resolve(window.qrcode) : reject(new Error("qr lib missing")));
    s.onerror = () => reject(new Error("qr lib failed to load"));
    document.head.append(s);
  });
  return qrLibPromise;
}

async function renderQr(container, text) {
  try {
    const qrcode = await loadQrLib();
    const qr = qrcode(0, "M");
    qr.addData(text);
    qr.make();
    container.innerHTML = qr.createSvgTag({ cellSize: 4, margin: 2, scalable: true });
    const svg = container.querySelector("svg");
    if (svg) {
      svg.setAttribute("width", "240");
      svg.setAttribute("height", "240");
      svg.setAttribute("role", "img");
      svg.setAttribute("aria-label", "QR code");
    }
  } catch {
    container.textContent = ""; // address + copy button remain usable
  }
}

function addressQrText(route) {
  const chain = CHAINS[route.chain];
  if (chain.kind === "evm" && chain.usdc) {
    // EIP-681: a USDC transfer to the receiver (plain "ethereum:addr" means ETH).
    return `ethereum:${chain.usdc}@${chain.chainId}/transfer?address=${route.address}`;
  }
  if (chain.kind === "solana" && chain.usdc && SOLANA_RECEIVER_KIND === "wallet") {
    // Solana Pay transfer request: wallets resolve the associated token account themselves.
    return `solana:${route.address}?spl-token=${chain.usdc}`;
  }
  return route.address;
}

/* ---------- card ---------- */

function renderCard(link) {
  const { routes, target, merchant, params } = link;
  let activeIndex = -1;
  let gasless = false;
  let inFlight = false;

  const tabs = () => `
    <div class="pay-routes" role="tablist">
      ${routes
      .map(
        (r, i) => `
        <button class="pay-route ${i === activeIndex ? "active" : ""}" type="button" data-index="${i}">
          <span class="pay-route-icon">${chainIcon(r.chain)}</span>
          <span class="pay-route-meta"><strong>Send via ${escapeHtml(CHAINS[r.chain].name)}</strong><span>Pay in USDC</span></span>
          <span class="pay-route-arrow">${i === activeIndex ? "✓" : "→"}</span>
        </button>`
      )
      .join("")}
    </div>`;

  function bindCommon() {
    laneCard.querySelectorAll(".pay-route").forEach((btn) =>
      btn.addEventListener("click", () => {
        activeIndex = Number(btn.dataset.index);
        draw();
      })
    );
    laneCard.querySelector(".embed-tag-btn")?.addEventListener("click", () => openEmbedOverlay(link));
    laneCard.querySelector("#modeToggle")?.addEventListener("change", (e) => {
      gasless = e.target.checked;
      draw();
    });
  }

  function draw() {
    if (activeIndex === -1) {
      laneCard.innerHTML = `${tabs()}<p class="status-hint">Select a payment network above to continue.</p>${poweredTag(true)}`;
      bindCommon();
      return;
    }

    const route = routes[activeIndex];
    const chain = CHAINS[route.chain];
    const gaslessOk = Boolean(target && merchant && canGasless(chain) && (chain.kind !== "starknet" || KEEPER_STARKNET_ADDRESS));

    if (!gasless || !gaslessOk) {
      laneCard.innerHTML = `
        ${tabs()}
        <div class="address-display-card">
          <div class="address-val" id="depositAddr">${escapeHtml(route.address)}</div>
          <button class="copy-btn" id="copyBtn" type="button">Copy Address</button>
        </div>
        ${gaslessOk
          ? `<div style="margin-top:.25rem;display:flex;justify-content:flex-end;">
                 <label style="font-size:.8rem;cursor:pointer;opacity:.8;display:inline-flex;align-items:center;gap:6px;">
                   <input type="checkbox" id="modeToggle" style="margin:0;cursor:pointer;"><span>Gasless Transfer</span>
                 </label></div>`
          : ""
        }
        <div class="qr-container"><div id="qrBox" style="width:240px;height:240px;margin:0 auto;"></div>
          <p style="font-size:.82rem;margin-top:.5rem;opacity:.8;">Scan and pay USDC on ${escapeHtml(chain.name)}</p></div>
        ${poweredTag(true)}`;
      bindCommon();
      laneCard.querySelector("#copyBtn")?.addEventListener("click", () => copyText(route.address, "Address copied to clipboard"));
      renderQr(laneCard.querySelector("#qrBox"), addressQrText(route));
      return;
    }

    laneCard.innerHTML = `
      ${tabs()}
      <div class="address-display-card">
        <label class="amount-label" style="display:block;text-align:left;font-size:.85rem;opacity:.85;">Amount (USDC)
          <input type="text" inputmode="decimal" id="amountInput" placeholder="0.00" autocomplete="off"
            style="display:block;width:100%;margin-top:4px;padding:8px 10px;border-radius:6px;border:1px solid rgba(255,255,255,.25);background:transparent;color:inherit;font-size:.95rem;box-sizing:border-box;" />
        </label>
        <button class="copy-btn" id="actionBtn" type="button" style="margin-top:.5rem;">Pay with Beanie</button>
        <p id="payHint" role="status" style="font-size:.8rem;opacity:.75;margin-top:0;"></p>
      </div>
      <div style="margin-top:.25rem;display:flex;justify-content:flex-end;">
        <label style="font-size:.8rem;cursor:pointer;opacity:.8;display:inline-flex;align-items:center;gap:6px;">
          <input type="checkbox" id="modeToggle" checked style="margin:0;cursor:pointer;"><span>Gasless Transfer</span>
        </label></div>
      <div class="qr-container"><div id="qrBox" style="width:240px;height:240px;margin:0 auto;"></div>
        <p style="font-size:.82rem;margin-top:.5rem;opacity:.8;">Scan to open this page in your wallet browser</p></div>
      ${poweredTag(true)}`;
    bindCommon();
    renderQr(laneCard.querySelector("#qrBox"), window.location.href);

    const hint = laneCard.querySelector("#payHint");
    const amountInput = laneCard.querySelector("#amountInput");
    const actionBtn = laneCard.querySelector("#actionBtn");

    amountInput?.addEventListener("input", (e) => {
      const input = e.target;
      const fromEnd = input.value.length - input.selectionStart;
      let v = input.value.replace(/[^\d.]/g, "");
      const dot = v.indexOf(".");
      if (dot !== -1) {
        const whole = v.slice(0, dot);
        const frac = v.slice(dot + 1).replace(/\./g, "").slice(0, USDC_DECIMALS);
        v = `${whole}.${frac}`;
      }
      if (v !== input.value) {
        input.value = v;
        const pos = Math.max(0, v.length - fromEnd);
        input.setSelectionRange(pos, pos);
      }
    });

    actionBtn?.addEventListener("click", async () => {
      if (inFlight) return;
      const amountRaw = parseDecimalToRawUnits(amountInput?.value ?? "", USDC_DECIMALS);
      if (!amountRaw) {
        hint.textContent = `Enter a valid amount (up to ${USDC_DECIMALS} decimal places).`;
        amountInput?.focus();
        return;
      }

      inFlight = true;
      actionBtn.disabled = true;
      try {
        hint.textContent = "Approve the transfer in your wallet…";
        const signed =
          chain.kind === "evm"
            ? await signEvmTransfer({ chainKey: chain.key, receiver: route.address, amountRaw })
            : chain.kind === "solana"
              ? await signSolanaTransfer({ receiver: route.address, amountRaw })
              : await signStarknetTransfer({ receiver: route.address, amountRaw });

        hint.textContent = "Relaying…";
        const ref = params.get("ref") || `beanie-${Date.now().toString(36)}`;
        const res = await api.submitPayment({
          chain: chain.key,
          merchantAddress: merchant,
          receiverAddress: signed.receiverAddress ?? route.address, // Solana: the receiver's token account
          destinationChain: target, // the ROUTE's settlement chain, not the source chain
          txRef: ref,
          fromAddress: signed.from,
          amountRaw,
          webhookUrl: null,
          signature: signed.payload,
        });
        notify("Payment authorized", "success", "Processing…");
        hint.textContent = res?.message ? `Queued: ${res.message}` : "Queued.";

        if (signed.settlement) {
          // Solana: the relay can silently miss the blockhash window, so wait for the outcome.
          hint.textContent = "Queued. Waiting for the network to confirm…";
          const outcome = await awaitSolanaSettlement(signed.settlement);
          if (outcome === "confirmed") {
            notify("Payment received", "success");
            hint.textContent = "Payment confirmed on Solana.";
          } else if (outcome === "expired") {
            notify("Payment expired", "error", "The authorization expired before it was processed. Nothing was sent. Please try again.");
            hint.textContent = "Expired before it was processed. Nothing was sent: tap Pay again.";
            actionBtn.disabled = false;
          } else {
            hint.textContent = "Still processing. Check your wallet or the merchant before paying again.";
          }
        }
      } catch (err) {
        const msg = describeError(err);
        notify("Payment failed", "error", msg);
        hint.textContent = `Error: ${msg}`;
        actionBtn.disabled = false;
      } finally {
        inFlight = false;
      }
    });
  }

  draw();
}

/* ---------- init ---------- */

shareBtn?.addEventListener("click", () => copyText(window.location.href, "Payment link copied to clipboard"));

(function init() {
  const link = parseLink();
  applyTheme(link.params);
  if (!link.routes.length) {
    laneCard.innerHTML = `<div class="error">This payment link is missing a destination address.</div>${poweredTag(false)}`;
    return;
  }
  try {
    renderCard(link);
  } catch (err) {
    console.error("Failed to render payment card:", err);
    laneCard.innerHTML = `<div class="error">Something went wrong loading this payment link. Please refresh.</div>${poweredTag(false)}`;
  }
})();
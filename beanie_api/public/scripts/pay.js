(() => {
  "use strict";

  const laneCard = document.querySelector("#laneCard");

  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      rpc: "https://mainnet.base.org",
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      rpc: "https://starknet-mainnet.public.blastapi.io/rpc/v0_7",
      // TODO: the token contract the ShieldInAnonymizer pool actually holds balances in.
      usdc: "",
    },
  };
  const STARKNET_BALANCEOF_SELECTOR = "0x2e4263afad30923c891518314c3c95dbe830a16874e8abc5777a9a20b54c76";
  const POLL_INTERVAL_MS = 15000;

  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" width="28" height="28" aria-hidden="true">
      <circle cx="21" cy="21" r="21" fill="#0052ff"/>
      <path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/>
    </svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" width="28" height="28" aria-hidden="true">
      <circle cx="21" cy="21" r="21" fill="#0c0c4d"/>
      <path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/>
    </svg>`,
  };

  function escapeHtml(value) {
    return String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function short(value) {
    return value && value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-8)}` : value || "";
  }

  /* ---------- routes from the query string ----------
   * ?lane=<id>&r=BASE:0xAA&r=STARKNET:0xBB — one pair per receiver the lane
   * actually got back from /api/v1/lanes/init. Falls back to the older
   * ?chain=&address= single-route form for any link built before this.
   */
  function routesFromQuery() {
    const params = new URLSearchParams(window.location.search);
    const routes = params.getAll("r")
      .map((pair) => {
        const [chain, address] = pair.split(":");
        return { chain: (chain || "").toUpperCase(), address: address || "" };
      })
      .filter((route) => CHAINS[route.chain] && route.address);
    if (routes.length) return routes;

    const chain = (params.get("chain") || "").toUpperCase();
    const address = params.get("address") || "";
    return CHAINS[chain] && address ? [{ chain, address }] : [];
  }

  /* ---------- public-RPC reads ---------- */
  async function rpcCall(url, method, params) {
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
    });
    const json = await res.json();
    if (json.error) throw new Error(json.error.message || `${method} failed`);
    return json.result;
  }

  async function contractExists(chainKey, address) {
    const chain = CHAINS[chainKey];
    try {
      if (chain.kind === "evm") {
        const code = await rpcCall(chain.rpc, "eth_getCode", [address, "latest"]);
        return typeof code === "string" && code !== "0x";
      }
      await rpcCall(chain.rpc, "starknet_getClassHashAt", ["latest", address]);
      return true;
    } catch { return false; }
  }

  async function tokenBalance(chainKey, address) {
    const chain = CHAINS[chainKey];
    if (!chain.usdc) return null;
    try {
      if (chain.kind === "evm") {
        const data = `0x70a08231${address.replace(/^0x/i, "").padStart(64, "0")}`;
        const result = await rpcCall(chain.rpc, "eth_call", [{ to: chain.usdc, data }, "latest"]);
        return BigInt(result || "0x0");
      }
      const result = await rpcCall(chain.rpc, "starknet_call", [
        { contract_address: chain.usdc, entry_point_selector: STARKNET_BALANCEOF_SELECTOR, calldata: [address] },
        "latest",
      ]);
      const low = BigInt(result?.[0] || "0x0");
      const high = BigInt(result?.[1] || "0x0");
      return (high << 128n) + low;
    } catch { return null; }
  }

  function renderQr(imgEl, value) {
    if (!imgEl || !window.QRCode?.toString) return;
    window.QRCode.toString(value, { type: "svg", errorCorrectionLevel: "H", margin: 1, width: 420 })
      .then((svg) => { imgEl.src = `data:image/svg+xml;charset=UTF-8,${encodeURIComponent(svg)}`; })
      .catch(() => imgEl.removeAttribute("src"));
  }

  async function copyAddress(address, feedbackEl) {
    try {
      await navigator.clipboard.writeText(address);
      return true;
    } catch {
      // Clipboard API unavailable (older browser / non-secure context) — select-and-copy fallback.
      try {
        const helper = document.createElement("textarea");
        helper.value = address;
        helper.style.position = "fixed";
        helper.style.opacity = "0";
        document.body.append(helper);
        helper.select();
        document.execCommand("copy");
        helper.remove();
        return true;
      } catch {
        if (feedbackEl) feedbackEl.textContent = "Couldn't copy — select the address manually.";
        return false;
      }
    }
  }

  function flashCopied(feedbackEl) {
    if (!feedbackEl) return;
    feedbackEl.textContent = "Address copied";
    feedbackEl.classList.add("visible");
    clearTimeout(feedbackEl._timer);
    feedbackEl._timer = setTimeout(() => feedbackEl.classList.remove("visible"), 2200);
  }

  /* ---------- card ---------- */
  let pollTimer = null;

  function renderCard(routes) {
    laneCard.innerHTML = `
      <div class="pay-routes" id="payRoutes" role="tablist" aria-label="Chain to pay from">
        ${routes.map((r, i) => `
          <button class="pay-route ${i === 0 ? "active" : ""}" type="button" data-chain="${r.chain}" data-address="${escapeHtml(r.address)}">
            <span class="pay-route-icon">${chainIcons[r.chain] || ""}</span>
            <span class="pay-route-meta">
              <strong>${escapeHtml(CHAINS[r.chain].name)}</strong>
              <span class="mono">${escapeHtml(short(r.address))}</span>
            </span>
            <span class="pay-route-arrow" aria-hidden="true">⧉</span>
          </button>
        `).join("")}
      </div>

      <div class="qr-wrap"><img id="payQr" alt="QR code for direct wallet transfer"></div>
      <p class="copy-feedback" id="copyFeedback"></p>
      <p class="hint" id="payHint">Checking chain status…</p>
    `;

    const routeButtons = laneCard.querySelectorAll(".pay-route");
    const feedbackEl = laneCard.querySelector("#copyFeedback");
    const qrEl = laneCard.querySelector("#payQr");
    const hintEl = laneCard.querySelector("#payHint");

    async function selectRoute(button) {
      routeButtons.forEach((btn) => btn.classList.toggle("active", btn === button));
      const chain = button.dataset.chain;
      const address = button.dataset.address;
      renderQr(qrEl, address);
      watchRoute(chain, address, hintEl);
      const copied = await copyAddress(address, feedbackEl);
      if (copied) flashCopied(feedbackEl);
    }

    routeButtons.forEach((button) => button.addEventListener("click", () => selectRoute(button)));

    // Prime the first route without requiring a click first.
    const first = routeButtons[0];
    renderQr(qrEl, first.dataset.address);
    watchRoute(first.dataset.chain, first.dataset.address, hintEl);
  }

  /* ---------- lightweight on-chain status for whichever route is active ---------- */
  function watchRoute(chainKey, address, hintEl) {
    if (pollTimer) clearInterval(pollTimer);
    let lastBalance = null;

    async function check() {
      const [exists, balance] = await Promise.all([
        contractExists(chainKey, address),
        tokenBalance(chainKey, address),
      ]);
      if (!exists) {
        hintEl.textContent = "Deployment confirming on chain…";
        return;
      }
      if (balance !== null && lastBalance !== null && balance > lastBalance) {
        hintEl.textContent = "Payment received. Thank you!";
        return;
      }
      lastBalance = balance;
      hintEl.textContent = "Ready to receive USDC.";
    }

    check();
    pollTimer = setInterval(check, POLL_INTERVAL_MS);
  }

  function showError(message) {
    laneCard.innerHTML = `<div class="error">${escapeHtml(message)}</div>`;
  }

  function init() {
    const routes = routesFromQuery();
    if (!routes.length) {
      showError("This payment link is missing a destination address.");
      return;
    }
    renderCard(routes);
  }

  init();
})();
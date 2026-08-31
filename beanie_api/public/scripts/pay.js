(() => {
  "use strict";

  const laneCard = document.querySelector("#laneCard");
  const shareBtn = document.querySelector("#payShareBtn");
  const toastStack = document.querySelector("#payToastStack");

  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      rpc: "https://base-mainnet.g.alchemy.com/v2/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      rpc: "https://starknet-mainnet.g.alchemy.com/starknet/version/rpc/v0_10/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x33068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
    },
  };

  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`,
  };

  function escapeHtml(value) {
    return String(value ?? "").replace(/[&<>"']/g, (c) => ({
      "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;",
    }[c]));
  }

  function notify(message) {
    if (!toastStack) return;
    const toast = document.createElement("div");
    toast.className = "live-toast";
    toast.textContent = message;
    toastStack.append(toast);
    setTimeout(() => toast.remove(), 3000);
  }

  async function resolveRoutes() {
    const params = new URLSearchParams(window.location.search);

    // Parse direct explicit receiver query params: ?r=BASE:0x...&r=STARKNET:0x...
    const directRoutes = params.getAll("r")
      .map((pair) => {
        const colonIndex = pair.indexOf(":");
        if (colonIndex === -1) return null;
        const chain = pair.slice(0, colonIndex).toUpperCase();
        const address = pair.slice(colonIndex + 1);
        return { chain, address };
      })
      .filter((route) => route && CHAINS[route.chain] && route.address);

    if (directRoutes.length) return directRoutes;

    // Single chain + address query fallback
    const singleChain = (params.get("chain") || "").toUpperCase();
    const singleAddress = params.get("address") || "";
    if (CHAINS[singleChain] && singleAddress) {
      return [{ chain: singleChain, address: singleAddress }];
    }

    // LocalStorage fallback
    const laneId = params.get("lane") || window.location.pathname.split("/").pop();
    if (laneId && laneId !== "pay") {
      try {
        const localLanes = JSON.parse(localStorage.getItem("beanie.lanes.v1") || "[]");
        const foundLane = localLanes.find((l) => l.id === laneId);
        if (foundLane && Array.isArray(foundLane.receivers)) {
          const routes = foundLane.receivers
            .map((r) => ({ chain: (r.chain || "").toUpperCase(), address: r.address || "" }))
            .filter((r) => CHAINS[r.chain] && r.address);
          if (routes.length) return routes;
        }
      } catch (e) { }

      // API endpoint fallback
      try {
        const res = await fetch(`/api/lanes/${laneId}`);
        if (res.ok) {
          const lane = await res.json();
          const receivers = lane.receivers || lane.lanes || [];
          if (Array.isArray(receivers)) {
            return receivers.map((r) => ({
              chain: (r.chain || "").toUpperCase(),
              address: r.address || "",
            })).filter((r) => CHAINS[r.chain] && r.address);
          }
        }
      } catch (e) { }
    }

    return [];
  }

  function renderCard(routes) {
    let activeIndex = 0;

    function buildHtml() {
      const currentRoute = routes[activeIndex];
      const chainName = CHAINS[currentRoute.chain]?.name || currentRoute.chain;

      laneCard.innerHTML = `
      <div class="pay-routes" role="tablist">
        ${routes.map((r, i) => `
          <button class="pay-route ${i === activeIndex ? "active" : ""}" type="button" data-index="${i}">
            <span class="pay-route-icon">${chainIcons[r.chain] || ""}</span>
            <span class="pay-route-meta">
              <strong>Send via ${escapeHtml(CHAINS[r.chain]?.name || r.chain)}</strong>
              <span>Pay in USDC</span>
            </span>
            <span class="pay-route-arrow">→</span>
          </button>
        `).join("")}
      </div>

      <div class="address-display-card">
        <div class="address-val" id="depositAddr">${escapeHtml(currentRoute.address)}</div>
        <button class="copy-btn" id="copyAddrBtn" type="button">Copy Address</button>
      </div>

      <p class="fee-notice">Beanie keeps 0.5% fee on every deposit</p>
      <p class="status-hint" id="payHint">Ready to receive USDC on ${escapeHtml(chainName)}</p>
    `;

      // Chain selection handler
      laneCard.querySelectorAll(".pay-route").forEach((btn) => {
        btn.addEventListener("click", () => {
          activeIndex = Number(btn.dataset.index);
          buildHtml();
        });
      });

      // Copy address handler
      const copyBtn = laneCard.querySelector("#copyAddrBtn");
      copyBtn?.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(currentRoute.address);
          notify("Address copied to clipboard");
        } catch {
          notify("Failed to copy address");
        }
      });
    }

    buildHtml();
  }

  shareBtn?.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(window.location.href);
      notify("Payment link copied to clipboard");
    } catch {
      notify("Failed to copy payment link");
    }
  });

  async function init() {
    const routes = await resolveRoutes();
    if (!routes.length) {
      laneCard.innerHTML = `<div class="error">This payment link is missing a destination address.</div>`;
      return;
    }
    renderCard(routes);
  }

  init();
})();
(() => {
  "use strict";

  const laneCard = document.querySelector("#laneCard");
  const shareBtn = document.querySelector("#payShareBtn");
  const toastStack = document.querySelector("#payToastStack");

  const RP_ID = "beanie.io";
  const AVNU_BUILD_TYPED_DATA_URL = "https://starknet.paymaster.avnu.fi/v1/build-typed-data";

  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      chainEnum: "Base",
      rpc: "https://base-mainnet.g.alchemy.com/v2/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      chainEnum: "Starknet",
      rpc: "https://starknet-mainnet.g.alchemy.com/starknet/version/rpc/v0_10/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x033068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
    },
  };

  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`,
  };

  function bytesToHex(bytes) {
    return Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
  }

  function bufferToBase64Url(buffer) {
    const bytes = new Uint8Array(buffer);
    let string = "";
    for (let i = 0; i < bytes.byteLength; i++) {
      string += String.fromCharCode(bytes[i]);
    }
    return btoa(string)
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "");
  }

  let cachedCredentialId = null;

  async function getOrRegisterCredential() {
    if (cachedCredentialId) return cachedCredentialId;

    try {
      const assertion = await navigator.credentials.get({
        publicKey: {
          challenge: crypto.getRandomValues(new Uint8Array(32)),
          rpId: RP_ID,
          userVerification: "required",
        },
      });
      cachedCredentialId = assertion.rawId;
      return cachedCredentialId;
    } catch {
      // Fallback registration
    }

    const credential = await navigator.credentials.create({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rp: { name: "Beanie", id: RP_ID },
        user: {
          id: crypto.getRandomValues(new Uint8Array(16)),
          name: "beanie-pay-user",
          displayName: "Beanie Pay User",
        },
        pubKeyCredParams: [{ type: "public-key", alg: -7 }],
        authenticatorSelection: { residentKey: "required", userVerification: "required" },
      },
    });

    cachedCredentialId = credential.rawId;
    return cachedCredentialId;
  }

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

    const singleChain = (params.get("chain") || "").toUpperCase();
    const singleAddress = params.get("address") || "";
    if (CHAINS[singleChain] && singleAddress) {
      return [{ chain: singleChain, address: singleAddress }];
    }

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

  // --- Base (EIP-3009 transferWithAuthorization) -----------------------------
  const USDC_DOMAIN_BASE = {
    name: "USD Coin",
    version: "2",
    chainId: 8453,
    verifyingContract: CHAINS.BASE.usdc,
  };

  async function prepareEvmSignedTransfer(receiverAddress, amountRaw) {
    if (!window.ethereum) throw new Error("Web3 wallet required for EVM.");
    const accounts = await window.ethereum.request({ method: "eth_requestAccounts" });
    const fromAddress = accounts[0];

    const nonce = "0x" + bytesToHex(crypto.getRandomValues(new Uint8Array(32)));
    const validAfter = 0;
    const validBefore = Math.floor(Date.now() / 1000) + 600; // 10 min window

    const message = { from: fromAddress, to: receiverAddress, value: amountRaw, validAfter, validBefore, nonce };

    const typedData = {
      types: {
        EIP712Domain: [
          { name: "name", type: "string" },
          { name: "version", type: "string" },
          { name: "chainId", type: "uint256" },
          { name: "verifyingContract", type: "address" },
        ],
        TransferWithAuthorization: [
          { name: "from", type: "address" },
          { name: "to", type: "address" },
          { name: "value", type: "uint256" },
          { name: "validAfter", type: "uint256" },
          { name: "validBefore", type: "uint256" },
          { name: "nonce", type: "bytes32" },
        ],
      },
      domain: USDC_DOMAIN_BASE,
      primaryType: "TransferWithAuthorization",
      message,
    };

    const signature = await window.ethereum.request({
      method: "eth_signTypedData_v4",
      params: [fromAddress, JSON.stringify(typedData)],
    });

    return { ...message, signature };
  }

  // --- Starknet: build typed data via OUR backend, not AVNU directly ---------
  async function prepareStarknetSignedCall(receiverAddress, amountRaw) {
    const starknetWindow = window.starknet || window.starknet_argentX || window.starknet_braavos;
    if (!starknetWindow) throw new Error("Starknet wallet extension not detected.");
    if (!starknetWindow.isConnected) await starknetWindow.enable();

    const amountUint256 = BigInt(amountRaw);
    const amountLow = (amountUint256 & BigInt("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF")).toString();
    const amountHigh = (amountUint256 >> BigInt(128)).toString();
    const userAccountAddress = starknetWindow.selectedAddress;

    const calls = [{
      contractAddress: CHAINS.STARKNET.usdc,
      entrypoint: "transfer",
      calldata: [receiverAddress, amountLow, amountHigh],
    }];

    // Ask OUR server to build the typed data. The AVNU API key never touches the browser.
    const typedDataRes = await fetch("/api/v1/paymaster/build-typed-data", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ userAddress: userAccountAddress, calls }),
    });
    if (!typedDataRes.ok) throw new Error("Failed to build paymaster typed data");
    const typedData = await typedDataRes.json();

    const signature = await starknetWindow.account.signMessage(typedData);
    const formattedSignature = Array.isArray(signature) ? signature : [signature.r, signature.s];

    return { typedData, signature: formattedSignature, userAddress: userAccountAddress };
  }

  function renderCard(routes) {
    let activeIndex = -1;
    let isGaslessMode = false;

    function buildHtml() {
      if (activeIndex === -1) {
        laneCard.innerHTML = `
        <div class="pay-routes" role="tablist">
          ${routes.map((r, i) => `
            <button class="pay-route" type="button" data-index="${i}">
              <span class="pay-route-icon">${chainIcons[r.chain] || ""}</span>
              <span class="pay-route-meta">
                <strong>Send via ${escapeHtml(CHAINS[r.chain]?.name || r.chain)}</strong>
                <span>Pay in USDC</span>
              </span>
              <span class="pay-route-arrow">→</span>
            </button>
          `).join("")}
        </div>
        <p class="status-hint">Please select a payment network above to continue.</p>
      `;

        laneCard.querySelectorAll(".pay-route").forEach((btn) => {
          btn.addEventListener("click", () => {
            activeIndex = Number(btn.dataset.index);
            buildHtml();
          });
        });
        return;
      }

      const currentRoute = routes[activeIndex];
      const chainConfig = CHAINS[currentRoute.chain];
      const chainName = chainConfig?.name || currentRoute.chain;

      let qrContent = "";
      let qrLabel = "";

      if (!isGaslessMode) {
        const scheme = chainConfig.kind === "evm" ? "ethereum" : "starknet";
        qrContent = `${scheme}:${currentRoute.address}`;
        qrLabel = `Scan to Pay on ${escapeHtml(chainName)}`;
      } else {
        const params = new URLSearchParams(window.location.search);
        const txHash = params.get("tx") || "0x0";
        const merchant = params.get("merchant") || currentRoute.address;
        const amount = params.get("amount") || "1000000";

        const passkeyPayUrl = new URL(window.location.origin + "/pay/passkey");
        passkeyPayUrl.searchParams.set("chain", chainConfig.chainEnum);
        passkeyPayUrl.searchParams.set("receiver", currentRoute.address);
        passkeyPayUrl.searchParams.set("merchant", merchant);
        passkeyPayUrl.searchParams.set("tx", txHash);
        passkeyPayUrl.searchParams.set("amount", amount);

        qrContent = passkeyPayUrl.toString();
        qrLabel = "Scan to Authorize Payment";
      }

      const qrApiUrl = `https://api.qrserver.com/v1/create-qr-code/?size=180x180&data=${encodeURIComponent(qrContent)}`;

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
        <button class="copy-btn" id="actionBtn" type="button">
          ${isGaslessMode ? "Pay with Beanie" : "Copy Address"}
        </button>
      </div>

      <div style="margin-top: 0.5rem; display: flex; justify-content: flex-end;">
        <label style="font-size: 0.8rem; cursor: pointer; opacity: 0.8; display: inline-flex; align-items: center; gap: 6px;">
          <input type="checkbox" id="modeToggle" ${isGaslessMode ? "checked" : ""} style="margin: 0; cursor: pointer;">
          <span>Gasless Transfer</span>
        </label>
      </div>

      <div class="qr-container" style="margin-top: 1.25rem; text-align: center;">
        <img src="${qrApiUrl}" alt="Scan to Pay QR Code" class="qr-code-img" width="240" height="240" />
        <p style="font-size: 0.85rem; margin-top: 0.75rem; opacity: 0.8;">${qrLabel}</p>
      </div>
    `;

      laneCard.querySelectorAll(".pay-route").forEach((btn) => {
        btn.addEventListener("click", () => {
          activeIndex = Number(btn.dataset.index);
          buildHtml();
        });
      });

      laneCard.querySelector("#modeToggle")?.addEventListener("change", (e) => {
        isGaslessMode = e.target.checked;
        buildHtml();
      });

      laneCard.querySelector("#actionBtn")?.addEventListener("click", async () => {
        if (!isGaslessMode) {
          try {
            await navigator.clipboard.writeText(currentRoute.address);
            notify("Address copied to clipboard");
          } catch {
            notify("Failed to copy address");
          }
        } else {
          const hint = laneCard.querySelector("#payHint");
          const params = new URLSearchParams(window.location.search);
          let txHash = params.get("tx") || "0x0000000000000000000000000000000000000000000000000000000000000000";
          const merchantAddr = params.get("merchant") || currentRoute.address;
          const amountRaw = params.get("amount") || "1000000";

          try {
            let signatureHex = "";
            let senderAddress = merchantAddr;

            let signaturePayload;
            if (chainConfig.kind === "evm") {
              if (hint) hint.textContent = "Sign the transfer authorization in your wallet...";
              const auth = await prepareEvmSignedTransfer(currentRoute.address, amountRaw);
              senderAddress = auth.from;
              signaturePayload = { kind: "evm", ...auth };
            } else if (chainConfig.kind === "starknet") {
              if (hint) hint.textContent = "Sign the sponsored transfer in your wallet...";
              const sn = await prepareStarknetSignedCall(currentRoute.address, amountRaw);
              senderAddress = sn.userAddress;
              signaturePayload = { kind: "starknet", typedData: sn.typedData, signature: sn.signature };
            }

            if (hint) hint.textContent = "Relaying request...";
            const payload = {
              chain: chainConfig.chainEnum,
              merchant_address: merchantAddr,
              receiver_address: currentRoute.address,
              destination_chain: chainConfig.chainEnum,
              tx_hash: txHash,
              from_address: senderAddress,
              amount_raw: amountRaw,
              webhook_url: null,
              signature: JSON.stringify(signaturePayload),
            };

            const res = await fetch("/api/v1/pay", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify(payload),
            });

            if (!res.ok) {
              const errData = await res.json().catch(() => ({}));
              throw new Error(errData.message || `API rejected request: ${res.status}`);
            }

            const data = await res.json();
            notify("Pay with Beanie Authorized Successfully✅");
            if (hint) hint.textContent = `Queued: ${data.message}`;
          } catch (err) {
            notify(`Payment execution failed: ${err.message}`);
            if (hint) hint.textContent = `Error: ${err.message}`;
          }
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
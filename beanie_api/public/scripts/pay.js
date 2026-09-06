(() => {
  "use strict";

  const laneCard = document.querySelector("#laneCard");
  const shareBtn = document.querySelector("#payShareBtn");
  const toastStack = document.querySelector("#payToastStack");

  const BEANIE_KEEPER_STARKNET_ADDRESS = "0x01d4a73b58909eb341e6357bd085fea917d71c386ebaecd770792f7b5a34615a"; // Beanie's own relayer account — set at build time

  const USDC_DECIMALS = 6;

  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      chainEnum: "BASE",
      chainId: 8453,
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      chainEnum: "STARKNET",
      usdc: "0x033068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
    },
  };

  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" aria-hidden="true"><circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`,
  };

  function bytesToHex(bytes) {
    return Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
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

  // Converts a user-typed decimal string (e.g. "12.5") into raw base units
  // (e.g. "12500000" for 6-decimal USDC) WITHOUT going through floating point,
  // so values like 0.1 or 19.99 can't get mangled by binary float rounding.
  // Returns null if the string isn't a valid, non-negative, non-zero amount
  // with at most `decimals` fractional digits.
  function parseDecimalToRawUnits(input, decimals) {
    if (typeof input !== "string") return null;
    const trimmed = input.trim();
    if (!/^\d+(\.\d+)?$/.test(trimmed)) return null;

    const [wholePart, fractionalPart = ""] = trimmed.split(".");
    if (fractionalPart.length > decimals) return null; // too many decimal places

    const paddedFraction = fractionalPart.padEnd(decimals, "0");
    const rawString = `${wholePart}${paddedFraction}`.replace(/^0+(?=\d)/, "");

    let rawValue;
    try {
      rawValue = BigInt(rawString || "0");
    } catch {
      return null;
    }

    if (rawValue <= 0n) return null;
    return rawValue.toString();
  }

  async function resolveRoutes() {
    const params = new URLSearchParams(window.location.search);

    const directRoutes = params.getAll("r")
      .map((pair) => {
        const colonIndex = pair.indexOf(":");
        if (colonIndex === -1) return null;
        return { chain: pair.slice(0, colonIndex).toUpperCase(), address: pair.slice(colonIndex + 1) };
      })
      .filter((route) => route && CHAINS[route.chain] && route.address);
    if (directRoutes.length) return directRoutes;

    const singleChain = (params.get("chain") || "").toUpperCase();
    const singleAddress = params.get("address") || "";
    if (CHAINS[singleChain] && singleAddress) return [{ chain: singleChain, address: singleAddress }];

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
      } catch {
        // no local record on this browser — expected for anyone but the merchant
        // who created the lane; not an error, just fall through.
      }
    }

    return [];
  }

  // --- Base: sign an EIP-3009 transferWithAuthorization ----------------------
  function usdcDomainBase() {
    return { name: "USD Coin", version: "2", chainId: CHAINS.BASE.chainId, verifyingContract: CHAINS.BASE.usdc };
  }

  async function ensureBaseChain() {
    const BASE_HEX = "0x2105"; // 8453
    try {
      await window.ethereum.request({
        method: "wallet_switchEthereumChain",
        params: [{ chainId: BASE_HEX }],
      });
    } catch (err) {
      // 4902 = chain not added to the wallet yet
      if (err.code === 4902) {
        await window.ethereum.request({
          method: "wallet_addEthereumChain",
          params: [{
            chainId: BASE_HEX,
            chainName: "Base",
            nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
            rpcUrls: ["https://base.org"],
            blockExplorerUrls: ["https://basescan.org"],
          }],
        });
      } else {
        throw err;
      }
    }
  }

  async function prepareEvmSignedTransfer(receiverAddress, amountRaw) {
    if (!window.ethereum) throw new Error("No EVM wallet detected.");
    const [fromAddress] = await window.ethereum.request({ method: "eth_requestAccounts" });
    await ensureBaseChain();

    const nonce = "0x" + bytesToHex(crypto.getRandomValues(new Uint8Array(32)));
    const validAfter = 0;
    const validBefore = Math.floor(Date.now() / 1000) + 600; // 10 minutes

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
      domain: usdcDomainBase(),
      primaryType: "TransferWithAuthorization",
      message,
    };

    const signature = await window.ethereum.request({
      method: "eth_signTypedData_v4",
      params: [fromAddress, JSON.stringify(typedData)],
    });

    return { kind: "evm", ...message, signature };
  }

  // --- Starknet Type Definitions and Utility Helpers -------------------------
  const typesRev0 = {
    StarkNetDomain: [
      { name: "name", type: "felt" },
      { name: "version", type: "felt" },
      { name: "chainId", type: "felt" },
    ],
    OutsideExecution: [
      { name: "caller", type: "felt" },
      { name: "nonce", type: "felt" },
      { name: "execute_after", type: "felt" },
      { name: "execute_before", type: "felt" },
      { name: "calls_len", type: "felt" },
      { name: "calls", type: "OutsideCall*" },
    ],
    OutsideCall: [
      { name: "to", type: "felt" },
      { name: "selector", type: "felt" },
      { name: "calldata_len", type: "felt" },
      { name: "calldata", type: "felt*" },
    ],
  };

  const typesRev1 = {
    StarknetDomain: [
      { name: "name", type: "shortstring" },
      { name: "version", type: "shortstring" },
      { name: "chainId", type: "shortstring" },
      { name: "revision", type: "shortstring" },
    ],
    OutsideExecution: [
      { name: "Caller", type: "ContractAddress" },
      { name: "Nonce", type: "felt" },
      { name: "Execute After", type: "u128" },
      { name: "Execute Before", type: "u128" },
      { name: "Calls", type: "Call*" },
    ],
    Call: [
      { name: "To", type: "ContractAddress" },
      { name: "Selector", type: "selector" },
      { name: "Calldata", type: "felt*" },
    ],
  };

  function getDomain(chainId, version) {
    if (version === "2") {
      // WARNING! Version and revision are encoded as numbers in the StarkNetDomain type 
      // and not as shortstring due to a legacy bug kept for compatibility.
      return {
        name: "Account.execute_from_outside",
        version: "2",
        chainId: chainId,
        revision: "1",
      };
    }
    return {
      name: "Account.execute_from_outside",
      version: "1",
      chainId: chainId,
    };
  }

  function getOutsideCall(call, hashModule) {
    return {
      to: call.contractAddress,
      selector: hashModule.getSelectorFromName(call.entrypoint),
      calldata: call.calldata ?? [],
    };
  }

  function getTypedData(outsideExecution, chainId, version) {
    if (version === "2") {
      return {
        types: typesRev1,
        primaryType: "OutsideExecution",
        domain: getDomain(chainId, version),
        message: {
          // MUST match the capitalized casing defined in typesRev1
          Caller: outsideExecution.caller,
          Nonce: outsideExecution.nonce,
          "Execute After": outsideExecution.execute_after,
          "Execute Before": outsideExecution.execute_before,
          Calls: outsideExecution.calls.map((call) => ({
            To: call.to,
            Selector: call.selector,
            Calldata: call.calldata,
          })),
        },
      };
    }

    return {
      types: typesRev0,
      primaryType: "OutsideExecution",
      domain: getDomain(chainId, version),
      message: {
        ...outsideExecution,
        calls_len: outsideExecution.calls.length,
        calls: outsideExecution.calls.map((call) => ({
          ...call,
          calldata_len: call.calldata.length,
          calldata: call.calldata,
        })),
      },
    };
  }

  // --- Starknet: sign sponsored external call -------------------------
  // Official SNIP-9 interface IDs:
  const SNIP9_V1_INTERFACE_ID = "0x68cfd18b92d1907b8ba3cc324900277f5a3622099431ea85dd8089255e4181";
  const SNIP9_V2_INTERFACE_ID = "0x1d1144bb2138366ff28d8e9ab57456b1d332ac42196230c3a602003c89872";
  function randomNonceHex() {
    // 31 bytes → max 2^248 - 1, safely inside the felt range [0, 2^251)
    const nonceBytes = crypto.getRandomValues(new Uint8Array(31));
    return (
      "0x" +
      Array.from(nonceBytes)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("")
    );
  }

  // Prefer the wallet's own native context provider to bypass browser localhost CORS restrictions.
  async function starknetCallContract(starknetWindow, call) {
    // 1. Check if the active connected account instance can handle the call directly
    if (starknetWindow.account && typeof starknetWindow.account.callContract === "function") {
      return starknetWindow.account.callContract(call);
    }
    // 2. Fall back to the unified base provider instance provided by the extension window
    if (starknetWindow.provider && typeof starknetWindow.provider.callContract === "function") {
      return starknetWindow.provider.callContract(call);
    }
    throw new Error("No valid wallet call provider interface found.");
  }

  async function detectSnip9Version(address, starknetWindow) {
    // Look for V2 version implementation first
    try {
      const v2 = await starknetCallContract(starknetWindow, {
        contractAddress: address,
        entrypoint: "supports_interface",
        calldata: [SNIP9_V2_INTERFACE_ID],
      });
      if (v2 && v2.result && BigInt(v2.result[0]) !== 0n) return "2";
      if (Array.isArray(v2) && BigInt(v2[0]) !== 0n) return "2";
    } catch (e) {
      console.warn("[beanie] Wallet skipped V2 detection checkout:", e.message || e);
    }

    // Look for V1 version implementation fallback
    try {
      const v1 = await starknetCallContract(starknetWindow, {
        contractAddress: address,
        entrypoint: "supports_interface",
        calldata: [SNIP9_V1_INTERFACE_ID],
      });
      if (v1 && v1.result && BigInt(v1.result[0]) !== 0n) return "1";
      if (Array.isArray(v1) && BigInt(v1[0]) !== 0n) return "1";
    } catch (e) {
      console.warn("[beanie] Wallet skipped V1 detection checkout:", e.message || e);
    }

    // Default to version 2 (Standard Argent/Braavos current spec) if checks are blocked
    return "2";
  }

  async function prepareStarknetSignedCall(receiverAddress, amountRaw) {
    const {
      CallData,
      validateAndParseAddress,
      cairo,
      hash, // getSelectorFromName lives here in starknet.js
    } = await import("/scripts/starknet.bundle.js");
    const starknetWindow =
      window.starknet || window.starknet_argentX || window.starknet_braavos;
    if (!starknetWindow) throw new Error("No Starknet wallet detected.");
    if (!starknetWindow.isConnected) await starknetWindow.enable();
    const rawUserAddress =
      starknetWindow.selectedAddress ||
      starknetWindow.account?.address;
    if (!rawUserAddress) throw new Error("Could not resolve Starknet account address.");
    if (!BEANIE_KEEPER_STARKNET_ADDRESS || BEANIE_KEEPER_STARKNET_ADDRESS.startsWith("0x0....")) {
      throw new Error("BEANIE_KEEPER_STARKNET_ADDRESS is not configured");
    }
    const userAddress = validateAndParseAddress(rawUserAddress);
    const receiver = validateAndParseAddress(receiverAddress);
    const usdc = validateAndParseAddress(CHAINS.STARKNET.usdc);
    const caller = validateAndParseAddress(BEANIE_KEEPER_STARKNET_ADDRESS);
    const nowSec = Math.floor(Date.now() / 1000);
    const nonceHex = randomNonceHex();
    let chainId = "0x534e5f4d41494e"; // SN_MAIN
    try {
      if (typeof starknetWindow.account?.getChainId === "function") {
        chainId = await starknetWindow.account.getChainId();
      } else if (typeof starknetWindow.provider?.getChainId === "function") {
        chainId = await starknetWindow.provider.getChainId();
      } else if (starknetWindow.chainId) {
        chainId = starknetWindow.chainId;
      }
    } catch { /* keep mainnet default */ }
    const callTarget =
      starknetWindow.account?.provider ||
      starknetWindow.provider ||
      starknetWindow.account;
    let version = "2";
    try {
      // Pass the root window container instead of the nested raw provider to unlock context calls
      const detected = await detectSnip9Version(userAddress, starknetWindow);
      if (detected === "1" || detected === "2") version = detected;
    } catch (err) {
      console.log("[beanie] Defaulting to Version 2 execution envelope strategy.");
    }

    // Compile calls natively into raw standard arrays using CallData.compile
    const rawCompiledCalldata = CallData.compile({
      recipient: receiver,
      amount: cairo.uint256(amountRaw),
    });
    const calls = [
      {
        contractAddress: usdc,
        entrypoint: "transfer",
        calldata: rawCompiledCalldata,
      },
    ];
    // Standard structural mapping from baseline calls
    const outsideCalls = calls.map((c) => getOutsideCall(c, hash));
    const outsideExecution = {
      caller,
      nonce: nonceHex,
      execute_after: nowSec - 60,
      execute_before: nowSec + 600,
      calls: outsideCalls,
    };
    // Construct the typedData structurally matching the contract standard requirements
    const typedData = getTypedData(outsideExecution, chainId, version);
    console.log("[beanie] aligned standard typedData", JSON.stringify(typedData, null, 2));
    if (typeof starknetWindow.account?.signMessage !== "function") {
      throw new Error("Wallet does not support signMessage (required for SNIP-9).");
    }
    const signature = await starknetWindow.account.signMessage(typedData, caller);
    const formattedSignature = Array.isArray(signature)
      ? signature
      : [signature.r, signature.s];
    return {
      kind: "starknet",
      outsideExecution,
      signature: formattedSignature,
      userAddress,
      version,
      entrypoint: version === "2" ? "execute_from_outside_v2" : "execute_from_outside"
    };
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
        <p class="status-hint">Select a payment network above to continue.</p>  
        `;
        laneCard.querySelectorAll(".pay-route").forEach((btn) => {
          btn.addEventListener("click", () => { activeIndex = Number(btn.dataset.index); buildHtml(); });
        });
        return;
      }

      const currentRoute = routes[activeIndex];
      const chainConfig = CHAINS[currentRoute.chain];
      const chainName = chainConfig?.name || currentRoute.chain;

      const routeTabs = `
        <div class="pay-routes" role="tablist">
          ${routes.map((r, i) => `
            <button class="pay-route ${i === activeIndex ? "active" : ""}" type="button" data-index="${i}">
              <span class="pay-route-icon">${chainIcons[r.chain] || ""}</span>
              <span class="pay-route-meta">
                <strong>Send via ${escapeHtml(CHAINS[r.chain]?.name || r.chain)}</strong>
                <span>Pay in USDC</span>
              </span>
              <span class="pay-route-arrow">📎</span>
            </button>
          `).join("")}
        </div>
      `;

      if (!isGaslessMode) {
        const scheme = chainConfig.kind === "evm" ? "ethereum" : "starknet";
        const qrContent = `${scheme}:${currentRoute.address}`;
        const qrApiUrl = `https://api.qrserver.com/v1/create-qr-code/?size=240x240&data=${encodeURIComponent(qrContent)}`;

        laneCard.innerHTML = `
    ${routeTabs}
    <div class="address-display-card">
      <div class="address-val" id="depositAddr">${escapeHtml(currentRoute.address)}</div>
      <button class="copy-btn" id="copyBtn" type="button">Copy Address</button>
    </div>
    <div style="margin-top: 0.5rem; display: flex; justify-content: flex-end;">
      <label style="font-size: 0.8rem; cursor: pointer; opacity: 0.8; display: inline-flex; align-items: center; gap: 6px;">
        <input type="checkbox" id="modeToggle" style="margin: 0; cursor: pointer;">
        <span>Gasless Transfer</span>
      </label>
    </div>
    <div class="qr-container" style="margin-top: 1.25rem; text-align: center;">
      <img src="${qrApiUrl}" alt="Scan to Pay QR Code" class="qr-code-img" width="240" height="240" />
      <p style="font-size: 0.85rem; margin-top: 0.75rem; opacity: 0.8;">Scan and Pay on ${escapeHtml(chainName)}</p>
    </div>
  `;

        laneCard.querySelector("#copyBtn")?.addEventListener("click", async () => {
          try {
            await navigator.clipboard.writeText(currentRoute.address);
            notify("Address copied to clipboard");
          } catch {
            notify("Failed to copy address");
          }
        });
      } else {
        // Gasless – QR opens this exact page inside the wallet browser
        const pageUrl = window.location.href;
        const qrApiUrl = `https://api.qrserver.com/v1/create-qr-code/?size=240x240&data=${encodeURIComponent(pageUrl)}`;

        laneCard.innerHTML = `
    ${routeTabs}
    <div class="address-display-card">
      <label class="amount-label" style="display:block; text-align:left; font-size: 0.85rem; opacity: 0.85;">
        Amount (USDC)
        <input
          type="text"
          inputmode="decimal"
          id="amountInput"
          placeholder="0.00"
          autocomplete="off"
          style="display:block; width:100%; margin-top:4px; padding:10px; border-radius:6px; border:1px solid rgba(255,255,255,0.25); background:transparent; color:inherit; font-size:1rem; box-sizing:border-box;"
        />
      </label>
      <button class="copy-btn" id="actionBtn" type="button" style="margin-top: 0.85rem;">Pay with Beanie</button>
      <p id="payHint" style="font-size:0.8rem; opacity:0.75; margin-top:0; min-height:0;"></p>
    </div>
    <div style="margin-top: 0.5rem; display: flex; justify-content: flex-end;">
      <label style="font-size: 0.8rem; cursor: pointer; opacity: 0.8; display: inline-flex; align-items: center; gap: 6px;">
        <input type="checkbox" id="modeToggle" checked style="margin: 0; cursor: pointer;">
        <span>Gasless Transfer</span>
      </label>
    </div>
    <div class="qr-container" style="margin-top: 1.25rem; text-align: center;">
      <img src="${qrApiUrl}" alt="Open in wallet browser" class="qr-code-img" width="240" height="240" />
      <p style="font-size: 0.85rem; margin-top: 0.75rem; opacity: 0.8;">
        Scan and Authorize Transfer
      </p>
    </div>
  `;

        laneCard.querySelector("#amountInput")?.addEventListener("input", (e) => {
          const input = e.target;
          const cursorFromEnd = input.value.length - input.selectionStart;

          // Strip anything that isn't a digit or a dot
          let cleaned = input.value.replace(/[^\d.]/g, "");

          // Collapse to at most one decimal point — keep the first, drop the rest
          const firstDot = cleaned.indexOf(".");
          if (firstDot !== -1) {
            cleaned = cleaned.slice(0, firstDot + 1) + cleaned.slice(firstDot + 1).replace(/\./g, "");
          }

          // Cap fractional digits to USDC_DECIMALS as they type
          if (firstDot !== -1) {
            const whole = cleaned.slice(0, firstDot);
            const frac = cleaned.slice(firstDot + 1, firstDot + 1 + USDC_DECIMALS);
            cleaned = frac.length ? `${whole}.${frac}` : `${whole}.`;
          }

          if (cleaned !== input.value) {
            input.value = cleaned;
            // Restore cursor position relative to the end, since we may have removed characters
            const pos = Math.max(0, cleaned.length - cursorFromEnd);
            input.setSelectionRange(pos, pos);
          }
        });

        laneCard.querySelector("#actionBtn")?.addEventListener("click", async () => {
          const hint = laneCard.querySelector("#payHint");
          const amountInput = laneCard.querySelector("#amountInput");
          const params = new URLSearchParams(window.location.search);
          const txHash = params.get("tx") || "0x0";
          const merchantAddr = params.get("merchant") || currentRoute.address;

          const amountRaw = parseDecimalToRawUnits(amountInput?.value ?? "", USDC_DECIMALS);
          if (!amountRaw) {
            if (hint) hint.textContent = `Enter a valid amount (up to ${USDC_DECIMALS} decimal places).`;
            amountInput?.focus();
            return;
          }

          try {
            let signaturePayload;
            let senderAddress;

            if (chainConfig.kind === "evm") {
              if (hint) hint.textContent = "Sign the transfer authorization in your wallet...";
              signaturePayload = await prepareEvmSignedTransfer(currentRoute.address, amountRaw);
              senderAddress = signaturePayload.from;
            } else {
              if (hint) hint.textContent = "Sign the sponsored transfer in your wallet...";
              signaturePayload = await prepareStarknetSignedCall(currentRoute.address, amountRaw);
              senderAddress = signaturePayload.userAddress;
            }

            if (hint) hint.textContent = "Relaying request...";
            const res = await fetch("/api/v1/pay", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({
                chain: chainConfig.chainEnum,
                merchant_address: merchantAddr,
                receiver_address: currentRoute.address,
                destination_chain: chainConfig.chainEnum,
                tx_hash: txHash,
                from_address: senderAddress,
                amount_raw: amountRaw,
                webhook_url: null,
                signature: JSON.stringify(signaturePayload),
              }),
            });

            if (!res.ok) {
              const errData = await res.json().catch(() => ({}));
              throw new Error(errData.message || `API rejected request: ${res.status}`);
            }

            const data = await res.json();
            notify("Payment authorized — processing");
            if (hint) hint.textContent = `Queued: ${data.message}`;
          } catch (err) {
            notify(`Payment failed: ${err.message}`);
            if (hint) hint.textContent = `Error: ${err.message}`;
          }
        });
      }

      laneCard.querySelectorAll(".pay-route").forEach((btn) => {
        btn.addEventListener("click", () => { activeIndex = Number(btn.dataset.index); buildHtml(); });
      });
      laneCard.querySelector("#modeToggle")?.addEventListener("change", (e) => {
        isGaslessMode = e.target.checked;
        buildHtml();
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
    try {
      const routes = await resolveRoutes();
      if (!routes.length) {
        laneCard.innerHTML = `<div class="error">This payment link is missing a destination address.</div>`;
        return;
      }
      renderCard(routes);
    } catch (err) {
      console.error("Failed to load payment routes:", err);
      laneCard.innerHTML = `<div class="error">Something went wrong loading this payment link. Please refresh.</div>`;
    }
  }

  init();
})();
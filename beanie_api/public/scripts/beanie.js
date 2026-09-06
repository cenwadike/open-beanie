(() => {
  "use strict";

  /* ---------- Config ---------- */
  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      chainId: 8453,
      rpc: "https://base-mainnet.g.alchemy.com/v2/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
      factory: "0x0000000000000000000000000000000000000000",
      explorerAddress: "https://basescan.org/address/",
      litCosigner: "0x0000000000000000000000000000000000000000",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      rpc: "https://starknet-mainnet.public.blastapi.io",
      usdc: "0x33068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
      factory: "0x0000000000000000000000000000000000000000",
      explorerAddress: "https://starkscan.co/contract/",
      stealthClassHash: "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      litCosigner: "0x0456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01",
    },
  };

  const SOURCE_CHAINS = ["BASE", "STARKNET"];
  const OPTION_TO_CHAIN = { base: "BASE", starknet: "STARKNET" };
  const POLL_INTERVAL_MS = 20000;
  const API_CREATE = "/api/v1/create";
  const RP_ID = window.location.hostname;

  const STARKNET_BALANCEOF_SELECTOR =
    "0x2e4263afad30923c891518314c3c95dbe830a16874e8abc5777a9a20b54c76";
  const STARKNET_PREDICT_SELECTOR =
    "0x0000000000000000000000000000000000000000000000000000000000000000";
  const EVM_PREDICT_SELECTOR = "0x0c40efef";

  const STORAGE_LANES = "beanie.lanes.v1";
  const STORAGE_HISTORY = "beanie.history.v1";
  const STORAGE_BALANCES = "beanie.balances.v1";
  const STORAGE_SEEN = "beanie.history.seen.v1";
  const STORAGE_CRED = "beanie.passkey.cred.v1";

  let activeLane = null;
  let activeReceiver = null;

  /* ---------- DOM / storage helpers ---------- */
  const $ = (sel) => document.querySelector(sel);
  const el = (tag, cls, html) => {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (html !== undefined) node.innerHTML = html;
    return node;
  };
  const escapeHtml = (v) =>
    String(v ?? "").replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" }[c])
    );
  const short = (v) => (v && v.length > 16 ? `${v.slice(0, 8)}…${v.slice(-6)}` : v || "");

  const readJson = (key, fallback) => {
    try {
      const raw = localStorage.getItem(key);
      return raw ? JSON.parse(raw) : fallback;
    } catch {
      return fallback;
    }
  };
  const writeJson = (key, value) => {
    try {
      localStorage.setItem(key, JSON.stringify(value));
    } catch { }
  };
  const getLanes = () => readJson(STORAGE_LANES, []);
  const saveLanes = (lanes) => writeJson(STORAGE_LANES, lanes);
  const getHistory = () => readJson(STORAGE_HISTORY, {});
  const saveHistory = (history) => writeJson(STORAGE_HISTORY, history);
  const getBalances = () => readJson(STORAGE_BALANCES, {});
  const saveBalances = (balances) => writeJson(STORAGE_BALANCES, balances);
  const getSeenAt = () => Number(localStorage.getItem(STORAGE_SEEN) || 0);
  const setSeenAt = (ts) => {
    try {
      localStorage.setItem(STORAGE_SEEN, String(ts));
    } catch { }
  };
  const receiverKey = (chain, address) => `${chain}:${address}`.toLowerCase();

  function bytesToHex(bytes) {
    return Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
  }

  function bufferToBase64Url(buffer) {
    const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
    let s = "";
    for (let i = 0; i < bytes.byteLength; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function base64UrlToBuffer(base64url) {
    const base64 = base64url.replace(/-/g, "+").replace(/_/g, "/");
    const padded = base64.padEnd(base64.length + ((4 - (base64.length % 4)) % 4), "=");
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
  }

  async function sha256Utf8(str) {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(str));
    return new Uint8Array(digest);
  }

  function privacyOptedIn() {
    const box = $("#privacyToggle");
    return Boolean(box?.checked);
  }

  function restrictChainSelect() {
    // Helper placeholder if needed for UI constraints
  }

  function refreshNotifyDot() {
    // Refresh indicator if unread deposits exist
  }

  function revealShareRoute() {
    $("#shareBtn")?.classList.remove("is-hidden");
  }

  /* ---------- Passkeys ---------- */
  let cachedRawId = null;

  async function getOrRegisterCredential({ requirePrf = false } = {}) {
    if (cachedRawId) return cachedRawId;

    const stored = localStorage.getItem(STORAGE_CRED);
    if (stored) {
      try {
        const assertion = await navigator.credentials.get({
          publicKey: {
            challenge: crypto.getRandomValues(new Uint8Array(32)),
            rpId: RP_ID,
            allowCredentials: [{ id: base64UrlToBuffer(stored), type: "public-key" }],
            userVerification: "required",
            extensions: requirePrf ? { prf: {} } : undefined,
          },
        });
        cachedRawId = assertion.rawId;
        return cachedRawId;
      } catch {
        /* fall through to create */
      }
    }

    try {
      const assertion = await navigator.credentials.get({
        publicKey: {
          challenge: crypto.getRandomValues(new Uint8Array(32)),
          rpId: RP_ID,
          userVerification: "required",
          extensions: requirePrf ? { prf: {} } : undefined,
        },
      });
      cachedRawId = assertion.rawId;
      localStorage.setItem(STORAGE_CRED, bufferToBase64Url(assertion.rawId));
      return cachedRawId;
    } catch {
      /* register */
    }

    const credential = await navigator.credentials.create({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rp: { name: "Beanie", id: RP_ID },
        user: {
          id: crypto.getRandomValues(new Uint8Array(16)),
          name: "beanie-user",
          displayName: "Beanie",
        },
        pubKeyCredParams: [{ type: "public-key", alg: -7 }],
        authenticatorSelection: {
          residentKey: "required",
          userVerification: "required",
        },
        extensions: { prf: {} },
      },
    });

    if (requirePrf) {
      const prf = credential.getClientExtensionResults()?.prf;
      if (!prf?.enabled) {
        throw new Error(
          "Private lanes need a passkey with PRF support. Try another device or turn privacy off."
        );
      }
    }

    cachedRawId = credential.rawId;
    localStorage.setItem(STORAGE_CRED, bufferToBase64Url(credential.rawId));
    return cachedRawId;
  }

  async function buildPasskeyAuth(txHash) {
    const credentialId = await getOrRegisterCredential({ requirePrf: false });
    const challengeBytes = await sha256Utf8(txHash);

    const assertion = await navigator.credentials.get({
      publicKey: {
        challenge: challengeBytes,
        rpId: RP_ID,
        allowCredentials: [{ id: credentialId, type: "public-key" }],
        userVerification: "required",
      },
    });

    const credIdHex = bytesToHex(new Uint8Array(credentialId));
    return {
      credentialId: credIdHex,
      headers: {
        "X-Passkey-Credential-Id": credIdHex,
        "X-Passkey-Client-Data": bufferToBase64Url(assertion.response.clientDataJSON),
        "X-Passkey-Auth-Data": bufferToBase64Url(assertion.response.authenticatorData),
        "X-Passkey-Tx-Hash": txHash,
      },
    };
  }

  /* ---------- Privacy Derivation ---------- */
  async function deriveLaneSalt(laneId) {
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(`beanie-stealth-salt-v1:${laneId}`)
    );
    return new Uint8Array(digest);
  }

  async function evaluatePRF(credentialRawId, salt) {
    const assertion = await navigator.credentials.get({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rpId: RP_ID,
        allowCredentials: [{ id: credentialRawId, type: "public-key" }],
        userVerification: "required",
        extensions: { prf: { eval: { first: salt } } },
      },
    });
    const out = assertion.getClientExtensionResults()?.prf?.results?.first;
    if (!out) throw new Error("PRF derivation failed — no key material returned.");
    return new Uint8Array(out);
  }

  async function derivePrivacyReceivers({ laneId, index = 0 }) {
    const helper = window.beanieStealth;
    if (!helper?.deriveReceivers) {
      throw new Error(
        "Privacy module not loaded. Include stealth helpers and set window.beanieStealth.deriveReceivers."
      );
    }

    const credentialId = await getOrRegisterCredential({ requirePrf: true });
    const salt = await deriveLaneSalt(laneId);
    const master = await evaluatePRF(credentialId, salt);

    return helper.deriveReceivers({
      masterSecret: master,
      laneId,
      index,
      chains: SOURCE_CHAINS.map((k) => ({ key: k, ...CHAINS[k] })),
    });
  }

  /* ---------- RPC and Announcements ---------- */
  async function rpcCall(url, method, params) {
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json", Origin: window.location.origin },
      body: JSON.stringify({ jsonrpc: "2.0", id: Date.now(), method, params }),
    });
    const json = await res.json();
    if (json.error) throw new Error(json.error.message || `${method} failed`);
    return json.result;
  }

  async function predictEvmReceiver(merchantAddress) {
    const chain = CHAINS.BASE;
    const data =
      EVM_PREDICT_SELECTOR +
      merchantAddress.replace(/^0x/i, "").toLowerCase().padStart(64, "0");
    const predictedAddrHex = await rpcCall(
      chain.rpc,
      "eth_call",
      [{ to: chain.factory, data }, "latest"]
    );
    return {
      chain: "BASE",
      address: `0x${String(predictedAddrHex).slice(-40)}`,
      is_privacy_lane: false,
    };
  }

  async function predictStarknetReceiver(merchantAddress) {
    const chain = CHAINS.STARKNET;
    const predictRes = await rpcCall(chain.rpc, "starknet_call", [
      {
        contract_address: chain.factory,
        entry_point_selector: STARKNET_PREDICT_SELECTOR,
        calldata: [merchantAddress],
      },
      "latest",
    ]);
    return {
      chain: "STARKNET",
      address: predictRes?.[0] || "0x0",
      is_privacy_lane: false,
    };
  }

  async function derivePublicReceivers(merchantAddress) {
    const results = await Promise.all([
      predictEvmReceiver(merchantAddress).catch((e) => {
        console.error(e);
        return null;
      }),
      predictStarknetReceiver(merchantAddress).catch((e) => {
        console.error(e);
        return null;
      }),
    ]);
    return results.filter(Boolean);
  }

  function announceBindingHash(chain, merchantAddress) {
    return `beanie-announce-v1|${chain}|${merchantAddress.toLowerCase()}`;
  }

  async function announceReceiverOnChain(chain, merchantAddress) {
    const txHash = announceBindingHash(chain, merchantAddress);
    const passkey = await buildPasskeyAuth(txHash);
    const res = await fetch(API_CREATE, {
      method: "POST",
      headers: { "Content-Type": "application/json", ...passkey.headers },
      body: JSON.stringify({
        chain,
        merchant_address: merchantAddress,
        credential_id: passkey.credentialId,
      }),
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || body.message || `Announce failed (${res.status})`);
    return body;
  }

  async function announceAllSourceChains(merchantAddress) {
    const results = [];
    for (const chain of SOURCE_CHAINS) {
      try {
        const out = await announceReceiverOnChain(chain, merchantAddress);
        results.push({ chain, ok: true, out });
      } catch (e) {
        results.push({ chain, ok: false, error: e.message || String(e) });
      }
    }
    return results;
  }

  /* ---------- Balances / UI Helpers ---------- */
  const encodeEvmBalanceOfCall = (address) =>
    `0x70a08231${address.replace(/^0x/i, "").padStart(64, "0")}`;

  async function tokenBalance(chainKey, address) {
    const chain = CHAINS[chainKey];
    if (!chain) return 0n;
    if (chain.kind === "evm") {
      const result = await rpcCall(
        chain.rpc,
        "eth_call",
        [{ to: chain.usdc, data: encodeEvmBalanceOfCall(address) }, "latest"]
      );
      return BigInt(result || "0x0");
    }
    const result = await rpcCall(chain.rpc, "starknet_call", [
      {
        contract_address: chain.usdc,
        entry_point_selector: STARKNET_BALANCEOF_SELECTOR,
        calldata: [address],
      },
      "latest",
    ]);
    return (BigInt(result?.[1] || "0") << 128n) + BigInt(result?.[0] || "0");
  }

  async function contractExists(chainKey, address) {
    const chain = CHAINS[chainKey];
    if (!chain) return false;
    try {
      if (chain.kind === "evm") {
        const code = await rpcCall(chain.rpc, "eth_getCode", [address, "latest"]);
        return typeof code === "string" && code !== "0x";
      }
      await rpcCall(chain.rpc, "starknet_getClassHashAt", ["latest", address]);
      return true;
    } catch {
      return false;
    }
  }

  function formatUsdc(atoms) {
    return (Number(atoms) / 1e6).toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: 6,
    });
  }

  const EVM_MAGNITUDE_LIMIT = 1n << 160n;
  const STARK_PRIME = (1n << 251n) + 17n * (1n << 192n) + 1n;

  function isValidEvmAddress(v) {
    if (typeof v !== "string" || !v.startsWith("0x") || v.length !== 42) return false;
    try {
      return BigInt(v) < EVM_MAGNITUDE_LIMIT;
    } catch {
      return false;
    }
  }

  function isValidStarknetAddress(v) {
    if (typeof v !== "string" || !v) return false;
    const hex = v.replace(/^0x/i, "");
    if (hex.length < 1 || hex.length > 64) return false;
    try {
      const val = BigInt("0x" + hex);
      return val >= EVM_MAGNITUDE_LIMIT && val < STARK_PRIME;
    } catch {
      return false;
    }
  }

  function walletMatchesChain(chainKey, value) {
    return CHAINS[chainKey]?.kind === "evm"
      ? isValidEvmAddress(value)
      : isValidStarknetAddress(value);
  }

  function sanitizeWebhookUrl(raw) {
    const trimmed = (raw || "").trim();
    if (!trimmed) return { ok: true, value: null };
    try {
      const parsed = new URL(trimmed);
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
        return { ok: false, error: "Webhook URL must use http or https." };
      }
      return { ok: true, value: parsed.toString() };
    } catch {
      return { ok: false, error: "Webhook URL is not a valid URL." };
    }
  }

  function notify(title, tone = "info", detail = "") {
    const stack = $("#toastStack");
    if (!stack) {
      console.warn(`[notify] ${title}`, detail);
      return;
    }
    const toast = el(
      "div",
      `live-toast ${tone}`,
      `<span class="live-toast-icon">${tone === "success" ? "✓" : tone === "error" ? "!" : "i"}</span>
       <div><p class="live-toast-title">${escapeHtml(title)}</p>
       ${detail ? `<p class="live-toast-body">${escapeHtml(detail)}</p>` : ""}</div>
       <button class="live-toast-close" type="button">×</button>`
    );
    toast.querySelector(".live-toast-close")?.addEventListener("click", () => toast.remove());
    stack.append(toast);
    setTimeout(() => toast.remove(), 7000);
  }

  const chainLabel = (k) => CHAINS[k]?.name || k;
  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" width="24" height="24"><circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" width="24" height="24"><circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`,
  };

  function laneShareUrl(lane) {
    const url = new URL("/pay", window.location.origin);
    url.searchParams.set("lane", lane.id);
    url.searchParams.set("idx", lane.currentIndex || 0);
    if (lane.privacy) url.searchParams.set("privacy", "1");
    for (const r of lane.receivers) {
      url.searchParams.append("r", `${r.chain}:${r.address}`);
    }
    return url.toString();
  }

  function escapeAttr(value) {
    return escapeHtml(value).replace(/`/g, "&#096;");
  }

  function renderLanes() {
    const lanes = getLanes();
    const subtitle = $("#receiversSubtitle");
    if (subtitle)
      subtitle.textContent = lanes.length
        ? `${lanes.length} lane${lanes.length > 1 ? "s" : ""}`
        : "No lanes yet";

    const list = $("#receiversList");
    if (!list) return;

    list.innerHTML = lanes.length
      ? ""
      : `<p class="receiver-empty">No lanes yet — create a payment lane to see it here.</p>`;

    for (const lane of lanes) {
      const laneUrl = laneShareUrl(lane);
      const privacyBadge = lane.privacy ? " · private" : "";
      const row = el(
        "div",
        "receiver-row",
        `<span class="mini-icon">${chainIcons[lane.targetChain] || ""}</span>
         <span>
           <div>${escapeHtml(short(lane.merchantAddress))} (Index #${lane.currentIndex || 0})</div>
           <div class="mono">settles on ${escapeHtml(chainLabel(lane.targetChain))}${privacyBadge} · ${lane.receivers.filter((r) => r.status === "active").length}/${lane.receivers.length} live</div>
         </span>
         <a class="receiver-link" href="${escapeAttr(laneUrl)}" target="_blank" rel="noreferrer">Pay page</a>
         <button class="receiver-copy" type="button">⧉</button>`
      );
      row.style.gridTemplateColumns = "24px 1fr auto auto";
      row.querySelector(".receiver-copy")?.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(laneUrl);
          notify("Pay link copied");
        } catch {
          notify("Copy failed", "error");
        }
      });
      list.append(row);
    }
  }

  function setReceiverStatus(chain, address, status) {
    const lanes = getLanes();
    let changed = false;
    for (const lane of lanes) {
      for (const r of lane.receivers) {
        if (r.chain === chain && r.address === address && r.status !== status) {
          r.status = status;
          changed = true;
        }
      }
    }
    if (changed) {
      saveLanes(lanes);
      renderLanes();
    }
  }

  /* ---------- History Panel ---------- */
  let historyFilterChain = "ALL";

  function renderHistoryChainMenu() {
    const menu = $("#historyChainMenu");
    if (!menu) return;
    menu.innerHTML = ["ALL", ...Object.keys(CHAINS)].map((key) => `
    <button class="history-chain-option" type="button" data-chain="${key}" aria-selected="${key === historyFilterChain}">
      <span class="mini-icon">${key === "ALL" ? "🔗" : chainIcons[key] || ""}</span>
      <span>${key === "ALL" ? "All chains" : escapeHtml(chainLabel(key))}</span>
    </button>
  `).join("");
  }

  function updateHistoryChainControl() {
    const icon = $("#historyChainIcon");
    const name = $("#historyChainName");
    if (icon) icon.innerHTML = historyFilterChain === "ALL" ? "🔗" : (chainIcons[historyFilterChain] || "");
    if (name) name.textContent = historyFilterChain === "ALL" ? "All chains" : chainLabel(historyFilterChain);
  }

  function renderHistory() {
    const history = getHistory();
    const rows = Object.values(history).flat()
      .filter((row) => historyFilterChain === "ALL" || row.chain === historyFilterChain)
      .sort((a, b) => b.time - a.time);

    const scopeLabel = historyFilterChain === "ALL" ? "all chains" : chainLabel(historyFilterChain);
    const total = rows.reduce((sum, row) => sum + Number(row.amount || 0), 0);

    const panel = $("#balancePanel");
    if (panel) {
      panel.innerHTML = `
      <div class="balance-card">
        <div>
          <span class="balance-label">Total received on ${escapeHtml(scopeLabel)}</span>
          <div class="balance-value">${escapeHtml(formatUsdc(total))} USDC</div>
        </div>
      </div>
    `;
    }

    const list = $("#historyList");
    if (list) {
      list.innerHTML = rows.length
        ? rows.map((row) => `
        <div class="history-row">
          <span class="mini-icon">${chainIcons[row.chain] || ""}</span>
          <span class="mono">${escapeHtml(short(row.address))}</span>
          <strong>${escapeHtml(formatUsdc(row.amount))} USDC</strong>
          <span>${new Date(row.time).toLocaleString()}</span>
        </div>
      `).join("")
        : `<p class="history-empty">History is empty — no deposits detected yet on ${escapeHtml(scopeLabel)}.</p>`;
    }

    const subtitle = $("#historySubtitle");
    if (subtitle) subtitle.textContent = rows.length
      ? `${rows.length} deposit${rows.length === 1 ? "" : "s"} received`
      : "Nothing yet";
  }

  function openHistory() {
    updateHistoryChainControl();
    renderHistoryChainMenu();
    renderHistory();
    $("#historyModal")?.classList.add("open");
    setSeenAt(Date.now());
    refreshNotifyDot();
  }

  /* ---------- Polling ---------- */
  async function pollDeposits(chain, address) {
    let balance;
    try {
      balance = await tokenBalance(chain, address);
    } catch {
      return;
    }
    const balances = getBalances();
    const key = receiverKey(chain, address);
    const previous = BigInt(balances[key] || "0");
    if (balance > previous) {
      const history = getHistory();
      const entries = history[key] || [];
      entries.unshift({
        chain,
        address,
        amount: (balance - previous).toString(),
        time: Date.now(),
      });
      history[key] = entries.slice(0, 50);
      saveHistory(history);
      notify(
        "Deposit detected",
        "success",
        `${formatUsdc(balance - previous)} USDC on ${chainLabel(chain)}`
      );
      refreshNotifyDot();
      if (activeReceiver?.chain === chain && activeReceiver?.address === address) {
        const statusEl = $("#statusLine");
        if (statusEl) statusEl.textContent = "Payment received.";
      }
    } else if (balance < previous) {
      if (chain === "BASE") {
        notify("Funds settled", "success", `${chainLabel(chain)} lane swept to your wallet.`);
      } else {
        notify(
          "Deposit shielded",
          "info",
          "Moved into the Starknet privacy pool — payout follows via bridge-out."
        );
      }
    }
    balances[key] = balance.toString();
    saveBalances(balances);
  }

  async function pollLane(lane) {
    for (const r of lane.receivers) {
      if (r.status !== "active") {
        const exists = await contractExists(r.chain, r.address).catch(() => false);
        if (exists) setReceiverStatus(r.chain, r.address, "active");
      }
      await pollDeposits(r.chain, r.address);
    }
  }

  function pollAllLanes() {
    getLanes().forEach((lane) => pollLane(lane).catch(() => { }));
  }

  /* ---------- Create Lane Handler ---------- */
  async function handleSubmit(event) {
    if (event) event.preventDefault();
    const btn = $("#createReceiverBtn") || $("#submitBtn");
    if (btn?.disabled) return;

    const walletEl = $("#wallet") || $("#merchantAddress");
    const settlementEl = $("#settlementChain") || $("#targetChain");
    const webhookEl = $("#webhookUrl");
    const privacy = privacyOptedIn();

    const merchantAddress = walletEl?.value?.trim() || "";
    const chainOption = settlementEl?.value || "";
    const targetChain = OPTION_TO_CHAIN[chainOption] || chainOption.toUpperCase();

    if (walletEl) walletEl.classList.remove("invalid");

    if (!targetChain || !CHAINS[targetChain]) {
      notify("Choose a settlement chain", "error", "Base and Starknet are supported right now.");
      return;
    }
    if (!merchantAddress || !walletMatchesChain(targetChain, merchantAddress)) {
      if (walletEl) walletEl.classList.add("invalid");
      notify(
        "Check your wallet address",
        "error",
        `Enter a valid ${chainLabel(targetChain)} address.`
      );
      return;
    }

    const webhookResult = sanitizeWebhookUrl(webhookEl?.value || "");
    if (!webhookResult.ok) {
      notify("Check your webhook URL", "error", webhookResult.error);
      return;
    }

    if (btn) {
      btn.disabled = true;
      btn.textContent = privacy ? "Setting up private lane…" : "Creating…";
    }

    try {
      const all = getLanes();
      const currentIndex = all.length;
      const laneId = `lane_${Date.now()}`;

      let receivers = [];
      let announced = [];

      if (privacy) {
        if (btn) btn.textContent = "Confirm passkey…";
        const stealth = await derivePrivacyReceivers({ laneId, index: 0 });

        if (!stealth?.length) {
          throw new Error("Could not derive stealth merchant identities.");
        }

        for (const s of stealth) {
          const chain = String(s.chain || "").toUpperCase();
          const stealthMerchant = s.address;
          if (!stealthMerchant) continue;

          let predicted;
          try {
            if (chain === "BASE") {
              predicted = await predictEvmReceiver(stealthMerchant);
            } else if (chain === "STARKNET") {
              predicted = await predictStarknetReceiver(stealthMerchant);
            } else {
              continue;
            }
          } catch (e) {
            console.error(`predict failed ${chain}`, e);
            notify("Predict failed", "info", `${chain}: ${e.message}`);
            continue;
          }

          receivers.push({
            chain,
            address: predicted.address,
            isPrivacy: true,
            stealthMerchant,
            status: "pending",
          });

          if (btn) btn.textContent = `Announcing ${chain}…`;
          try {
            await announceReceiverOnChain(chain, stealthMerchant);
            announced.push(chain);
          } catch (e) {
            console.error(`announce failed ${chain}`, e);
            notify("Announce failed", "info", `${chain}: ${e.message}`);
          }
        }

        if (!receivers.length) {
          throw new Error("Could not derive private factory receivers.");
        }
      } else {
        receivers = (await derivePublicReceivers(merchantAddress)).map((l) => ({
          chain: String(l.chain || "").toUpperCase(),
          address: l.address,
          isPrivacy: false,
          status: "pending",
        }));

        if (!receivers.length) {
          throw new Error("Could not predict receiver addresses.");
        }

        if (btn) btn.textContent = "Confirm passkey…";
        const announceResults = await announceAllSourceChains(merchantAddress);
        const failed = announceResults.filter((r) => !r.ok);

        if (failed.length === announceResults.length) {
          throw new Error(
            failed.map((f) => `${f.chain}: ${f.error}`).join("; ") ||
            "Announce failed on all chains"
          );
        }
        if (failed.length) {
          notify(
            "Partial announce",
            "info",
            failed.map((f) => `${f.chain}: ${f.error}`).join("; ")
          );
        }

        announced = announceResults.filter((r) => r.ok).map((r) => r.chain);
      }

      const record = {
        id: laneId,
        merchantAddress,
        targetChain,
        currentIndex,
        webhookUrl: webhookResult.value,
        createdAt: Date.now(),
        privacy,
        announced,
        receivers,
      };

      all.unshift(record);
      saveLanes(all);

      notify(
        privacy ? "Private payment lane ready" : "Payment lane created",
        "success",
        privacy
          ? "Receivers are linked to a passkey-derived identity — your wallet stays off-chain."
          : "Receivers announced — native deposits will be detected."
      );

      window.location.href = laneShareUrl(record);
      pollAllLanes();
    } catch (error) {
      console.error("[beanie] create failed:", error);
      notify("Could not create payment lane", "error", error.message || "");
      if (btn) {
        btn.disabled = false;
        btn.textContent = "Create Payment Link";
      }
    }
  }

  /* ---------- Privacy Segmented Control ---------- */
  const privacyOpenBtn = $("#privacyOpen");
  const privacyStealthBtn = $("#privacyStealth");
  const privacyToggle = $("#privacyToggle");

  privacyOpenBtn?.addEventListener("click", () => {
    privacyOpenBtn.setAttribute("aria-pressed", "true");
    privacyStealthBtn?.setAttribute("aria-pressed", "false");
    if (privacyToggle) privacyToggle.checked = false;
  });

  privacyStealthBtn?.addEventListener("click", () => {
    privacyStealthBtn.setAttribute("aria-pressed", "true");
    privacyOpenBtn?.setAttribute("aria-pressed", "false");
    if (privacyToggle) privacyToggle.checked = true;
  });

  /* ---------- Event Listeners ---------- */
  $("#receiverForm")?.addEventListener("submit", handleSubmit);

  $("#historyBtn")?.addEventListener("click", openHistory);
  $("#closeHistoryModal")?.addEventListener("click", () => $("#historyModal")?.classList.remove("open"));
  $("#historyModal")?.addEventListener("click", (e) => {
    if (e.target.id === "historyModal") $("#historyModal")?.classList.remove("open");
  });

  $("#historyChainSelect")?.addEventListener("click", (e) => {
    e.stopPropagation();
    renderHistoryChainMenu();
    $("#historyChainMenu")?.classList.toggle("open");
  });

  $("#historyChainMenu")?.addEventListener("click", (e) => {
    const option = e.target.closest(".history-chain-option");
    if (!option) return;
    historyFilterChain = option.dataset.chain;
    updateHistoryChainControl();
    renderHistoryChainMenu();
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
  $("#closeReceiversModal")?.addEventListener("click", () =>
    $("#receiversModal")?.classList.remove("open")
  );
  $("#receiversModal")?.addEventListener("click", (e) => {
    if (e.target.id === "receiversModal") $("#receiversModal")?.classList.remove("open");
  });

  const selectEl = $("#settlementChain");
  selectEl?.addEventListener("change", () => {
    const rawVal = selectEl.value;
    const targetChain = OPTION_TO_CHAIN[rawVal] || rawVal?.toUpperCase();
    const summaryEl = $("#routeSummary");
    if (summaryEl) {
      summaryEl.textContent = targetChain
        ? `USDC lands on ${chainLabel(targetChain)}`
        : "USDC lands on Starknet";
    }
    const walletEl = $("#wallet");
    if (walletEl && walletEl.value.trim()) {
      walletEl.classList.toggle(
        "invalid",
        !!targetChain && !walletMatchesChain(targetChain, walletEl.value.trim())
      );
    }
  });

  selectEl?.addEventListener("change", () => {
    const rawVal = selectEl.value;
    const targetChain = OPTION_TO_CHAIN[rawVal] || rawVal?.toUpperCase();

    const summaryEl = $("#routeSummary");
    if (summaryEl) {
      summaryEl.textContent = targetChain
        ? `USDC lands on ${chainLabel(targetChain)}`
        : "USDC lands on Starknet";
    }

    const walletEl = $("#wallet");
    if (walletEl && walletEl.value.trim()) {
      walletEl.classList.toggle(
        "invalid",
        !!targetChain && !walletMatchesChain(targetChain, walletEl.value.trim())
      );
    }
  });

  /* ---------- Init ---------- */
  restrictChainSelect();
  renderLanes();
  refreshNotifyDot();

  const storedLanes = getLanes();
  if (storedLanes.length) {
    activeLane = storedLanes[0];
    revealShareRoute();
  }

  pollAllLanes();
  setInterval(pollAllLanes, POLL_INTERVAL_MS);
  setInterval(refreshNotifyDot, 5000);
})();
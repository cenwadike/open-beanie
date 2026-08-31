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
      explorerAddress: "https://basescan.org/address/",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      rpc: "https://starknet-mainnet.g.alchemy.com/starknet/version/rpc/v0_10/alch_ElnVnrKipwLUIRlEuAmno",
      usdc: "0x33068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
      explorerAddress: "https://starkscan.co/contract/",
    },
  };

  const SOURCE_CHAINS = ["BASE", "STARKNET"];
  const OPTION_TO_CHAIN = { base: "BASE", starknet: "STARKNET" };
  const POLL_INTERVAL_MS = 20000;
  const STARKNET_BALANCEOF_SELECTOR = "0x2e4263afad30923c891518314c3c95dbe830a16874e8abc5777a9a20b54c76";

  const STORAGE_LANES = "beanie.lanes.v1";
  const STORAGE_HISTORY = "beanie.history.v1";
  const STORAGE_BALANCES = "beanie.balances.v1";
  const STORAGE_SEEN = "beanie.history.seen.v1";

  /* ---------- DOM helpers ---------- */
  const $ = (sel) => document.querySelector(sel);
  const el = (tag, cls, html) => {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (html !== undefined) node.innerHTML = html;
    return node;
  };
  const escapeHtml = (v) => String(v ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;",
  }[c]));
  const short = (v) => (v && v.length > 16 ? `${v.slice(0, 8)}…${v.slice(-6)}` : v || "");

  /* ---------- local storage ---------- */
  const readJson = (key, fallback) => {
    try { const raw = localStorage.getItem(key); return raw ? JSON.parse(raw) : fallback; }
    catch { return fallback; }
  };
  const writeJson = (key, value) => {
    try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* storage unavailable */ }
  };
  const getLanes = () => readJson(STORAGE_LANES, []);
  const saveLanes = (lanes) => writeJson(STORAGE_LANES, lanes);
  const getHistory = () => readJson(STORAGE_HISTORY, {});
  const saveHistory = (history) => writeJson(STORAGE_HISTORY, history);
  const getBalances = () => readJson(STORAGE_BALANCES, {});
  const saveBalances = (balances) => writeJson(STORAGE_BALANCES, balances);
  const getSeenAt = () => Number(localStorage.getItem(STORAGE_SEEN) || 0);
  const setSeenAt = (ts) => { try { localStorage.setItem(STORAGE_SEEN, String(ts)); } catch { /* noop */ } };
  const receiverKey = (chain, address) => `${chain}:${address}`.toLowerCase();

  /* ---------- public-RPC reads ---------- */
  async function rpcCall(url, method, params) {
    const res = await fetch(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "Origin": window.location.origin, // Guarantees origin presence for Alchemy domain restrictions
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: Date.now(), method, params }),
    });
    const json = await res.json();
    if (json.error) throw new Error(json.error.message || `${method} failed`);
    return json.result;
  }

  const encodeEvmBalanceOfCall = (address) => `0x70a08231${address.replace(/^0x/i, "").padStart(64, "0")}`;

  async function evmContractExists(rpc, address) {
    const code = await rpcCall(rpc, "eth_getCode", [address, "latest"]);
    return typeof code === "string" && code !== "0x";
  }

  async function evmTokenBalance(rpc, token, address) {
    if (!token) return 0n;
    const result = await rpcCall(rpc, "eth_call", [{ to: token, data: encodeEvmBalanceOfCall(address) }, "latest"]);
    return BigInt(result || "0x0");
  }

  async function starknetContractExists(rpc, address) {
    try { await rpcCall(rpc, "starknet_getClassHashAt", ["latest", address]); return true; }
    catch { return false; }
  }

  async function starknetTokenBalance(rpc, token, address) {
    if (!token) return 0n;
    const result = await rpcCall(rpc, "starknet_call", [
      { contract_address: token, entry_point_selector: STARKNET_BALANCEOF_SELECTOR, calldata: [address] },
      "latest",
    ]);
    const low = BigInt(result?.[0] || "0x0");
    const high = BigInt(result?.[1] || "0x0");
    return (high << 128n) + low;
  }

  async function contractExists(chainKey, address) {
    const chain = CHAINS[chainKey];
    if (!chain) return false;
    try {
      return chain.kind === "evm"
        ? await evmContractExists(chain.rpc, address)
        : await starknetContractExists(chain.rpc, address);
    } catch { return false; }
  }

  async function tokenBalance(chainKey, address) {
    const chain = CHAINS[chainKey];
    if (!chain) return 0n;
    return chain.kind === "evm"
      ? evmTokenBalance(chain.rpc, chain.usdc, address)
      : starknetTokenBalance(chain.rpc, chain.usdc, address);
  }

  function formatUsdc(atoms) {
    const value = Number(atoms) / 1_000_000;
    return value.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 6 });
  }

  /* ---------- wallet-address validation ---------- */
  const EVM_MAGNITUDE_LIMIT = 1n << 160n;
  const STARK_PRIME = (1n << 251n) + (17n * (1n << 192n)) + 1n;

  function isValidEvmAddress(v) {
    if (typeof v !== "string" || !v.startsWith("0x")) return false;
    const hex = v.slice(2);
    if (hex.length !== 40) return false;
    try {
      const val = BigInt(`0x${hex}`);
      return val < EVM_MAGNITUDE_LIMIT;
    } catch {
      return false;
    }
  }

  function isValidStarknetAddress(v) {
    if (typeof v !== "string" || !v) return false;
    const hex = (v.startsWith("0x") || v.startsWith("0X")) ? v.slice(2) : v;
    if (hex.length < 1 || hex.length > 64) return false;
    try {
      const val = BigInt(`0x${hex}`);
      return val >= EVM_MAGNITUDE_LIMIT && val < STARK_PRIME;
    } catch {
      return false;
    }
  }

  function walletMatchesChain(chainKey, value) {
    if (CHAINS[chainKey]?.kind === "evm") return isValidEvmAddress(value);
    if (CHAINS[chainKey]?.kind === "starknet") return isValidStarknetAddress(value);
    return false;
  }

  /* ---------- URL sanitization ---------- */
  function sanitizeWebhookUrl(raw) {
    const trimmed = (raw || "").trim();
    if (!trimmed) return { ok: true, value: null };
    let parsed;
    try { parsed = new URL(trimmed); }
    catch { return { ok: false, error: "Webhook URL is not a valid URL." }; }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      return { ok: false, error: "Webhook URL must use http or https." };
    }
    return { ok: true, value: parsed.toString() };
  }

  /* ---------- backend API ---------- */
  async function createLane({ merchantAddress, targetChain, webhookUrl }) {
    const normalizedTargetChain = (targetChain || "").toUpperCase();

    const body = {
      merchant_address: merchantAddress,
      target_chain: normalizedTargetChain,
      source_chains: SOURCE_CHAINS,
      enable_privacy: false,
      webhook_url: webhookUrl || null,
    };

    console.info("[beanie] POST /api/v1/lanes/init", body);

    const res = await fetch("/api/v1/lanes/init", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "Idempotency-Key": `lane_${Date.now()}_${Math.random().toString(36).slice(2, 9)}`,
      },
      body: JSON.stringify(body),
    }).catch((err) => {
      console.error("[beanie] Network error reaching API:", err);
      throw err;
    });

    const payload = await res.json().catch(() => ({}));

    if (!res.ok) {
      console.error("[beanie] /api/v1/lanes/init failed", res.status, payload);
      throw new Error(payload.error || payload.message || `HTTP ${res.status}`);
    }

    console.info("[beanie] /api/v1/lanes/init OK", payload);
    return payload.lanes || [];
  }

  /* ---------- QR ---------- */
  function paymentUri(chainKey, address) {
    return CHAINS[chainKey]?.kind === "evm" ? `ethereum:${address}` : address;
  }

  async function renderQr(imgEl, text) {
    if (!imgEl || !window.QRCode?.toString) return;
    try {
      const svg = await window.QRCode.toString(text, { type: "svg", errorCorrectionLevel: "H", margin: 1, width: 480 });
      imgEl.src = `data:image/svg+xml;charset=UTF-8,${encodeURIComponent(svg)}`;
    } catch { imgEl.removeAttribute("src"); }
  }

  /* ---------- toasts ---------- */
  function notify(title, tone = "info", detail = "") {
    const stack = $("#toastStack");
    if (!stack) {
      console.warn(`[notify] ${tone.toUpperCase()}: ${title} - ${detail}`);
      return;
    }
    const toast = el("div", `live-toast ${tone}`, `
      <span class="live-toast-icon">${tone === "success" ? "✓" : tone === "error" ? "!" : "i"}</span>
      <div>
        <p class="live-toast-title">${escapeHtml(title)}</p>
        ${detail ? `<p class="live-toast-body">${escapeHtml(detail)}</p>` : ""}
      </div>
      <button class="live-toast-close" type="button" aria-label="Dismiss">×</button>
    `);
    toast.querySelector(".live-toast-close")?.addEventListener("click", () => toast.remove());
    stack.append(toast);
    setTimeout(() => toast.remove(), 7000);
  }

  /* ---------- UI State & Execution ---------- */
  function historyToggleBtn() {
    return document.querySelector(".history-btn:not(.receivers-btn)");
  }

  function ensureNotifyDot() {
    const btn = historyToggleBtn();
    if (!btn) return null;
    let dot = btn.querySelector("#historyDot");
    if (!dot) {
      dot = el("span", "notify-dot");
      dot.id = "historyDot";
      dot.hidden = true;
      btn.append(dot);
    }
    return dot;
  }

  function pendingDepositCount() {
    const seenAt = getSeenAt();
    return Object.values(getHistory()).flat().filter((entry) => entry.time > seenAt).length;
  }

  function refreshNotifyDot() {
    const dot = ensureNotifyDot();
    if (dot) dot.hidden = pendingDepositCount() === 0;
  }

  const chainIcons = {
    BASE: `<svg viewBox="0 0 42 42" width="24" height="24" aria-hidden="true">
      <circle cx="21" cy="21" r="21" fill="#0052ff"/>
      <path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/>
    </svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" width="24" height="24" aria-hidden="true">
      <circle cx="21" cy="21" r="21" fill="#0c0c4d"/>
      <path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/>
    </svg>`,
  };
  const chainLabel = (chainKey) => CHAINS[chainKey]?.name || chainKey;

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
    if (changed) { saveLanes(lanes); renderLanes(); }
  }

  // Automatically update routing to point to /pay with full receiver params after lane creation:
  function laneShareUrl(lane) {
    const url = new URL("/pay", window.location.origin);
    url.searchParams.set("lane", lane.id);
    for (const r of lane.receivers) {
      url.searchParams.append("r", `${r.chain}:${r.address}`);
    }
    return url.toString();
  }

  function renderLanes() {
    const lanes = getLanes();
    const subtitle = $("#receiversSubtitle");
    if (subtitle) subtitle.textContent = lanes.length ? `${lanes.length} lane${lanes.length > 1 ? "s" : ""}` : "No lanes yet";
    const list = $("#receiversList");
    if (!list) return;
    list.innerHTML = lanes.length ? "" : `<p class="receiver-empty">No lanes yet — create a payment lane to see it here.</p>`;
    for (const lane of lanes) {
      const laneUrl = laneShareUrl(lane);
      const row = el("div", "receiver-row", `
        <span class="mini-icon">${chainIcons[lane.targetChain] || ""}</span>
        <span>
          <div>${escapeHtml(short(lane.merchantAddress))}</div>
          <div class="mono">settles on ${escapeHtml(chainLabel(lane.targetChain))} · ${lane.receivers.filter((r) => r.status === "active").length}/${lane.receivers.length} live</div>
        </span>
        <a class="receiver-link" href="${escapeAttr(laneUrl)}" target="_blank" rel="noreferrer">Pay page</a>
        <button class="receiver-copy" type="button" aria-label="Copy pay link">⧉</button>
      `);
      row.style.gridTemplateColumns = "24px 1fr auto auto";
      row.querySelector(".receiver-copy")?.addEventListener("click", async () => {
        try { await navigator.clipboard.writeText(laneUrl); notify("Pay link copied"); }
        catch { notify("Copy failed", "error"); }
      });
      list.append(row);
    }
  }

  function escapeAttr(value) { return escapeHtml(value).replace(/`/g, "&#096;"); }

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

  function revealShareRoute() {
    $("#shareBtn")?.classList.remove("is-hidden");
  }

  let activeLane = null;
  let activeReceiver = null;

  async function showResultFor(lane, receiver) {
    activeLane = lane;
    activeReceiver = receiver;
    const addrEl = $("#address");
    if (addrEl) addrEl.textContent = receiver.address;

    const summaryEl = $("#routeSummary");
    if (summaryEl) summaryEl.textContent = `USDC lands on ${chainLabel(lane.targetChain)}`;

    const statusEl = $("#statusLine");
    if (statusEl) statusEl.textContent = "Waiting for chain confirmation...";

    $("#qrWrap")?.classList.add("pending");
    $("#result")?.classList.add("visible");
    $("#result")?.classList.remove("locked");
    document.querySelector(".stage")?.classList.add("created");

    const qrImg = $("#qr");
    if (qrImg) await renderQr(qrImg, paymentUri(receiver.chain, receiver.address));

    const qrLink = $("#qrLink");
    if (qrLink) qrLink.href = paymentUri(receiver.chain, receiver.address);

    const exists = await contractExists(receiver.chain, receiver.address).catch(() => false);
    if (exists) {
      setReceiverStatus(receiver.chain, receiver.address, "active");
      if (statusEl) statusEl.textContent = "Ready to receive USDC.";
      $("#qrWrap")?.classList.remove("pending");
    }
  }

  async function pollDeposits(chain, address) {
    let balance;
    try { balance = await tokenBalance(chain, address); } catch { return; }
    const balances = getBalances();
    const key = receiverKey(chain, address);
    const previous = BigInt(balances[key] || "0");
    if (balance > previous) {
      const history = getHistory();
      const entries = history[key] || [];
      entries.unshift({ chain, address, amount: (balance - previous).toString(), time: Date.now() });
      history[key] = entries.slice(0, 50);
      saveHistory(history);
      notify("Deposit detected", "success", `${formatUsdc(balance - previous)} USDC on ${chainLabel(chain)}`);
      refreshNotifyDot();
      if (activeReceiver?.chain === chain && activeReceiver?.address === address) {
        const statusEl = $("#statusLine");
        if (statusEl) statusEl.textContent = "Payment received.";
      }
    } else if (balance < previous) {
      if (chain === "BASE") {
        notify("Funds settled", "success", `${chainLabel(chain)} lane swept to your wallet.`);
      } else {
        notify("Deposit shielded", "info", "Moved into the Starknet privacy pool — payout follows via bridge-out.");
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

  function restrictChainSelect() {
    const select = $("#settlementChain") || $("#targetChain");
    if (!select) return;
    const solanaOption = select.querySelector('option[value="solana"]');
    if (solanaOption) {
      solanaOption.disabled = true;
      solanaOption.textContent = "Solana (coming soon)";
    }
  }

  /* ---------- Unified Form Submission ---------- */
  async function handleSubmit(event) {
    if (event) event.preventDefault();

    const btn = $("#createReceiverBtn") || $("#submitBtn");
    if (btn?.disabled) return;

    // Safely extract inputs across differing markup structures
    const walletEl = $("#wallet") || $("#merchantAddress");
    const settlementEl = $("#settlementChain") || $("#targetChain");
    const webhookEl = $("#webhookUrl");

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
      notify("Check your wallet address", "error", `Enter a valid ${chainLabel(targetChain)} address.`);
      return;
    }

    const webhookResult = sanitizeWebhookUrl(webhookEl?.value || "");
    if (!webhookResult.ok) {
      notify("Check your webhook URL", "error", webhookResult.error);
      return;
    }

    if (btn) {
      btn.disabled = true;
      btn.textContent = "Creating…";
    }

    try {
      const lanes = await createLane({
        merchantAddress,
        targetChain,
        webhookUrl: webhookResult.value,
      });

      const record = {
        id: `lane_${Date.now()}`,
        merchantAddress,
        targetChain,
        webhookUrl: webhookResult.value,
        createdAt: Date.now(),
        receivers: lanes.map((l) => ({
          chain: String(l.chain || "").toUpperCase(),
          address: l.address,
          isPrivacy: Boolean(l.is_privacy_lane),
          status: "pending",
        })),
      };

      const all = getLanes();
      all.unshift(record);
      saveLanes(all);

      // Immediately redirect to main page with lane parameters attached
      window.location.href = laneShareUrl(record);
      pollAllLanes()
    } catch (error) {
      console.error("[beanie] Submission error:", error);
      notify("Could not create payment lane", "error", error.message || "");
      if (btn) {
        btn.disabled = false;
        btn.textContent = "Create Payment Lane";
      }
    }
  }

  /* ---------- Event Listeners ---------- */
  $("#receiverForm")?.addEventListener("submit", handleSubmit);
  $("#laneForm")?.addEventListener("submit", handleSubmit);

  // Bind click only if button isn't nested inside a handled form
  const createBtn = $("#createReceiverBtn") || $("#submitBtn");
  if (createBtn && !createBtn.closest("form")) {
    createBtn.addEventListener("click", handleSubmit);
  }

  const selectEl = $("#settlementChain") || $("#targetChain");
  selectEl?.addEventListener("change", () => {
    const rawVal = selectEl.value;
    const targetChain = OPTION_TO_CHAIN[rawVal] || rawVal?.toUpperCase();
    const summaryEl = $("#routeSummary");
    if (summaryEl) {
      summaryEl.textContent = targetChain ? `USDC lands on ${chainLabel(targetChain)}` : "USDC lands on Starknet";
    }
    const walletEl = $("#wallet") || $("#merchantAddress");
    if (walletEl && walletEl.value.trim()) {
      walletEl.classList.toggle("invalid", !!targetChain && !walletMatchesChain(targetChain, walletEl.value.trim()));
    }
  });

  historyToggleBtn()?.addEventListener("click", openHistory);
  $("#closeHistoryModal")?.addEventListener("click", () => $("#historyModal")?.classList.remove("open"));
  $("#historyModal")?.addEventListener("click", (e) => { if (e.target.id === "historyModal") $("#historyModal")?.classList.remove("open"); });

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

  $("#receiversBtn")?.addEventListener("click", () => { renderLanes(); $("#receiversModal")?.classList.add("open"); });
  $("#closeReceiversModal")?.addEventListener("click", () => $("#receiversModal")?.classList.remove("open"));
  $("#receiversModal")?.addEventListener("click", (e) => { if (e.target.id === "receiversModal") $("#receiversModal")?.classList.remove("open"); });

  $("#backBtn")?.addEventListener("click", () => {
    document.querySelector(".stage")?.classList.remove("created");
    $("#result")?.classList.remove("visible");
  });

  $("#copyBtn")?.addEventListener("click", async () => {
    if (!activeReceiver) return;
    try { await navigator.clipboard.writeText(activeReceiver.address); notify("Address copied"); }
    catch { notify("Copy failed", "error"); }
  });

  $("#shareBtn")?.addEventListener("click", async () => {
    if (!activeLane) { notify("Create a payment lane first", "info"); return; }
    const link = laneShareUrl(activeLane);
    try { await navigator.clipboard.writeText(link); notify("Pay link copied", "success", link); }
    catch { notify("Copy failed", "error"); }
  });

  $("#openRelayBtn")?.addEventListener("click", () => notify("Gasless transfer", "info", "Coming soon."));

  /* ---------- init ---------- */
  restrictChainSelect();
  renderLanes();
  refreshNotifyDot();
  $("#shareBtn")?.classList.add("is-hidden");

  const storedLanes = getLanes();
  if (storedLanes.length) {
    activeLane = storedLanes[0];
    revealShareRoute();
  }

  pollAllLanes();
  setInterval(pollAllLanes, POLL_INTERVAL_MS);
  setInterval(refreshNotifyDot, 5000);
})();
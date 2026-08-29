(() => {
  "use strict";

  /* ---------- Config ----------
   * Base and Starknet only, per the current contract set. Solana stays in the
   * <select> markup but is disabled below until a real receiver exists for it.
   */
  const CHAINS = {
    BASE: {
      name: "Base",
      kind: "evm",
      chainId: 8453,
      rpc: "https://mainnet.base.org",
      usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
      explorerAddress: "https://basescan.org/address/",
    },
    STARKNET: {
      name: "Starknet",
      kind: "starknet",
      rpc: "https://starknet-mainnet.public.blastapi.io/rpc/v0_7",
      // TODO: the token contract the ShieldInAnonymizer pool actually holds balances in.
      usdc: "",
      explorerAddress: "https://starkscan.co/contract/",
    },
  };
  const SOURCE_CHAINS = ["BASE", "STARKNET"];
  const OPTION_TO_CHAIN = { base: "BASE", starknet: "STARKNET" };
  const POLL_INTERVAL_MS = 20000;
  // Standard Starknet "balanceOf" entry-point selector (get_selector_from_name("balanceOf")).
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
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
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
  const isValidEvmAddress = (v) => /^0x[a-fA-F0-9]{40}$/.test(v);
  const isValidStarknetAddress = (v) => /^(0x)?[a-fA-F0-9]{1,64}$/.test(v);
  function walletMatchesChain(chainKey, value) {
    if (CHAINS[chainKey]?.kind === "evm") return isValidEvmAddress(value);
    if (CHAINS[chainKey]?.kind === "starknet") return isValidStarknetAddress(value);
    return false;
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
    if (!stack) return;
    const toast = el("div", `live-toast ${tone}`, `
      <span class="live-toast-icon">${tone === "success" ? "✓" : tone === "error" ? "!" : "i"}</span>
      <div>
        <p class="live-toast-title">${escapeHtml(title)}</p>
        ${detail ? `<p class="live-toast-body">${escapeHtml(detail)}</p>` : ""}
      </div>
      <button class="live-toast-close" type="button" aria-label="Dismiss">×</button>
    `);
    toast.querySelector(".live-toast-close").addEventListener("click", () => toast.remove());
    stack.append(toast);
    setTimeout(() => toast.remove(), 7000);
  }

  /* ---------- unread-deposit pulse dot on the History button ---------- */
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

  /* ---------- chain icons (Base + Starknet only) ---------- */
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

  /* ---------- lane status helpers ---------- */
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

  /* ---------- rendering: Lanes modal ---------- */
  function laneShareUrl(lane) {
    const url = new URL("/pay.html", window.location.origin);
    url.searchParams.set("lane", lane.id);
    for (const r of lane.receivers) url.searchParams.append("r", `${r.chain}:${r.address}`);
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
      row.querySelector(".receiver-copy").addEventListener("click", async () => {
        try { await navigator.clipboard.writeText(laneUrl); notify("Pay link copied"); }
        catch { notify("Copy failed", "error"); }
      });
      list.append(row);
    }
  }

  function escapeAttr(value) { return escapeHtml(value).replace(/`/g, "&#096;"); }

  /* ---------- rendering: History modal ---------- */
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
    $("#historyModal").classList.add("open");
    setSeenAt(Date.now());
    refreshNotifyDot();
  }

  /* ---------- Share Route: hidden until a lane exists ---------- */
  function revealShareRoute() {
    $("#shareBtn")?.classList.remove("is-hidden");
  }

  /* ---------- checkout panel on the create page ---------- */
  let activeLane = null;
  let activeReceiver = null;

  async function showResultFor(lane, receiver) {
    activeLane = lane;
    activeReceiver = receiver;
    $("#address").textContent = receiver.address;
    $("#routeSummary").textContent = `USDC lands on ${chainLabel(lane.targetChain)}`;
    $("#statusLine").textContent = "Waiting for chain confirmation...";
    $("#qrWrap").classList.add("pending");
    $("#result").classList.add("visible");
    $("#result").classList.remove("locked");
    document.querySelector(".stage")?.classList.add("created");
    await renderQr($("#qr"), paymentUri(receiver.chain, receiver.address));
    $("#qrLink").href = paymentUri(receiver.chain, receiver.address);
    const exists = await contractExists(receiver.chain, receiver.address).catch(() => false);
    if (exists) {
      setReceiverStatus(receiver.chain, receiver.address, "active");
      $("#statusLine").textContent = "Ready to receive USDC.";
      $("#qrWrap").classList.remove("pending");
    }
  }

  /* ---------- background polling: deployment confirmation + deposit detection ----------
   * Deliberately simple: poll public RPCs on an interval instead of running a
   * websocket/event-subscription stack. Both ChainXReceiver (Base) and
   * ShieldInAnonymizer (Starknet) hold the token balance until swept/shielded,
   * so a balance-delta increase reliably means "deposit detected" on either
   * chain. A balance *decrease* does NOT mean the same thing on both:
   *   - Base:     sweep() sent the funds onward (same-chain or CCTP burn).
   *   - Starknet: privacy_invoke() shielded the funds into a stealth note —
   *               the merchant hasn't been paid yet; that happens later via
   *               BridgeOutAnonymizer, which this balance poll can't see.
   */
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
        $("#statusLine").textContent = "Payment received.";
      }
    } else if (balance < previous) {
      // Base: sweep() ran — funds actually left for the merchant (same-chain)
      // or CCTP (cross-chain). Starknet: privacy_invoke() ran instead — the
      // balance moved into a stealth note in the STRK20 pool, not to the
      // merchant. The real payout happens later via BridgeOutAnonymizer,
      // which we have no visibility into from this receiver's balance.
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

  /* ---------- settlement-chain <select>: Base + Starknet only ---------- */
  function restrictChainSelect() {
    const select = $("#settlementChain");
    if (!select) return;
    const solanaOption = select.querySelector('option[value="solana"]');
    if (solanaOption) {
      solanaOption.disabled = true;
      solanaOption.textContent = "Solana (coming soon)";
    }
  }

  /* ---------- form ---------- */
  async function handleSubmit(event) {
    event.preventDefault();
    const wallet = $("#wallet");
    const settlementChain = $("#settlementChain");
    const webhookUrl = $("#webhookUrl").value.trim();
    const merchantAddress = wallet.value.trim();
    const chainOption = settlementChain.value;
    const targetChain = OPTION_TO_CHAIN[chainOption];

    wallet.classList.remove("invalid");

    if (!targetChain) {
      notify("Choose a settlement chain", "error", "Base and Starknet are supported right now.");
      return;
    }
    if (!merchantAddress || !walletMatchesChain(targetChain, merchantAddress)) {
      wallet.classList.add("invalid");
      notify("Check your wallet address", "error", `Enter a valid ${chainLabel(targetChain)} address.`);
      return;
    }

    const btn = $("#createReceiverBtn");
    btn.disabled = true;
    btn.textContent = "Creating…";
    try {
      const lanes = await createLane({ merchantAddress, targetChain, webhookUrl });
      const record = {
        id: `lane_${Date.now()}`,
        merchantAddress,
        targetChain,
        webhookUrl,
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
      renderLanes();
      notify("Payment lane created", "success", `${record.receivers.length} route${record.receivers.length === 1 ? "" : "s"} ready to poll.`);

      const primary = record.receivers.find((r) => r.chain === targetChain) || record.receivers[0];
      activeLane = record;
      revealShareRoute();
      if (primary) await showResultFor(record, primary);
      pollAllLanes();
    } catch (error) {
      notify("Could not create payment lane", "error", error.message || "");
    } finally {
      btn.disabled = false;
      btn.textContent = "Create Payment Lane";
    }
  }

  /* ---------- wire up ---------- */
  $("#receiverForm")?.addEventListener("submit", handleSubmit);
  $("#createReceiverBtn")?.addEventListener("click", handleSubmit);

  $("#laneForm")?.addEventListener("submit", async (e) => {
    e.preventDefault();

    const submitBtn = $("#submitBtn");
    if (submitBtn) submitBtn.disabled = true;

    try {
      const merchantAddress = $("#merchantAddress")?.value.trim();
      const rawTarget = $("#targetChain")?.value;
      const targetChain = OPTION_TO_CHAIN[rawTarget] || rawTarget?.toUpperCase();
      const webhookUrl = $("#webhookUrl")?.value.trim();

      if (!merchantAddress || !targetChain) {
        throw new Error("Please enter a valid merchant address and target chain.");
      }

      const lanes = await createLane({ merchantAddress, targetChain, webhookUrl });

      const record = {
        id: `lane_${Date.now()}`,
        merchantAddress,
        targetChain,
        webhookUrl,
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
      renderLanes();
      notify("Payment lane created", "success");
    } catch (err) {
      notify(err.message || "Failed to create lane", "error");
    } finally {
      if (submitBtn) submitBtn.disabled = false;
    }
  });

  $("#settlementChain")?.addEventListener("change", () => {
    const targetChain = OPTION_TO_CHAIN[$("#settlementChain").value];
    $("#routeSummary").textContent = targetChain ? `USDC lands on ${chainLabel(targetChain)}` : "USDC lands on Starknet";
  });

  historyToggleBtn()?.addEventListener("click", openHistory);
  $("#closeHistoryModal")?.addEventListener("click", () => $("#historyModal").classList.remove("open"));
  $("#historyModal")?.addEventListener("click", (e) => { if (e.target.id === "historyModal") $("#historyModal").classList.remove("open"); });

  $("#historyChainSelect")?.addEventListener("click", (e) => {
    e.stopPropagation();
    renderHistoryChainMenu();
    $("#historyChainMenu").classList.toggle("open");
  });
  $("#historyChainMenu")?.addEventListener("click", (e) => {
    const option = e.target.closest(".history-chain-option");
    if (!option) return;
    historyFilterChain = option.dataset.chain;
    updateHistoryChainControl();
    renderHistoryChainMenu();
    $("#historyChainMenu").classList.remove("open");
    renderHistory();
  });
  document.addEventListener("click", (e) => {
    if (!e.target.closest("#historyChainSelect") && !e.target.closest("#historyChainMenu")) {
      $("#historyChainMenu")?.classList.remove("open");
    }
  });

  $("#receiversBtn")?.addEventListener("click", () => { renderLanes(); $("#receiversModal").classList.add("open"); });
  $("#closeReceiversModal")?.addEventListener("click", () => $("#receiversModal").classList.remove("open"));
  $("#receiversModal")?.addEventListener("click", (e) => { if (e.target.id === "receiversModal") $("#receiversModal").classList.remove("open"); });

  $("#backBtn")?.addEventListener("click", () => {
    document.querySelector(".stage")?.classList.remove("created");
    $("#result").classList.remove("visible");
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
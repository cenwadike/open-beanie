(() => {
  "use strict";

  // Where /pay lives: inferred from this script's own src, so the same file
  // works in prod, staging or local dev without editing.
  const script = document.currentScript || document.querySelector('script[src*="embed.js"]');
  if (!script || !script.src) return;
  const ORIGIN = new URL(script.src).origin;

  const RE = {
    lane: /^[A-Za-z0-9_-]{1,128}$/,
    chain: /^[A-Za-z]{2,16}$/,
    addr: /^[0-9A-Za-z]{1,100}$/,
    ref: /^[\w.-]{1,64}$/,
    color: /^#?[0-9a-fA-F]{6}$/,
    css: /^\d+(\.\d+)?(px|%|rem|em|vh|vw)$/,
  };

  function mount(el) {
    if (el.dataset.beanieMounted) return;
    const d = el.dataset;
    const params = new URLSearchParams();

    if (d.lane && RE.lane.test(d.lane)) params.set("lane", d.lane);

    const routes = (d.routes || "").split(",").map((r) => r.trim()).filter(Boolean);
    if (d.chain && d.address) routes.push(`${d.chain}:${d.address}`); // legacy attributes
    for (const r of routes) {
      const i = r.indexOf(":");
      const chain = r.slice(0, i);
      const address = r.slice(i + 1);
      if (i > 0 && RE.chain.test(chain) && RE.addr.test(address)) params.append("r", `${chain}:${address}`);
    }
    if (![...params.keys()].length) return; // nothing to pay to

    if (d.target && RE.chain.test(d.target)) params.set("target", d.target);
    if (d.merchant && RE.addr.test(d.merchant)) params.set("merchant", d.merchant);
    if (d.ref && RE.ref.test(d.ref)) params.set("ref", d.ref);
    if (d.primaryColor && RE.color.test(d.primaryColor)) params.set("primaryColor", d.primaryColor.replace("#", ""));
    if (d.secondaryColor && RE.color.test(d.secondaryColor)) params.set("secondaryColor", d.secondaryColor.replace("#", ""));

    const iframe = document.createElement("iframe");
    iframe.src = `${ORIGIN}/pay?${params.toString()}`;
    iframe.title = "Checkout";
    iframe.allow = "clipboard-write";
    iframe.loading = "lazy";
    iframe.referrerPolicy = "no-referrer";
    Object.assign(iframe.style, {
      display: "block",
      width: "100%",
      maxWidth: RE.css.test(d.width || "") ? d.width : "440px",
      height: RE.css.test(d.height || "") ? d.height : "640px",
      border: "0",
      borderRadius: "20px",
    });
    el.dataset.beanieMounted = "1";
    el.replaceChildren(iframe);
  }

  const mountAll = (root = document) => root.querySelectorAll("[data-beanie-checkout]").forEach(mount);
  mountAll();

  // Support containers added after this script ran (SPAs, late renders).
  new MutationObserver((records) => {
    for (const rec of records) {
      for (const node of rec.addedNodes) {
        if (node.nodeType !== 1) continue;
        if (node.matches?.("[data-beanie-checkout]")) mount(node);
        else mountAll(node);
      }
    }
  }).observe(document.documentElement, { childList: true, subtree: true });
})();
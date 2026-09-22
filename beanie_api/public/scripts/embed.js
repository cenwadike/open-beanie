(() => {
  "use strict";

  // Where /pay lives — inferred from this script's own src so the same file
  // works in prod, staging, or a local dev build without editing anything.
  const ORIGIN = new URL(document.currentScript.src).origin;

  function mount(el) {
    const d = el.dataset;
    const params = new URLSearchParams();

    if (d.lane) params.set("lane", d.lane);
    if (d.chain && d.address) params.set("chain", d.chain), params.set("address", d.address);
    (d.routes || "").split(",").map((r) => r.trim()).filter(Boolean)
      .forEach((r) => params.append("r", r));

    if (d.primaryColor) params.set("primaryColor", d.primaryColor.replace("#", ""));
    if (d.secondaryColor) params.set("secondaryColor", d.secondaryColor.replace("#", ""));

    const iframe = document.createElement("iframe");
    iframe.src = `${ORIGIN}/pay?${params.toString()}`;
    iframe.title = "Checkout";
    iframe.allow = "clipboard-write";
    iframe.style.cssText = `
      display: block;
      width: 100%;
      max-width: ${d.width || "440px"};
      height: ${d.height || "640px"};
      border: 0;
      border-radius: 20px;
    `;
    el.replaceChildren(iframe);
  }

  document.querySelectorAll("[data-beanie-checkout]").forEach(mount);
})();

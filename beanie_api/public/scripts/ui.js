// ui.js
//
// Small DOM-free-where-possible helpers shared by all pages.

import { USDC_DECIMALS } from "./chains.js";

export const $ = (sel, root = document) => root.querySelector(sel);

export const escapeHtml = (v) =>
    String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" }[c]));

export const short = (v) => {
    const s = String(v ?? "");
    return s.length > 16 ? `${s.slice(0, 8)}…${s.slice(-6)}` : s;
};

/** Exact (BigInt) USDC formatting: atoms -> "1,234.50". */
export function formatUsdc(atoms, { minFraction = 2 } = {}) {
    let v;
    try {
        v = BigInt(atoms ?? 0);
    } catch {
        return "0.00";
    }
    const neg = v < 0n;
    if (neg) v = -v;
    const base = 10n ** BigInt(USDC_DECIMALS);
    const whole = v / base;
    let frac = (v % base).toString().padStart(USDC_DECIMALS, "0").replace(/0+$/, "");
    while (frac.length < minFraction) frac += "0";
    return `${neg ? "-" : ""}${whole.toLocaleString("en-US")}${frac ? `.${frac}` : ""}`;
}

/**
 * "12.5" -> "12500000" without floating point. Returns null unless the input
 * is a positive decimal with at most `decimals` fractional digits.
 */
export function parseDecimalToRawUnits(input, decimals = USDC_DECIMALS) {
    if (typeof input !== "string") return null;
    const trimmed = input.trim();
    if (!/^\d+(\.\d+)?$/.test(trimmed)) return null;
    const [whole, fraction = ""] = trimmed.split(".");
    if (fraction.length > decimals) return null;
    const raw = `${whole}${fraction.padEnd(decimals, "0")}`.replace(/^0+(?=\d)/, "");
    let v;
    try {
        v = BigInt(raw || "0");
    } catch {
        return null;
    }
    return v > 0n ? v.toString() : null;
}

export function sanitizeWebhookUrl(raw) {
    const trimmed = (raw || "").trim();
    if (!trimmed) return { ok: true, value: null };
    try {
        const u = new URL(trimmed);
        if (u.protocol !== "http:" && u.protocol !== "https:") {
            return { ok: false, error: "Webhook URL must use http or https." };
        }
        return { ok: true, value: u.toString() };
    } catch {
        return { ok: false, error: "Webhook URL is not a valid URL." };
    }
}

/** Toast built with textContent only (no HTML injection from error text). */
export function notify(title, tone = "info", detail = "") {
    const stack = document.querySelector("#toastStack") || document.querySelector("#payToastStack");
    if (!stack) {
        console.warn(`[notify] ${title}`, detail);
        return;
    }
    const toast = document.createElement("div");
    toast.className = `live-toast ${tone}`;

    const icon = document.createElement("span");
    icon.className = "live-toast-icon";
    icon.textContent = tone === "success" ? "✓" : tone === "error" ? "!" : "i";

    const body = document.createElement("div");
    const t = document.createElement("p");
    t.className = "live-toast-title";
    t.textContent = title;
    body.append(t);
    if (detail) {
        const d = document.createElement("p");
        d.className = "live-toast-body";
        d.textContent = detail;
        body.append(d);
    }

    const close = document.createElement("button");
    close.type = "button";
    close.className = "live-toast-close";
    close.textContent = "×";
    close.addEventListener("click", () => toast.remove());

    toast.append(icon, body, close);
    stack.append(toast);
    setTimeout(() => toast.remove(), 7000);
}

export async function copyText(text, okMessage = "Copied") {
    try {
        await navigator.clipboard.writeText(text);
        notify(okMessage, "success");
        return true;
    } catch {
        notify("Copy failed", "error");
        return false;
    }
}

export function downloadText(filename, text, type = "application/json") {
    const url = URL.createObjectURL(new Blob([text], { type }));
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Turns thrown values into a short message fit for a toast. */
export function describeError(err) {
    if (err?.name === "NotAllowedError" || err?.name === "AbortError") {
        return "The passkey or wallet prompt was cancelled or timed out.";
    }
    if (err?.name === "ApiError" && err.status === 429) {
        const wait = err.retryAfter ? ` Try again in ${err.retryAfter}s.` : " Try again later.";
        return `${err.message}.${wait}`.replace("..", ".");
    }
    if (err?.code === 4001) return "You rejected the request in your wallet.";
    return String(err?.message || err || "Something went wrong.").slice(0, 300);
}
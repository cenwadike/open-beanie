// stealth-claim.js
//
// Runs entirely in the merchant's browser. The spending key never leaves
// this tab — it's read from an input field, held in a local JS variable
// for the duration of the derive+sign+submit flow, and is not sent to
// Beanie's API, logged, or persisted (no localStorage/sessionStorage use,
// per this environment's constraints — and deliberately so even outside
// that constraint, since a spend key has no business touching storage).
//
// Flow:
//   1. Call Beanie's /api/v1/stealth/scan with merchant_address +
//      viewing_key (view-only, safe to send — cannot spend funds) to get
//      candidate stealth addresses.
//   2. For each match the merchant selects, derive the stealth PRIVATE
//      key locally using their spend key (never sent anywhere).
//   3. Sign and submit a transfer from the stealth address to the
//      merchant's real destination address, directly to a public RPC —
//      not through Beanie's backend.
//
// Dependency: starknet.js, loaded from a CDN for this standalone page.
// If you're integrating this into a bundled frontend instead, swap the
// import for your normal package-managed starknet.js install.

import {
  ec,
  RpcProvider,
  Account,
  CallData,
  hash,
} from "https://cdn.jsdelivr.net/npm/starknet@6/dist/index.js";

const RPC_URL = "https://starknet-mainnet.public.blastapi.io"; // swap for your preferred provider
const TOKEN_ADDRESS = "0x..."; // USDC address, fill in
const provider = new RpcProvider({ nodeUrl: RPC_URL });

const $ = (id) => document.getElementById(id);

let currentMatches = [];
let selectedMatch = null;

// ---- Shared STARK-curve helpers (mirror scanner.rs's intended logic) ----

function poseidonHashToFelt(bytesOrFelts) {
  // starknet.js exposes Poseidon hashing via `hash.computePoseidonHash`
  // or similar in v6 — check your installed version's exact API surface.
  return hash.computePoseidonHashOnElements(bytesOrFelts);
}

function deriveSharedSecret(privKeyHex, otherPubKeyHex) {
  // ECDH on the STARK curve: privKey * otherPubKey (point mult).
  const priv = BigInt(privKeyHex);
  const pubPoint = ec.starkCurve.ProjectivePoint.fromHex(otherPubKeyHex.replace("0x", ""));
  const shared = pubPoint.multiply(priv);
  return poseidonHashToFelt([shared.x.toString()]);
}

function deriveViewTag(sharedSecretHash) {
  return "0x" + BigInt(sharedSecretHash).toString(16).slice(0, 2);
}

function deriveStealthPrivateKey(spendPrivKeyHex, sharedSecretHash) {
  const CURVE_ORDER = ec.starkCurve.CURVE.n;
  const spend = BigInt(spendPrivKeyHex);
  const offset = BigInt(sharedSecretHash);
  return ((spend + offset) % CURVE_ORDER).toString(16);
}

function deriveAddressFromPrivateKey(privKeyHex) {
  const pubKey = ec.starkCurve.getStarkKey(privKeyHex);
  // Standard AA address computation — actual formula depends on which
  // account class hash you're deploying/using for the stealth address
  // (must match whatever the sender used when deriving the same address
  // to send funds to). Placeholder: swap in your account contract's
  // real address-from-pubkey calculation (calculateContractAddressFromHash
  // in starknet.js, with your chosen class hash + constructor calldata).
  return hash.calculateContractAddressFromHash(
    pubKey, // salt
    "0x...", // your stealth-account class hash
    CallData.compile({ publicKey: pubKey }),
    0
  );
}

// ---- Step 1: scan ----

$("scan-btn").addEventListener("click", async () => {
  const merchantAddress = $("merchant-address").value.trim();
  const viewingKey = $("viewing-key").value.trim();
  const status = $("scan-status");

  if (!merchantAddress || !viewingKey) {
    status.textContent = "Enter merchant address and viewing key.";
    return;
  }

  status.textContent = "Scanning...";

  try {
    const res = await fetch("/api/v1/stealth/scan", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ merchant_address: merchantAddress, viewing_key: viewingKey }),
    });

    if (!res.ok) {
      status.textContent = `Scan failed: ${res.status}`;
      return;
    }

    const data = await res.json();
    currentMatches = data.matches ?? [];
    renderMatches();
    status.textContent = `Found ${currentMatches.length} candidate payment(s).`;
  } catch (err) {
    status.textContent = `Scan error: ${err.message}`;
  }
});

function renderMatches() {
  const list = $("matches-list");
  list.innerHTML = "";

  currentMatches.forEach((m, i) => {
    const li = document.createElement("li");
    li.textContent = `${m.stealth_address} (block ${m.block_number})`;
    li.addEventListener("click", () => {
      document.querySelectorAll("#matches-list li").forEach((el) => el.classList.remove("selected"));
      li.classList.add("selected");
      selectedMatch = m;
      $("claim-btn").disabled = false;
    });
    list.appendChild(li);
  });
}

// ---- Step 2: claim (spend key stays local) ----

$("claim-btn").addEventListener("click", async () => {
  const spendKey = $("spend-key").value.trim();
  const destination = $("destination-address").value.trim();
  const status = $("claim-status");

  if (!spendKey || !destination || !selectedMatch) {
    status.textContent = "Select a match and fill in both key + destination.";
    return;
  }

  status.textContent = "Deriving stealth key locally...";

  try {
    // 1. Recompute the same shared secret used to derive this address,
    //    using the merchant's own spend key + the announcement's
    //    ephemeral pubkey (both are effectively public/semi-public
    //    inputs; only spendKey is sensitive, and it never leaves here).
    const sharedSecretHash = deriveSharedSecret(spendKey, selectedMatch.ephemeral_pubkey);
    const stealthPrivKey = deriveStealthPrivateKey(spendKey, sharedSecretHash);
    const derivedAddress = deriveAddressFromPrivateKey(stealthPrivKey);

    if (derivedAddress.toLowerCase() !== selectedMatch.stealth_address.toLowerCase()) {
      status.textContent = "Derived address doesn't match — check your spend key.";
      return;
    }

    status.textContent = "Submitting claim transaction...";

    // 2. Sign and submit DIRECTLY to the RPC provider — not through
    //    Beanie's API. Beanie never sees this transaction being
    //    constructed or the key that signed it.
    const stealthAccount = new Account(provider, derivedAddress, stealthPrivKey);

    const tx = await stealthAccount.execute({
      contractAddress: TOKEN_ADDRESS,
      entrypoint: "transfer",
      calldata: CallData.compile({
        recipient: destination,
        amount: { low: "0", high: "0" }, // fill with the actual balance — query balance_of first
      }),
    });

    status.textContent = `Claimed. Tx: ${tx.transaction_hash}`;
  } catch (err) {
    status.textContent = `Claim error: ${err.message}`;
  } finally {
    // Best-effort clear of the input — the underlying string may still
    // be retained by the JS engine until GC, which is a real limitation
    // of doing this in a browser tab rather than a dedicated signer; a
    // hardware wallet or a wallet-extension-mediated flow (session key
    // held by Ready, not typed into a page) is the safer long-term shape
    // for this claim step. This form-based version is a prototype.
    $("spend-key").value = "";
  }
});

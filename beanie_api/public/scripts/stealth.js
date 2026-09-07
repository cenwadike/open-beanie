// stealth.js
//
// Client-Side Deterministic Index Recovery & Claim for Starknet & EVM (Base)
// Scans USDC Transfer logs and counterfactual 2-of-2 account addresses
// using WebAuthn PRF master seed + index loop.
// Co-signing & Gasless Paymaster execution delegated to /api/v1/stealth/claim backend route.

import {
    RpcProvider as StarknetProvider,
    CallData,
    hash,
    ec as starkEc,
    uint256 as starkUint256,
    constants as starknetConstants,
} from "./starknet.js";

import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";

import {
    CHAINS,
    deriveDeterministicStealthKey,
    deriveEvmCreate2Salt,
    deriveStealthAccount,
} from "./stealth.core.js";

const UDC_ADDRESS = starknetConstants.UDC.ADDRESS;
const UDC_ENTRYPOINT = starknetConstants.UDC.ENTRYPOINT;
const $ = (id) => document.getElementById(id);

let currentMatches = [];
let selectedMatch = null;
let cachedCredentialId = null;
let historyFilterChain = "all";

// Chain SVG Icons
const chainIcons = {
    all: `🔗`,
    BASE: `<svg viewBox="0 0 42 42" width="24" height="24"><circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`,
    STARKNET: `<svg viewBox="0 0 42 42" width="24" height="24"><circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`,
};

const chainNames = {
    all: "All chains",
    base: "Base",
    starknet: "Starknet",
};

// ---- WebAuthn Helpers & Helper Utils ----

function bufferToBase64Url(buffer) {
    const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
    let s = "";
    for (let i = 0; i < bytes.byteLength; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base64UrlToBuffer(base64url) {
    if (!base64url) return new ArrayBuffer(0);
    const base64 = base64url.replace(/-/g, "+").replace(/_/g, "/");
    const padded = base64.padEnd(base64.length + ((4 - (base64.length % 4)) % 4), "=");
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
}

function prepareCreationOptions(resp) {
    const o = resp.publicKey; // unwrap webauthn-rs's CreationChallengeResponse envelope
    return {
        ...o,
        challenge: base64UrlToBuffer(o.challenge),
        user: { ...o.user, id: base64UrlToBuffer(o.user.id) },
        excludeCredentials: (o.excludeCredentials || []).map((c) => ({
            ...c,
            id: base64UrlToBuffer(c.id),
        })),
    };
}

function prepareRequestOptions(resp) {
    const o = resp.publicKey; // unwrap webauthn-rs's RequestChallengeResponse envelope
    return {
        ...o,
        challenge: base64UrlToBuffer(o.challenge),
        allowCredentials: (o.allowCredentials || []).map((c) => ({
            ...c,
            id: base64UrlToBuffer(c.id),
        })),
    };
}

function credentialToJSON(cred) {
    if (!cred) return null;
    const json = {
        id: cred.id,
        rawId: bufferToBase64Url(cred.rawId),
        type: cred.type,
        response: { clientDataJSON: bufferToBase64Url(cred.response.clientDataJSON) },
    };
    if (cred.response.attestationObject) {
        json.response.attestationObject = bufferToBase64Url(cred.response.attestationObject);
    }
    if (cred.response.authenticatorData) {
        json.response.authenticatorData = bufferToBase64Url(cred.response.authenticatorData);
        json.response.signature = bufferToBase64Url(cred.response.signature);
        if (cred.response.userHandle) {
            json.response.userHandle = bufferToBase64Url(cred.response.userHandle);
        }
    }
    return json;
}

async function getOrRegisterCredential(forceNew = false) {
    const storageKey = "beanie.passkey.cred.v1";

    if (!forceNew && cachedCredentialId) return cachedCredentialId;

    const storedId = !forceNew && localStorage.getItem(storageKey);
    if (storedId) {
        cachedCredentialId = storedId;
        return cachedCredentialId;
    }

    let credential;
    try {
        const startRes = await fetch("/api/v1/webauthn/register/start", { method: "POST" });
        if (!startRes.ok) throw new Error("Could not start passkey registration");

        const { session_token, options } = await startRes.json();

        const requestOptions = prepareCreationOptions(options);
        requestOptions.extensions = { prf: {} };

        credential = await navigator.credentials.create({ publicKey: requestOptions });

        const finishRes = await fetch("/api/v1/webauthn/register/finish", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ session_token, credential: credentialToJSON(credential) }),
        });
        if (!finishRes.ok) throw new Error("Passkey registration rejected");
        const { credential_id } = await finishRes.json();

        localStorage.setItem(storageKey, credential_id);
        cachedCredentialId = credential_id;
    } catch (err) {
        throw new Error(`Passkey initialization failed: ${err.message}`);
    }

    const prfResults = credential.getClientExtensionResults()?.prf;
    if (!prfResults?.enabled) {
        throw new Error("Device passkey does not support PRF extension.");
    }

    return cachedCredentialId;
}

async function getVerifiedToken(binding, { salt, maxUses = 1, forceNew = false } = {}) {
    const storageKey = "beanie.passkey.cred.v1";
    let credentialId = localStorage.getItem(storageKey);

    // Only invoke passkey registration if no credential ID exists yet
    if (!credentialId || forceNew) {
        credentialId = await getOrRegisterCredential(forceNew);
    }

    const startRes = await fetch("/api/v1/webauthn/auth/start", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ credential_id: credentialId, binding, max_uses: maxUses }),
    });

    if (startRes.status === 409) {
        localStorage.removeItem("beanie.passkey.cred.v1");
        cachedCredentialId = null;
        return getVerifiedToken(binding, { salt, maxUses, forceNew: true });
    }

    if (!startRes.ok) {
        const text = await startRes.text();
        throw new Error(`Server returned ${startRes.status}: ${text}`);
    }

    const { session_token, options } = await startRes.json();

    const requestOptions = prepareRequestOptions(options);
    if (salt) {
        requestOptions.extensions = { prf: { eval: { first: salt } } };
    }

    const assertion = await navigator.credentials.get({ publicKey: requestOptions });

    const finishRes = await fetch("/api/v1/webauthn/auth/finish", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ session_token, credential: credentialToJSON(assertion) }),
    });
    if (!finishRes.ok) throw new Error("Passkey verification rejected");
    const { verified_token } = await finishRes.json();

    const prfOutput = assertion.getClientExtensionResults()?.prf?.results?.first;
    return {
        verifiedToken: verified_token,
        prfOutput: prfOutput ? new Uint8Array(prfOutput) : null,
    };
}

async function deriveMasterSalt() {
    const data = new TextEncoder().encode("beanie-stealth-master-salt-v1");
    const digest = await crypto.subtle.digest("SHA-256", data);
    return new Uint8Array(digest);
}

// ---- Multi-Chain Selector UI Logic ----

function updateChainControl() {
    const icon = $("stealthChainIcon");
    const name = $("stealthChainName");
    if (icon) icon.innerHTML = chainIcons[historyFilterChain] || chainIcons.all;
    if (name) name.textContent = chainNames[historyFilterChain] || "All chains";
}

function bindChainMenu() {
    const selectBtn = $("stealthChainSelect");
    const menu = $("stealthChainMenu");

    selectBtn?.addEventListener("click", (e) => {
        e.stopPropagation();
        menu?.classList.toggle("open");
    });

    menu?.addEventListener("click", (e) => {
        const option = e.target.closest(".history-chain-option");
        if (!option) return;

        const selectedChain = option.dataset.chain;
        historyFilterChain = selectedChain;

        menu.querySelectorAll(".history-chain-option").forEach((opt) => {
            opt.setAttribute("aria-selected", opt.dataset.chain === selectedChain);
        });

        updateChainControl();
        menu.classList.remove("open");
        renderMatches();
    });

    document.addEventListener("click", (e) => {
        if (!e.target.closest("#stealthChainSelect") && !e.target.closest("#stealthChainMenu")) {
            menu?.classList.remove("open");
        }
    });
}

// ---- Scanning Logic ----

async function executeScan() {
    const status = $("scan-status");
    if (status) status.textContent = "Authenticating passkey...";

    try {
        const salt = await deriveMasterSalt();
        const { prfOutput: beanMasterSecret } = await getVerifiedToken("scan:all", { salt });
        if (!beanMasterSecret) throw new Error("Key derivation failed — no PRF output returned.");

        if (status) status.textContent = "Scanning accounts across chains...";

        const activeChains = ["starknet", "base"];
        const matches = [];

        for (const chainKey of activeChains) {
            const chainConfig = CHAINS[chainKey];
            if (!chainConfig) continue;

            let receivedAddresses = new Set();
            let starknetProvider = null;
            let evmProvider = null;

            if (chainConfig.type === "starknet") {
                starknetProvider = new StarknetProvider({ nodeUrl: chainConfig.rpcUrl });
                const transferSelector = hash.getSelectorFromName("Transfer");
                const latestBlock = await starknetProvider.getBlockNumber();

                const eventResponse = await starknetProvider.getEvents({
                    from_block: { block_number: 0 },
                    to_block: { block_number: latestBlock },
                    address: chainConfig.tokenAddress,
                    keys: [[transferSelector]],
                    chunk_size: 1000,
                });

                for (const ev of eventResponse.events) {
                    const toAddr = (ev.data[1] || ev.keys[2]).toLowerCase();
                    receivedAddresses.add(toAddr);
                }
            } else {
                evmProvider = new ethers.JsonRpcProvider(chainConfig.rpcUrl);
                const erc20Abi = ["event Transfer(address indexed from, address indexed to, uint256 value)"];
                const tokenContract = new ethers.Contract(chainConfig.tokenAddress, erc20Abi, evmProvider);

                const latestBlock = await evmProvider.getBlockNumber();
                const BLOCK_RANGE = 10; // Alchemy Free Tier limit
                const START_BLOCK = Math.max(0, latestBlock - 1000); // Or your deployment block

                let logs = [];
                for (let i = START_BLOCK; i <= latestBlock; i += BLOCK_RANGE) {
                    const toBlock = Math.min(i + BLOCK_RANGE - 1, latestBlock);
                    const chunkLogs = await tokenContract.queryFilter(
                        tokenContract.filters.Transfer(),
                        i,
                        toBlock
                    );
                    logs.push(...chunkLogs);
                }
                for (const log of logs) {
                    if (log.args && log.args.to) {
                        receivedAddresses.add(log.args.to.toLowerCase());
                    }
                }
            }

            const GAP_LIMIT = 5;
            let consecutiveEmpty = 0;
            let index = 0;

            while (consecutiveEmpty < GAP_LIMIT) {
                const { stealthPrivScalar, address: stealthAddress, clientPubKeyFelt, clientAddress } =
                    await deriveStealthAccount(beanMasterSecret, "default-lane", index, chainKey);

                let balance = 0n;
                let isDeployed = false;
                let constructorCalldata = [];

                if (chainConfig.type === "starknet") {
                    constructorCalldata = CallData.compile({
                        client_pubkey: clientPubKeyFelt,
                        cosigner_pubkey: chainConfig.litCosignerPubKey,
                    });

                    const formattedAddress = stealthAddress.toLowerCase();

                    try {
                        const classHash = await starknetProvider.getClassHashAt(stealthAddress);
                        isDeployed = classHash && classHash !== "0x0";
                    } catch {
                        isDeployed = false;
                    }

                    try {
                        const balanceResult = await starknetProvider.callContract({
                            contractAddress: chainConfig.tokenAddress,
                            entrypoint: "balance_of",
                            calldata: [stealthAddress],
                        });
                        balance = starkUint256.uint256ToBN({ low: balanceResult[0], high: balanceResult[1] });
                    } catch {
                        // Zero balance fallback
                    }

                    if (balance > 0n || receivedAddresses.has(formattedAddress)) {
                        matches.push({
                            index,
                            chainKey,
                            stealthAddress,
                            clientPubKeyFelt,
                            stealthPrivScalar,
                            balance,
                            isDeployed,
                            constructorCalldata,
                            classHash: chainConfig.stealthAccountClassHash,
                        });
                        consecutiveEmpty = 0;
                    } else {
                        consecutiveEmpty++;
                    }
                } else {
                    const formattedAddress = stealthAddress.toLowerCase();
                    const code = await evmProvider.getCode(stealthAddress);
                    isDeployed = code !== "0x" && code !== "0x0";

                    try {
                        const erc20Abi = ["function balanceOf(address account) view returns (uint256)"];
                        const tokenContract = new ethers.Contract(chainConfig.tokenAddress, erc20Abi, evmProvider);
                        balance = await tokenContract.balanceOf(stealthAddress);
                    } catch {
                        // Zero balance fallback
                    }

                    if (balance > 0n || receivedAddresses.has(formattedAddress)) {
                        matches.push({
                            index,
                            chainKey,
                            stealthAddress,
                            clientAddress,
                            stealthPrivScalar,
                            balance: BigInt(balance.toString()),
                            isDeployed,
                        });
                        consecutiveEmpty = 0;
                    } else {
                        consecutiveEmpty++;
                    }
                }

                index++;
            }
        }

        currentMatches = matches;
        updateBalances();
        renderMatches();
        if (status) status.textContent = "Scanning complete";
    } catch (err) {
        if (status) status.textContent = `Scan error: ${err.message}`;
    }
}

function updateBalances() {
    const filtered = currentMatches.filter(
        (m) => historyFilterChain === "all" || m.chainKey === historyFilterChain
    );

    const shieldedTotal = filtered.reduce((acc, curr) => acc + curr.balance, 0n);
    const formattedShielded = (Number(shieldedTotal) / 1e6).toFixed(2);

    const shieldedEl = $("shieldedBalance");
    if (shieldedEl) shieldedEl.textContent = `${formattedShielded} USDC`;

    const unshieldBtn = $("unshieldBtn");
    if (unshieldBtn) unshieldBtn.disabled = shieldedTotal === 0n;
}

function renderMatches() {
    const list = $("matches-list");
    if (!list) return;
    list.innerHTML = "";

    const filtered = currentMatches.filter(
        (m) => historyFilterChain === "all" || m.chainKey === historyFilterChain
    );

    if (filtered.length === 0) {
        const scopeLabel = historyFilterChain === "all" ? "all chains" : chainNames[historyFilterChain];
        list.innerHTML = `<div class="history-empty">History is empty — no deposits detected yet on ${scopeLabel}.</div>`;
        return;
    }

    filtered.forEach((m) => {
        const row = document.createElement("div");
        row.className = "history-row";

        const formattedBalance = (Number(m.balance) / 1e6).toFixed(2);
        const shortAddr = `${m.stealthAddress.slice(0, 6)}...${m.stealthAddress.slice(-4)}`;

        row.innerHTML = `
      <span class="mini-icon">${chainIcons[m.chainKey] || ""}</span>
      <span class="mono">${shortAddr}</span>
      <strong>${formattedBalance} USDC</strong>
    `;

        row.addEventListener("click", () => {
            document.querySelectorAll(".history-row").forEach((el) => el.classList.remove("selected"));
            row.classList.add("selected");
            selectedMatch = m;
            const unshieldBtn = $("unshieldBtn");
            if (unshieldBtn) unshieldBtn.disabled = false;
        });

        list.appendChild(row);
    });
}

// ---- Unshield / Claim Dispatch ----

$("unshieldBtn")?.addEventListener("click", async () => {
    const destInput = $("destination-address");
    const destination = destInput ? destInput.value.trim() : "";
    const status = $("scan-status");

    if (!selectedMatch) {
        if (currentMatches.length > 0) {
            selectedMatch = currentMatches[0];
        } else {
            if (status) status.textContent = "No shielded funds available to unshield.";
            return;
        }
    }

    if (!destination) {
        if (status) status.textContent = "Please specify a destination address.";
        return;
    }

    const chainConfig = CHAINS[selectedMatch.chainKey];
    let stealthPrivScalar = selectedMatch.stealthPrivScalar;

    try {
        if (status) status.textContent = "Authenticating WebAuthn session...";

        const chainEnumMap = { starknet: "Starknet", base: "Base", ethereum: "Ethereum" };
        const wireChain = chainEnumMap[selectedMatch.chainKey];

        const binding = `claim:${wireChain}:${selectedMatch.stealthAddress}:${destination}`;
        const { verifiedToken } = await getVerifiedToken(binding);

        if (status) status.textContent = "Constructing shielded transaction...";

        const stealthPrivKeyHex = "0x" + stealthPrivScalar.toString(16).padStart(64, "0");
        let callsPayload = [];
        let txHashToSign = "";

        if (chainConfig.type === "starknet") {
            if (!selectedMatch.isDeployed) {
                const udcCalldata = [
                    selectedMatch.classHash,
                    selectedMatch.clientPubKeyFelt,
                    "0x0",
                    selectedMatch.constructorCalldata.length.toString(),
                    ...selectedMatch.constructorCalldata,
                ];

                callsPayload.push({
                    contract_address: UDC_ADDRESS,
                    entrypoint: UDC_ENTRYPOINT,
                    calldata: udcCalldata,
                });
            }

            const sweepAmount = starkUint256.bnToUint256(selectedMatch.balance);

            const commitmentNote = hash.computeHashOnElements([
                selectedMatch.clientPubKeyFelt,
                selectedMatch.index.toString(),
                selectedMatch.balance.toString(),
            ]);

            const approveCalldata = CallData.compile({
                spender: chainConfig.shieldedPoolAddress,
                amount: sweepAmount,
            });
            callsPayload.push({
                contract_address: chainConfig.tokenAddress,
                entrypoint: "approve",
                calldata: approveCalldata,
            });

            const shieldCalldata = CallData.compile({
                token: chainConfig.tokenAddress,
                amount: sweepAmount,
                commitment: commitmentNote,
            });
            callsPayload.push({
                contract_address: chainConfig.shieldedPoolAddress,
                entrypoint: "shield_deposit",
                calldata: shieldCalldata,
            });

            txHashToSign = hash.calculateInvokeTransactionHash({
                senderAddress: selectedMatch.stealthAddress,
                calls: callsPayload,
                version: "0x1",
                maxFee: 0,
                chainId: starknetConstants.StarknetChainId.SN_MAIN,
                nonce: 0,
            });

            const clientSig = starkEc.starkCurve.sign(txHashToSign, stealthPrivKeyHex);
            const r1 = "0x" + clientSig.r.toString(16).padStart(64, "0");
            const s1 = "0x" + clientSig.s.toString(16).padStart(64, "0");

            if (status) status.textContent = "Queuing payload to worker pipeline...";

            const requestBody = {
                chain: wireChain,
                tx_hash: txHashToSign,
                derived_address: selectedMatch.stealthAddress,
                client_sig: { r1, s1 },
                verified_token: verifiedToken,
                calls: callsPayload,
            };

            const res = await fetch("/api/v1/stealth/claim", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(requestBody),
            });

            if (!res.ok) {
                const errData = await res.json().catch(() => ({}));
                throw new Error(errData.message || `Backend rejected execution: ${res.status}`);
            }

            const responseData = await res.json();
            if (status) status.textContent = `Unshielded successfully! Tx Hash: ${responseData.transaction_hash}`;
        } else {
            const erc20Interface = new ethers.Interface([
                "function transfer(address to, uint256 amount) returns (bool)",
            ]);
            const transferCalldata = erc20Interface.encodeFunctionData("transfer", [
                destination,
                selectedMatch.balance,
            ]);

            const formattedCalldata = "0x" + transferCalldata.replace(/^0x/, "");

            callsPayload.push({
                contract_address: chainConfig.tokenAddress,
                entrypoint: "transfer",
                calldata: [formattedCalldata],
            });

            txHashToSign = ethers.keccak256(
                ethers.SolidityPack(
                    ["address", "address", "uint256", "bytes"],
                    [selectedMatch.stealthAddress, chainConfig.tokenAddress, chainConfig.chainId, formattedCalldata]
                )
            );

            const signingWallet = new ethers.Wallet(stealthPrivKeyHex);
            const sigStruct = signingWallet.signingKey.sign(ethers.getBytes(txHashToSign));

            const requestBody = {
                chain: wireChain,
                tx_hash: txHashToSign,
                derived_address: selectedMatch.stealthAddress,
                client_sig: { r1: sigStruct.r, s1: sigStruct.s },
                verified_token: verifiedToken,
                calls: callsPayload,
            };

            const res = await fetch("/api/v1/stealth/claim", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(requestBody),
            });

            if (!res.ok) {
                const errData = await res.json().catch(() => ({}));
                throw new Error(errData.message || `Backend rejected execution: ${res.status}`);
            }

            const responseData = await res.json();
            if (status) status.textContent = `Claim queued! Tx Hash: ${responseData.transaction_hash}`;
        }
    } catch (err) {
        if (status) status.textContent = `Unshield failed: ${err.message}`;
    } finally {
        stealthPrivScalar = null;
    }
});

// ---- Initialization ----

document.addEventListener("DOMContentLoaded", () => {
    bindChainMenu();
    updateChainControl();
    executeScan();
});

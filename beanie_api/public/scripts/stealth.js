// stealth.js
//
// Client-Side Deterministic Index Recovery & Claim for Starknet & EVM (Base / Ethereum)
// Zero-State Recovery: Scans USDC Transfer logs and counterfactual 2-of-2 account addresses
// using WebAuthn PRF master seed + index loop.
// Co-signing & Gasless Paymaster execution delegated to /api/v1/stealth/claim backend route.
import {
    RpcProvider as StarknetProvider,
    CallData,
    hash,
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

// ---- Configuration ----
const RP_ID = window.location.hostname;
const UDC_ADDRESS = starknetConstants.UDC.ADDRESS;
const UDC_ENTRYPOINT = starknetConstants.UDC.ENTRYPOINT;

const $ = (id) => document.getElementById(id);

let currentMatches = [];
let selectedMatch = null;
let cachedCredentialId = null;

// ---- Helpers ----

function bytesToHex(bytes) {
    return Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
}

function bytesToScalar(bytes, curveOrder) {
    const n = BigInt("0x" + bytesToHex(bytes));
    return n % curveOrder;
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

function prepareCreationOptions(o) {
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

function prepareRequestOptions(o) {
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

// ---- WebAuthn PRF Key Derivation & Ceremonies ----

async function getOrRegisterCredential() {
    if (cachedCredentialId) return cachedCredentialId;

    const storageKey = "beanie.passkey.cred.v1";
    const storedId = localStorage.getItem(storageKey);
    if (storedId) {
        cachedCredentialId = storedId;
        return cachedCredentialId;
    }

    let credential;
    try {
        const startRes = await fetch("/api/v1/auth/register/start", { method: "POST" });
        if (!startRes.ok) throw new Error("Could not start passkey registration");
        const { session_token, options } = await startRes.json();

        const requestOptions = prepareCreationOptions(options);
        requestOptions.extensions = { prf: {} };

        credential = await navigator.credentials.create({
            publicKey: requestOptions,
        });

        const finishRes = await fetch("/api/v1/auth/register/finish", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ session_token, credential: credentialToJSON(credential) }),
        });
        if (!finishRes.ok) throw new Error("Passkey registration was rejected by the server");
        const { credential_id } = await finishRes.json();

        localStorage.setItem(storageKey, credential_id);
        cachedCredentialId = credential_id;
    } catch (err) {
        throw new Error(`Passkey initialization failed: ${err.message}`);
    }

    // Single point PRF support check
    const prfResults = credential.getClientExtensionResults()?.prf;
    if (!prfResults?.enabled) {
        throw new Error("This device's passkey does not support the PRF extension.");
    }

    return cachedCredentialId;
}

async function getVerifiedToken(binding, { salt } = {}) {
    const credentialId = await getOrRegisterCredential();

    const startRes = await fetch("/api/v1/auth/start", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ credential_id: credentialId, binding }),
    });
    if (!startRes.ok) throw new Error("Could not start passkey verification");
    const { session_token, options } = await startRes.json();

    const requestOptions = prepareRequestOptions(options);
    if (salt) {
        requestOptions.extensions = { prf: { eval: { first: salt } } };
    }

    const assertion = await navigator.credentials.get({ publicKey: requestOptions });

    const finishRes = await fetch("/api/v1/auth/finish", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ session_token, credential: credentialToJSON(assertion) }),
    });
    if (!finishRes.ok) throw new Error("Passkey verification was rejected by the server");
    const { verified_token } = await finishRes.json();

    const prfOutput = assertion.getClientExtensionResults()?.prf?.results?.first;
    return {
        verifiedToken: verified_token,
        prfOutput: prfOutput ? new Uint8Array(prfOutput) : null,
    };
}

async function deriveLaneSalt(laneId) {
    const data = new TextEncoder().encode(`beanie-stealth-salt-v1:${laneId}`);
    const digest = await crypto.subtle.digest("SHA-256", data);
    return new Uint8Array(digest);
}

async function hkdf(ikm, info) {
    const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
    const bits = await crypto.subtle.deriveBits(
        { name: "HKDF", hash: "SHA-256", salt: new Uint8Array(32), info: new TextEncoder().encode(info) },
        key,
        256
    );
    return new Uint8Array(bits);
}

// ---- Key & Address Derivation ----

async function deriveDeterministicStealthKey(beanMasterSecret, laneId, index, chainKey) {
    const chainConfig = CHAINS[chainKey];
    const curveOrder = chainConfig.type === "starknet" ? STARK_CURVE_ORDER : SECP256K1_ORDER;
    const spendMasterPriv = await hkdf(beanMasterSecret, `spend-v1:${chainKey}`);
    const spendMasterScalar = bytesToScalar(spendMasterPriv, curveOrder);
    const indexBytes = await hkdf(beanMasterSecret, `beanie-lane-index-v1:${laneId}:${chainKey}:${index}`);
    const indexScalar = bytesToScalar(indexBytes, curveOrder);
    return (spendMasterScalar + indexScalar) % curveOrder;
}

async function deriveEvmCreate2Salt(beanMasterSecret, laneId, index, chainKey) {
    const saltBytes = await hkdf(beanMasterSecret, `evm-create2-salt-v1:${laneId}:${chainKey}:${index}`);
    return "0x" + bytesToHex(saltBytes);
}

function deriveStarknetStealthAddress(clientPubKeyFelt, cosignerPubKeyFelt, classHash) {
    return hash.calculateContractAddressFromHash(
        clientPubKeyFelt,
        classHash,
        CallData.compile({
            client_pubkey: clientPubKeyFelt,
            cosigner_pubkey: cosignerPubKeyFelt,
        }),
        0
    );
}

function deriveEvmStealthAddress(clientAddress, cosignerAddress, chainConfig, saltHex) {
    const abiCoder = ethers.AbiCoder.defaultAbiCoder();
    const constructorArgs = abiCoder.encode(
        ["address", "address", "address"],
        [chainConfig.entryPointAddress, clientAddress, cosignerAddress]
    );
    const salt = ethers.keccak256(saltHex);
    const initCodeHash = ethers.keccak256(ethers.concat([chainConfig.byteCodeHash, constructorArgs]));
    return ethers.getCreate2Address(chainConfig.factoryAddress, salt, initCodeHash);
}

// ---- Step 1: Scan Payments ----

$("scan-btn")?.addEventListener("click", async () => {
    const laneId = $("lane-id").value.trim();
    const chainKey = $("chain-select")?.value || "starknet";
    const status = $("scan-status");
    const onboardingBlock = parseInt($("onboarding-block")?.value || "0", 10);

    const chainConfig = CHAINS[chainKey];
    if (!chainConfig || !laneId) {
        status.textContent = "Please select a valid chain and specify a Lane ID.";
        return;
    }

    status.textContent = "Authenticating passkey...";

    try {
        const salt = await deriveLaneSalt(laneId);
        const { prfOutput: beanMasterSecret } = await getVerifiedToken(`scan:${laneId}`, { salt });
        if (!beanMasterSecret) throw new Error("Key derivation failed — no PRF output returned.");

        status.textContent = `Scanning logs on ${chainKey.toUpperCase()}...`;

        let receivedAddresses = new Set();
        let starknetProvider = null;
        let evmProvider = null;

        if (chainConfig.type === "starknet") {
            starknetProvider = new StarknetProvider({ nodeUrl: chainConfig.rpcUrl });
            const transferSelector = hash.getSelectorFromName("Transfer");
            const latestBlock = await starknetProvider.getBlockNumber();

            const eventResponse = await starknetProvider.getEvents({
                from_block: { block_number: onboardingBlock },
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
            const logs = await tokenContract.queryFilter(tokenContract.filters.Transfer(), onboardingBlock, "latest");

            for (const log of logs) {
                if (log.args && log.args.to) {
                    receivedAddresses.add(log.args.to.toLowerCase());
                }
            }
        }

        status.textContent = "Evaluating deterministic stealth accounts...";

        const GAP_LIMIT = 10;
        let consecutiveEmpty = 0;
        let index = 0;
        const matches = [];

        while (consecutiveEmpty < GAP_LIMIT) {
            const { stealthPrivScalar, address: stealthAddress, clientPubKeyFelt, clientAddress } =
                await deriveStealthAccount(beanMasterSecret, laneId, index, chainKey);

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
                    // Zero balance
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
                    // Zero balance
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

        currentMatches = matches;
        renderMatches();
        status.textContent = `Scan complete. Found ${currentMatches.length} lane(s).`;
    } catch (err) {
        status.textContent = `Scan failed: ${err.message}`;
    }
});

function renderMatches() {
    const list = $("matches-list");
    list.innerHTML = "";

    currentMatches.forEach((m) => {
        const li = document.createElement("li");
        const formattedBalance = (Number(m.balance) / 1e6).toFixed(2);
        const deployLabel = m.isDeployed ? "" : " [Conditional JIT Deploy]";
        li.textContent = `[${m.chainKey.toUpperCase()}] Index #${m.index}: ${m.stealthAddress.slice(0, 10)}... (${formattedBalance} USDC)${deployLabel}`;

        li.addEventListener("click", () => {
            document.querySelectorAll("#matches-list li").forEach((el) => el.classList.remove("selected"));
            li.classList.add("selected");
            selectedMatch = m;
            $("claim-btn").disabled = false;
        });
        list.appendChild(li);
    });
}

// ---- Step 2: Atomic Execution Claim Dispatch ----

$("claim-btn")?.addEventListener("click", async () => {
    const destination = $("destination-address").value.trim();
    const status = $("claim-status");

    if (!destination || !selectedMatch) {
        status.textContent = "Destination address and lane selection required.";
        return;
    }

    const chainConfig = CHAINS[selectedMatch.chainKey];
    let stealthPrivScalar = selectedMatch.stealthPrivScalar;

    try {
        status.textContent = "Authenticating WebAuthn session...";

        const chainEnumMap = {
            starknet: "Starknet",
            base: "Base",
            ethereum: "Ethereum",
        };
        const wireChain = chainEnumMap[selectedMatch.chainKey];

        const binding = `claim:${wireChain}:${selectedMatch.stealthAddress}:${destination}`;
        const { verifiedToken } = await getVerifiedToken(binding);

        status.textContent = "Constructing stealth transfer transaction...";

        const stealthPrivKeyHex = "0x" + stealthPrivScalar.toString(16).padStart(64, "0");
        let callsPayload = [];
        let txHashToSign = "";

        if (chainConfig.type === "starknet") {
            // Conditionally prepend UDC JIT deployment call
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

            // Append sweep call
            const sweepAmount = starkUint256.bnToUint256(selectedMatch.balance);
            const sweepCalldata = CallData.compile({
                recipient: destination,
                amount: sweepAmount,
            });

            callsPayload.push({
                contract_address: chainConfig.tokenAddress,
                entrypoint: "transfer",
                calldata: sweepCalldata,
            });

            // Standard Starknet Invoke Transaction Hash
            txHashToSign = hash.calculateInvokeTransactionHash({
                senderAddress: selectedMatch.stealthAddress,
                calls: callsPayload,
                version: "0x1",
                maxFee: 0,
                chainId: starknetConstants.StarknetChainId.SN_MAIN,
                nonce: 0,
            });

            // Sign locally using Stark key
            const clientSig = starkEc.starkCurve.sign(txHashToSign, stealthPrivKeyHex);
            const r1 = "0x" + clientSig.r.toString(16).padStart(64, "0");
            const s1 = "0x" + clientSig.s.toString(16).padStart(64, "0");

            status.textContent = "Queuing payload to Axum worker pipeline...";

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
                headers: {
                    "Content-Type": "application/json",
                },
                body: JSON.stringify(requestBody),
            });

            if (!res.ok) {
                const errData = await res.json().catch(() => ({}));
                throw new Error(errData.message || `Backend rejected execution: ${res.status}`);
            }

            const responseData = await res.json();
            status.textContent = `Claim successfully queued! Transaction Hash: ${responseData.transaction_hash}`;
        } else {
            // EVM Path
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

            // Included chainId in the EVM signed-hash construction
            txHashToSign = ethers.keccak256(
                ethers.SolidityPack(
                    ["address", "address", "uint256", "bytes"],
                    [
                        selectedMatch.stealthAddress,
                        chainConfig.tokenAddress,
                        chainConfig.chainId,
                        formattedCalldata,
                    ]
                )
            );

            const signingWallet = new ethers.Wallet(stealthPrivKeyHex);
            const sigStruct = signingWallet.signingKey.sign(ethers.getBytes(txHashToSign));

            const r1 = sigStruct.r;
            const s1 = sigStruct.s;

            status.textContent = "Queuing payload to Axum worker pipeline...";

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
                headers: {
                    "Content-Type": "application/json",
                },
                body: JSON.stringify(requestBody),
            });

            if (!res.ok) {
                const errData = await res.json().catch(() => ({}));
                throw new Error(errData.message || `Backend rejected execution: ${res.status}`);
            }

            const responseData = await res.json();
            status.textContent = `Claim successfully queued! Transaction Hash: ${responseData.transaction_hash}`;
        }
    } catch (err) {
        status.textContent = `Claim dispatch failed: ${err.message}`;
    } finally {
        stealthPrivScalar = null;
    }
});

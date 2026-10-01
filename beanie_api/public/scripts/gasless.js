// gasless.js
//
// Client-side signers for the gasless /pay flow. Payload shapes follow the
// backend docs exactly:
//   evm      { kind, from, to, value, validAfter, validBefore, nonce, signature }
//   starknet { kind, outsideExecution, signature[], userAddress }
//   solana   { kind, message (base64), signature (base64), owner }
//
// The wallet signs an authorization; the keeper pays gas. Nothing here sends a
// transaction from the payer's wallet.

import { CHAINS, KEEPER_STARKNET_ADDRESS, SOLANA_RECEIVER_KIND, canGasless } from "./chains.js";
import { headBlock, rpc, rpcCall, scanDeposits, tokenBalance } from "./onchain.js";
import {
    buildTransferCheckedMessage, bytesEqual, parseTransaction, pubkeyBytes, toBase64, unsignedTransaction, usdcAta,
} from "./solana.js";

const VALID_FOR_SEC = 600; // backend allows up to 3600
const SNIP9_V1_INTERFACE_ID = "0x68cfd18b92d1907b8ba3cc324900277f5a3622099431ea85dd8089255e4181";
const SNIP9_V2_INTERFACE_ID = "0x1d1144bb2138366ff28d8e9ab57456b1d332ac42196230c3a602003c89872";

const randomHex = (bytes) =>
    "0x" + Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (b) => b.toString(16).padStart(2, "0")).join("");

// ---- EVM: EIP-3009 transferWithAuthorization -------------------------------

async function ensureEvmChain(provider, chain) {
    const hex = `0x${chain.chainId.toString(16)}`;
    try {
        await provider.request({ method: "wallet_switchEthereumChain", params: [{ chainId: hex }] });
    } catch (err) {
        const code = err?.code ?? err?.data?.originalError?.code;
        if (code !== 4902) throw err; // 4902 = chain unknown to the wallet
        await provider.request({
            method: "wallet_addEthereumChain",
            params: [
                {
                    chainId: hex,
                    chainName: chain.name,
                    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
                    rpcUrls: [chain.rpc],
                    blockExplorerUrls: chain.explorerBase ? [chain.explorerBase] : [],
                },
            ],
        });
    }
    const current = await provider.request({ method: "eth_chainId" });
    if (parseInt(current, 16) !== chain.chainId) throw new Error(`Switch your wallet to ${chain.name} and try again.`);
}

export async function signEvmTransfer({ chainKey, receiver, amountRaw }) {
    const chain = CHAINS[chainKey];
    if (!chain || !canGasless(chain)) throw new Error("Gasless payments are not available on this network.");
    const provider = window.ethereum;
    if (!provider) throw new Error("No EVM wallet detected.");

    const [from] = await provider.request({ method: "eth_requestAccounts" });
    if (!from) throw new Error("No wallet account available.");
    await ensureEvmChain(provider, chain);

    const now = Math.floor(Date.now() / 1000);
    const message = {
        from,
        to: receiver,
        value: String(amountRaw),
        validAfter: 0,
        validBefore: now + VALID_FOR_SEC,
        nonce: randomHex(32),
    };

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
        domain: { ...chain.eip712, chainId: chain.chainId, verifyingContract: chain.usdc },
        primaryType: "TransferWithAuthorization",
        message,
    };

    const signature = await provider.request({
        method: "eth_signTypedData_v4",
        params: [from, JSON.stringify(typedData)],
    });

    return { payload: { kind: "evm", ...message, signature }, from };
}

// ---- Starknet: SNIP-9 outside execution ------------------------------------

const TYPES_REV0 = {
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

const TYPES_REV1 = {
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

function buildTypedData(exec, chainId, version) {
    if (version === "2") {
        return {
            types: TYPES_REV1,
            primaryType: "OutsideExecution",
            domain: { name: "Account.execute_from_outside", version: "2", chainId, revision: "1" },
            message: {
                Caller: exec.caller,
                Nonce: exec.nonce,
                "Execute After": exec.execute_after,
                "Execute Before": exec.execute_before,
                Calls: exec.calls.map((c) => ({ To: c.to, Selector: c.selector, Calldata: c.calldata })),
            },
        };
    }
    return {
        types: TYPES_REV0,
        primaryType: "OutsideExecution",
        domain: { name: "Account.execute_from_outside", version: "1", chainId },
        message: {
            ...exec,
            calls_len: exec.calls.length,
            calls: exec.calls.map((c) => ({ ...c, calldata_len: c.calldata.length })),
        },
    };
}

async function supportsInterface(address, interfaceId, hash) {
    const out = await rpcCall(CHAINS.STARKNET.rpc, "starknet_call", [
        {
            contract_address: address,
            entry_point_selector: hash.getSelectorFromName("supports_interface"),
            calldata: [interfaceId],
        },
        "latest",
    ]);
    return Array.isArray(out) && out.length > 0 && BigInt(out[0]) !== 0n;
}

// Reads the account over the public RPC (not the wallet), so it does not
// depend on what the wallet extension exposes.
async function detectSnip9Version(address, hash) {
    let reachable = false;
    try {
        if (await supportsInterface(address, SNIP9_V2_INTERFACE_ID, hash)) return "2";
        reachable = true;
    } catch {
        /* fall through */
    }
    try {
        if (await supportsInterface(address, SNIP9_V1_INTERFACE_ID, hash)) return "1";
        reachable = true;
    } catch {
        /* fall through */
    }
    if (reachable) throw new Error("This Starknet account does not support gasless (SNIP-9) transfers.");
    return "2"; // RPC unreachable: current Argent/Braavos default
}

const toHexFelt = (v) => `0x${BigInt(v).toString(16)}`;

export async function signStarknetTransfer({ receiver, amountRaw }) {
    const chain = CHAINS.STARKNET;
    if (!canGasless(chain)) throw new Error("Gasless payments are not available on Starknet.");

    const { CallData, validateAndParseAddress, cairo, hash } = await import("./starknet.js");
    const wallet = window.starknet_argentX || window.starknet_braavos || window.starknet;
    if (!wallet) throw new Error("No Starknet wallet detected.");
    if (!wallet.isConnected) await wallet.enable();

    const account = wallet.account;
    const rawUser = wallet.selectedAddress || account?.address;
    if (!rawUser) throw new Error("Could not read your Starknet account.");
    if (typeof account?.signMessage !== "function") throw new Error("This wallet cannot sign SNIP-9 messages.");

    const userAddress = validateAndParseAddress(rawUser);
    const caller = validateAndParseAddress(KEEPER_STARKNET_ADDRESS);

    let chainId = "0x534e5f4d41494e"; // SN_MAIN
    try {
        let reported = null;
        if (typeof account.getChainId === "function") reported = await account.getChainId();
        else if (typeof wallet.provider?.getChainId === "function") reported = await wallet.provider.getChainId();
        else if (wallet.chainId) reported = wallet.chainId;
        if (reported) chainId = String(reported);
    } catch {
        /* keep mainnet default */
    }
    if (!/^(SN_MAIN|0x534e5f4d41494e)$/i.test(chainId)) {
        throw new Error("Switch your Starknet wallet to mainnet and try again.");
    }

    const version = await detectSnip9Version(userAddress, hash);

    const calldata = CallData.compile({
        recipient: validateAndParseAddress(receiver),
        amount: cairo.uint256(amountRaw),
    });
    const now = Math.floor(Date.now() / 1000);
    const outsideExecution = {
        caller,
        nonce: randomHex(31), // < 2^248, inside the felt range
        execute_after: now - 60,
        execute_before: now + VALID_FOR_SEC,
        calls: [
            {
                to: validateAndParseAddress(chain.usdc),
                selector: hash.getSelectorFromName("transfer"),
                calldata,
            },
        ],
    };

    const signed = await account.signMessage(buildTypedData(outsideExecution, chainId, version));
    const signature = Array.isArray(signed) ? signed.map(String) : [toHexFelt(signed.r), toHexFelt(signed.s)];

    return {
        payload: { kind: "starknet", outsideExecution, signature, userAddress },
        from: userAddress,
        version,
    };
}
// ---- Solana: signed legacy Message (fee payer = keeper) ---------------------
//
// The payer signs ONE SPL TransferChecked; the keeper is fee payer, adds its own
// signature and submits. Wallets are found through the Wallet Standard
// (Phantom, Solflare, Backpack...), so no wallet SDK is bundled.

const solanaWallets = [];
function initWalletStandard() {
    if (typeof window === "undefined") return;
    const register = (...wallets) => {
        for (const w of wallets) if (!solanaWallets.includes(w)) solanaWallets.push(w);
        return () => { };
    };
    const api = Object.freeze({ register, get: () => solanaWallets.slice(), on: () => () => { } });
    try {
        window.addEventListener("wallet-standard:register-wallet", ({ detail }) => detail(api)); // wallets that load later
        window.dispatchEvent(new CustomEvent("wallet-standard:app-ready", { detail: api })); // wallets already loaded
    } catch {
        /* no wallet support in this environment */
    }
}
initWalletStandard(); // at import time, before a wallet can have finished announcing itself

const supportsSolanaPay = (w, chain) =>
    Boolean(w.features?.["solana:signTransaction"] && w.features?.["standard:connect"]) &&
    (w.chains || []).includes(chain.walletChain);

async function connectSolanaWallet(chain) {
    const wallet = solanaWallets.find((w) => supportsSolanaPay(w, chain));
    if (!wallet) throw new Error("No Solana wallet detected.");
    if (!wallet.accounts?.length) await wallet.features["standard:connect"].connect();
    const account = wallet.accounts?.find((a) => (a.chains || []).includes(chain.walletChain)) || wallet.accounts?.[0];
    if (!account) throw new Error("No Solana account available.");
    return { wallet, account };
}

export async function signSolanaTransfer({ receiver, amountRaw }) {
    const chain = CHAINS.SOLANA;
    if (!canGasless(chain)) throw new Error("Gasless payments are not available on Solana.");
    pubkeyBytes(chain.keeper, "keeper address"); // misconfiguration fails here, not in the wallet

    const { wallet, account } = await connectSolanaWallet(chain);
    const owner = account.address;

    if ((await tokenBalance(chain.key, owner)) < BigInt(amountRaw)) {
        throw new Error("This Solana wallet does not hold enough USDC.");
    }

    const source = await usdcAta(owner, chain.usdc);
    const destination = SOLANA_RECEIVER_KIND === "token-account" ? receiver : await usdcAta(receiver, chain.usdc);
    const { value } = await rpc(chain.key, "getLatestBlockhash", [{ commitment: "confirmed" }]);
    const fromSlot = Math.max(0, (await headBlock(chain.key).catch(() => 0)) - 5);

    const message = buildTransferCheckedMessage({
        feePayer: chain.keeper,
        owner,
        source,
        destination,
        mint: chain.usdc,
        amount: amountRaw,
        decimals: 6,
        blockhash: value.blockhash,
    });

    const [out] = await wallet.features["solana:signTransaction"].signTransaction({
        account,
        transaction: unsignedTransaction(message),
        chain: chain.walletChain,
    });

    const signed = parseTransaction(out.signedTransaction);
    // The backend accepts exactly one SPL transfer. A wallet that rewrites the
    // transaction (some inject guard instructions) would get a 400 there anyway.
    if (!bytesEqual(signed.message, message)) {
        throw new Error("Your wallet modified the transaction, so it cannot be relayed. Try another Solana wallet.");
    }
    const signature = signed.signatures[1]; // slot 0 is the keeper's, slot 1 the owner's
    if (!signature || signature.every((b) => b === 0)) throw new Error("The wallet did not sign the transaction.");

    return {
        payload: { kind: "solana", message: toBase64(message), signature: toBase64(signature), owner },
        from: owner,
        // /pay wants the SPL transfer's DESTINATION here: the payment worker unpacks
        // receiver_address as a token account and reads its owner from it.
        receiverAddress: destination,
        settlement: { receiver, owner, amountRaw: String(amountRaw), fromSlot, lastValidBlockHeight: value.lastValidBlockHeight },
    };
}

const SETTLE_POLL_MS = 6000;
const SETTLE_MAX_POLLS = 40; // ~4 min, far beyond a blockhash's life
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * The API only says "queued", and a Solana blockhash lives ~60-90 s, so a slow
 * or failed relay would otherwise be silent. Watches for this payment's
 * transfer (same amount, authorised by the payer) and gives up only once the
 * blockhash can no longer land.
 *   "confirmed" : the transfer is finalized on-chain
 *   "expired"   : the blockhash expired and no transfer landed; nothing moved, safe to retry
 *   "unknown"   : could not tell (RPC trouble); do not retry blindly
 */
export async function awaitSolanaSettlement({ receiver, owner, amountRaw, fromSlot, lastValidBlockHeight }) {
    let cursor = fromSlot;
    for (let i = 0; i < SETTLE_MAX_POLLS; i++) {
        await sleep(SETTLE_POLL_MS);
        try {
            // Height first, scan after: a transfer that landed before expiry is
            // finalized by the time the height passes, so the scan below sees it.
            const height = Number(await rpc("SOLANA", "getBlockHeight", [{ commitment: "finalized" }]));
            const { deposits, nextBlock } = await scanDeposits("SOLANA", receiver, cursor);
            if (deposits.some((d) => d.amount === amountRaw && d.from === owner)) return "confirmed";
            cursor = nextBlock;
            if (height > lastValidBlockHeight) return "expired";
        } catch (e) {
            console.warn("[solana] settlement check:", e?.message || e);
        }
    }
    return "unknown";
}
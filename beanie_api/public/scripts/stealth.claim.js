// Claim payload builders. Derivation stays in stealth.core.js; this module only
// constructs and signs the family-specific transaction the backend will relay.

import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";
import { ed25519 } from "https://esm.sh/@noble/curves@1.8.2/ed25519?bundle";
import { CallData, ec as starkEc, hash } from "./starknet.js";
import { canonicalAddress } from "./identity.js";
import { USDC_DECIMALS } from "./chains.js";
import { contractExists, rpc, starknetAccountNonce } from "./onchain.js";
import { concatBytes, pubkeyBytes, TOKEN_PROGRAM, usdcAta } from "./solana.js";

const EVM_AUTH_TYPES = {
    TransferWithAuthorization: [
        { name: "from", type: "address" },
        { name: "to", type: "address" },
        { name: "value", type: "uint256" },
        { name: "validAfter", type: "uint256" },
        { name: "validBefore", type: "uint256" },
        { name: "nonce", type: "bytes32" },
    ],
};
const ZERO_EVM = `0x${"0".repeat(40)}`;
const STARK_PRIME = (1n << 251n) + 17n * (1n << 192n) + 1n;

const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
const felt = (value) => {
    const n = BigInt(value);
    if (n < 0n || n >= STARK_PRIME) throw new Error("Starknet value is outside the felt range.");
    return `0x${n.toString(16)}`;
};
const shortvec = (value) => {
    const out = [];
    let n = value;
    while (true) {
        const byte = n & 0x7f;
        n >>>= 7;
        if (n) out.push(byte | 0x80);
        else return Uint8Array.from([...out, byte]);
    }
};

function assertEvmClaim({ signer, destination, balance, config }) {
    const to = canonicalAddress("BASE", destination);
    if (!to || to.toLowerCase() === ZERO_EVM) throw new Error("Enter a valid, non-zero EVM destination.");
    if (to.toLowerCase() === signer.address.toLowerCase()) throw new Error("The destination cannot be the stealth account.");
    if (balance <= 0n) throw new Error("Nothing to claim.");
    if (!config?.usdc || !config?.chainId || !config?.eip712?.name || !config?.eip712?.version) {
        throw new Error("EVM USDC or EIP-712 domain configuration is incomplete.");
    }
    const window = Number(config.maxAuthWindowSecs ?? 86400);
    if (!Number.isSafeInteger(window) || window <= 120) throw new Error("The EVM authorization window is invalid.");
    return to;
}

export async function buildEvmClaim({ signer, destination, balance, config }) {
    const to = assertEvmClaim({ signer, destination, balance, config });
    const now = BigInt(Math.floor(Date.now() / 1000));
    const validBefore = now + 15n * 60n;
    if (validBefore - now <= 120n || validBefore - now > BigInt(config.maxAuthWindowSecs ?? 86400)) {
        throw new Error("The EVM authorization expiry is outside the permitted window.");
    }

    const nonce = ethers.hexlify(ethers.randomBytes(32));
    const auth3009 = {
        client: signer.clientAddress,
        to,
        value: balance.toString(),
        valid_after: "0",
        valid_before: validBefore.toString(),
        nonce,
        salt: signer.salt,
    };
    const typedValue = {
        from: signer.address,
        to,
        value: balance.toString(),
        validAfter: "0",
        validBefore: validBefore.toString(),
        nonce,
    };
    const domain = {
        name: config.eip712.name,
        version: config.eip712.version,
        chainId: Number(config.chainId),
        verifyingContract: config.usdc,
    };
    const txHash = ethers.TypedDataEncoder.hash(domain, EVM_AUTH_TYPES, typedValue);
    const wallet = new ethers.Wallet(signer.privateKey);
    const signature = ethers.Signature.from(await wallet.signMessage(ethers.getBytes(txHash)));

    return {
        txHash,
        derivedAddress: signer.address,
        clientSig: { r1: signature.r, s1: signature.s, v: signature.v },
        calls: [],
        auth3009,
    };
}

function checkedStarknetBounds(config) {
    const names = ["l1_gas", "l2_gas", "l1_data_gas"];
    const bounds = config?.resourceBounds;
    if (!bounds || names.some((name) => !bounds[name])) throw new Error("Starknet resource bounds are missing.");
    let maximum = 0n;
    for (const name of names) {
        const amount = BigInt(bounds[name].max_amount);
        const price = BigInt(bounds[name].max_price_per_unit);
        if (amount <= 0n || price <= 0n) throw new Error(`Starknet ${name} bounds must be positive.`);
        maximum += amount * price;
    }
    if (maximum > BigInt(config.maxFeeFri)) throw new Error("Starknet resource bounds exceed the 0.2 STRK fee cap.");
    return bounds;
}

export async function buildStarknetClaim({ signer, destination, balance, config }) {
    const to = canonicalAddress("STARKNET", destination);
    if (!to) throw new Error("Enter a valid Starknet destination address.");
    if (balance <= 0n) throw new Error("Nothing to claim.");
    if (!config?.usdc || !config?.classHash || !config?.cosigner || !config?.chainId) {
        throw new Error("Starknet account configuration is incomplete.");
    }
    const resourceBounds = checkedStarknetBounds(config);
    if (signer.address === to) throw new Error("The destination cannot be the stealth account.");

    const nonce = await starknetAccountNonce("STARKNET", signer.address);
    const amountLow = balance & ((1n << 128n) - 1n);
    const amountHigh = balance >> 128n;
    const transferData = [to, felt(amountLow), felt(amountHigh)];
    const selector = hash.getSelectorFromName("transfer");
    const calls = [{
        contract_address: config.usdc,
        entrypoint: "transfer",
        calldata: transferData,
    }];
    const calldata = [
        felt(1),
        felt(config.usdc),
        felt(selector),
        felt(transferData.length),
        ...transferData,
    ];
    const calldataFromHelper = CallData.compile(calldata);
    const tip = "0";
    const txHash = hash.calculateInvokeTransactionHash({
        senderAddress: signer.address,
        version: "0x3",
        compiledCalldata: calldataFromHelper,
        chainId: config.chainId,
        nonce: nonce.toString(),
        tip,
        paymasterData: [],
        accountDeploymentData: [],
        nonceDataAvailabilityMode: 0,
        feeDataAvailabilityMode: 0,
        resourceBounds,
    });
    const signature = starkEc.starkCurve.sign(txHash, signer.privateKey);
    const clientSig = { r1: felt(signature.r), s1: felt(signature.s) };

    return {
        txHash: felt(txHash),
        derivedAddress: signer.address,
        clientSig,
        calls,
        starknet: {
            client_pubkey: signer.publicKey,
            deploy_salt: signer.salt,
            nonce: nonce.toString(),
            tip,
            l1_gas: resourceBounds.l1_gas,
            l2_gas: resourceBounds.l2_gas,
            l1_data_gas: resourceBounds.l1_data_gas,
        },
    };
}

function u64le(value) {
    let n = BigInt(value);
    if (n < 0n || n >= 1n << 64n) throw new Error("Solana transfer amount is out of range.");
    const out = new Uint8Array(8);
    for (let i = 0; i < out.length; i++) {
        out[i] = Number(n & 0xffn);
        n >>= 8n;
    }
    return out;
}

function buildSolanaMessage({ relayer, client, cosigner, source, destination, mint, amount, blockhash, authority }) {
    const accountBytes = [relayer, client, cosigner, source, destination, mint, TOKEN_PROGRAM];
    accountBytes.push(authority);
    if (new Set(accountBytes).size !== 8) throw new Error("Solana transfer accounts must be distinct.");
    const rawKeys = accountBytes.map((key, i) => pubkeyBytes(key, `account ${i}`));
    const ixData = concatBytes(Uint8Array.of(12), u64le(amount), Uint8Array.of(USDC_DECIMALS));
    return concatBytes(
        Uint8Array.of(3, 2, 3),
        shortvec(rawKeys.length),
        ...rawKeys,
        pubkeyBytes(blockhash, "blockhash"),
        shortvec(1),
        Uint8Array.of(6),
        shortvec(6),
        Uint8Array.of(3, 5, 4, 7, 1, 2),
        shortvec(ixData.length),
        ixData,
    );
}

async function sha256(bytes) {
    return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

export async function buildSolanaClaim({ signer, destination, balance, config }) {
    const dest = canonicalAddress("SOLANA", destination);
    if (!dest) throw new Error("Enter a valid Solana destination address.");
    if (dest === signer.address) throw new Error("The destination cannot be the private-lane account.");
    if (balance <= 0n) throw new Error("Nothing to claim.");
    if (balance >= 1n << 64n) throw new Error("The Solana token amount exceeds the SPL u64 limit.");
    if (!config?.cosigner || !config?.relayer || !config?.usdc) throw new Error("Solana stealth configuration is incomplete.");

    const source = await usdcAta(signer.address, config.usdc);
    const destinationAta = await usdcAta(dest, config.usdc);
    if (!(await contractExists("SOLANA", source))) throw new Error("The private lane's USDC token account is not initialized yet.");
    if (!(await contractExists("SOLANA", destinationAta))) throw new Error("The destination has no USDC token account. Create it before claiming.");

    const latest = await rpc("SOLANA", "getLatestBlockhash", [{ commitment: "confirmed" }]);
    const blockhash = latest?.value?.blockhash;
    if (!blockhash) throw new Error("Solana did not return a recent blockhash.");
    const messageBytes = buildSolanaMessage({
        relayer: config.relayer,
        client: signer.clientAddress,
        cosigner: config.cosigner,
        source,
        destination: destinationAta,
        mint: config.usdc,
        amount: balance,
        blockhash,
        authority: signer.address,
    });
    if (messageBytes.length > 1232) throw new Error("The Solana claim message exceeds the transaction size limit.");

    const signature = ed25519.sign(messageBytes, ethers.getBytes(signer.privateKey));
    const txHash = `0x${hex(await sha256(messageBytes))}`;
    return {
        txHash,
        derivedAddress: signer.clientAddress,
        clientSig: { sig_hex: hex(signature) },
        calls: [],
        messageBytes: `0x${hex(messageBytes)}`,
    };
}

export const CLAIM_BUILDERS = {
    evm: buildEvmClaim,
    starknet: buildStarknetClaim,
    solana: buildSolanaClaim,
};

// stealth.core.js
//
// Canonical stealth-account derivation. This is the ONLY place chain
// config (factory/entrypoint/class-hash/cosigner) and the HKDF-based
// key-derivation math should live. Both the lane-creation path
// (stealth.receiver.js, loaded on the main page) and the scan/claim UI
// (stealth.js) import from here. Do not copy these values into another
// file — two independently-maintained copies is exactly how a lane
// created on one derivation gets scanned/claimed against a different
// one and the funds become unrecoverable.

import { ec as starkEc, CallData, hash } from "./starknet.js";
import { ethers } from "https://cdnjs.cloudflare.com/ajax/libs/ethers/6.13.2/ethers.js";

export const CHAINS = {
  starknet: {
    type: "starknet",
    rpcUrl: "https://starknet-mainnet.g.alchemy.com/starknet/version/rpc/v0_10/alch_pbUufy18xMzGDkyKmU87-",
    tokenAddress: "0x33068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
    shieldedPoolAddress: "0x040337b1af3c663e86e333bab5a4b28da8d4652a15a69beee2b677776ffe812a", // Cannonica Privacy Pool Address
    stealthAccountClassHash: "0x1764a400b3131c39a4ecb85199ac75ba2717c498d9a0245e932ec815674a003",
    litCosignerPubKey: "0x0456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01",
    decimals: 6,
  },
  base: {
    type: "evm",
    chainId: 8453,
    rpcUrl: "https://base-mainnet.g.alchemy.com/v2/alch_pbUufy18xMzGDkyKmU87-",
    tokenAddress: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    factoryAddress: "0x51E9813CAd0d94b0eBC8AedC27706bDE2a94d49A",
    entryPointAddress: "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
    litCosignerPubKey: "0x0000000000000000000000000000000000000000",
    byteCodeHash: "0x0000000000000000000000000000000000000000000000000000000000000000",
    decimals: 6,
  },
  ethereum: {
    type: "evm",
    chainId: 1,
    rpcUrl: "https://eth.llamarpc.com",
    tokenAddress: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    factoryAddress: "0x0000000000000000000000000000000000000000",
    entryPointAddress: "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
    litCosignerPubKey: "0x0000000000000000000000000000000000000000",
    byteCodeHash: "0x0000000000000000000000000000000000000000000000000000000000000000",
    decimals: 6,
  },
};

export const STARK_CURVE_ORDER = starkEc.starkCurve.CURVE.n;
export const SECP256K1_ORDER = BigInt(
  "0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141"
);

export function bytesToHex(bytes) {
  return Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
}

export function bytesToScalar(bytes, curveOrder) {
  const n = BigInt("0x" + bytesToHex(bytes));
  return n % curveOrder;
}

export async function hkdf(ikm, info) {
  const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: new Uint8Array(32), info: new TextEncoder().encode(info) },
    key,
    256
  );
  return new Uint8Array(bits);
}

/**
 * Resolves a chain reference to its canonical config. Accepts either a
 * plain string key ("base", "STARKNET", ...) or an object with a `.key`
 * field — but ALWAYS reads the actual chain parameters (factory,
 * cosigner, class hash, etc.) from this module's own CHAINS, never from
 * fields the caller might have attached to the object. That's what keeps
 * creation and scan/claim from ever disagreeing about an address.
 */
export function resolveChainKey(chainKeyOrRef) {
  const raw = typeof chainKeyOrRef === "string" ? chainKeyOrRef : chainKeyOrRef?.key;
  const chainKey = String(raw || "").toLowerCase();
  const chainConfig = CHAINS[chainKey];
  if (!chainConfig) {
    throw new Error(`stealth-core: unknown chain "${chainKey}"`);
  }
  return { chainKey, chainConfig };
}

export async function deriveDeterministicStealthKey(beanMasterSecret, laneId, index, chainKeyOrRef) {
  const { chainKey, chainConfig } = resolveChainKey(chainKeyOrRef);
  const curveOrder = chainConfig.type === "starknet" ? STARK_CURVE_ORDER : SECP256K1_ORDER;
  const spendMasterPriv = await hkdf(beanMasterSecret, `spend-v1:${chainKey}`);
  const spendMasterScalar = bytesToScalar(spendMasterPriv, curveOrder);
  const indexBytes = await hkdf(beanMasterSecret, `beanie-lane-index-v1:${laneId}:${chainKey}:${index}`);
  const indexScalar = bytesToScalar(indexBytes, curveOrder);
  return (spendMasterScalar + indexScalar) % curveOrder;
}

export async function deriveEvmCreate2Salt(beanMasterSecret, laneId, index, chainKeyOrRef) {
  const { chainKey } = resolveChainKey(chainKeyOrRef);
  const saltBytes = await hkdf(beanMasterSecret, `evm-create2-salt-v1:${laneId}:${chainKey}:${index}`);
  return "0x" + bytesToHex(saltBytes);
}

export function deriveStarknetStealthAddress(clientPubKeyFelt, cosignerPubKeyFelt, classHash) {
  return hash.calculateContractAddressFromHash(
    clientPubKeyFelt,
    classHash,
    CallData.compile({ client_pubkey: clientPubKeyFelt, cosigner_pubkey: cosignerPubKeyFelt }),
    0
  );
}

export function deriveEvmStealthAddress(clientAddress, cosignerAddress, chainConfig, saltHex) {
  const abiCoder = ethers.AbiCoder.defaultAbiCoder();
  const constructorArgs = abiCoder.encode(
    ["address", "address", "address"],
    [chainConfig.entryPointAddress, clientAddress, cosignerAddress]
  );
  const salt = ethers.keccak256(saltHex);
  const initCodeHash = ethers.keccak256(ethers.concat([chainConfig.byteCodeHash, constructorArgs]));
  return ethers.getCreate2Address(chainConfig.factoryAddress, salt, initCodeHash);
}

/**
 * THE canonical (laneId, index, chain) -> (privScalar, address) mapping.
 * Creation, scanning, and claiming should all resolve accounts through
 * this single function rather than re-implementing the branch logic
 * locally, so there's no way for the three call sites to drift apart.
 */
export async function deriveStealthAccount(masterSecret, laneId, index, chainKeyOrRef) {
  const { chainKey, chainConfig } = resolveChainKey(chainKeyOrRef);
  const stealthPrivScalar = await deriveDeterministicStealthKey(masterSecret, laneId, index, chainKey);

  if (chainConfig.type === "starknet") {
    const G = starkEc.starkCurve.ProjectivePoint.BASE;
    const clientPoint = G.multiply(stealthPrivScalar);
    const clientPubKeyFelt = "0x" + clientPoint.x.toString(16);
    const address = deriveStarknetStealthAddress(
      clientPubKeyFelt,
      chainConfig.litCosignerPubKey,
      chainConfig.stealthAccountClassHash
    );
    return { chainKey, chainConfig, stealthPrivScalar, address, clientPubKeyFelt };
  }

  const privKeyHex = "0x" + stealthPrivScalar.toString(16).padStart(64, "0");
  const wallet = new ethers.Wallet(privKeyHex);
  const clientAddress = wallet.address;
  const saltHex = await deriveEvmCreate2Salt(masterSecret, laneId, index, chainKey);
  const address = deriveEvmStealthAddress(clientAddress, chainConfig.litCosignerPubKey, chainConfig, saltHex);
  return { chainKey, chainConfig, stealthPrivScalar, address, clientAddress };
}
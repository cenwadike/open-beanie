// chains.js
//
// Single source of truth for per-chain config. Internal keys are UPPERCASE
// (BASE, STARKNET, ...). What goes into JSON bodies sent to the API is decided
// in ONE place: API_CHAIN_CASE below.
//
// A `null` field means "not deployed / not configured yet".

const ZERO_ADDR = `0x${"0".repeat(40)}`;

/**
 * Casing of chain identifiers in API request bodies.
 *   "lower" -> "base"   (what the backend README documents)
 *   "upper" -> "BASE"   (what older frontend code sent)
 * If /create or /pay answers 400 about the chain, flip this ONE constant.
 */
export const API_CHAIN_CASE = "lower";

/**
 * How the backend renders a Starknet felt as a STRING before it hashes it into
 * a foreign-chain (EVM) merchant identity. Only Starknet-settlement lanes are
 * affected (the Base leg's merchant = keccak256 of that string).
 *   "pad64" -> 0x + 64 hex digits   (what create_routes.rs is documented to do)
 *   "pad62" -> 0x + 62 hex digits   (Rust `{:#064x}` on a std integer: width counts the 0x)
 *   "min"   -> 0x + no leading zeros
 * Confirm with the parity snippet in the deploy notes, then set this once.
 */
export const BACKEND_FELT_FORMAT = "pad64";

/**
 * What a Solana receiver address IS (confirmed: an ordinary on-curve keypair
 * pubkey, not a PDA and not a token account).
 *   "wallet"        -> receiver is an owner pubkey; USDC lands in its associated token account
 *   "token-account" -> receiver is itself the USDC token account
 * Consequence for /pay: the SPL transfer destination AND `receiver_address` are
 * the receiver's ATA (the payment worker unpacks receiver_address as a token
 * account and reads its owner). Links, QR codes and the registry use the wallet.
 */
export const SOLANA_RECEIVER_KIND = "wallet";

/**
 * The 32-byte CCTP recipient pinned in the route when a NON-Solana receiver
 * settles on Solana. It feeds the predicted EVM/Starknet receiver address, so it
 * MUST equal what the backend announces.
 *   "wallet" -> targetRecipient's pubkey bytes. This is what the backend pins TODAY
 *               (recipient_bytes32: `recipient.parse::<Pubkey>().to_bytes()`).
 *   "ata"    -> ATA(targetRecipient, mint). This is what CCTP requires: Circle's docs say
 *               a Solana mintRecipient is the USDC TOKEN ACCOUNT, which must exist at
 *               receiveMessage.
 * BACKEND BUG until recipient_bytes32 returns the ATA for Solana: EVM/Starknet -> Solana
 * settlements would burn on the source chain and not be mintable. Fix the backend first,
 * then flip this to "ata" in the same deploy.
 */
export const SOLANA_CCTP_RECIPIENT = "wallet";

export const SOLANA_PLACEHOLDER_KEEPER = "11111111111111111111111111111111"; // all-zero key: signs nothing

/** Internal read API (keeps provider keys server-side). See rpc_proxy.rs. */
export const RPC_PROXY_ENABLED = true;
export const RPC_PROXY_PATH = "/api/v1/rpc";
/**
 * Receiver addresses decide where money goes, so they must be confirmed by two
 * independent sources (our proxy AND a direct public RPC). If only one answers,
 * refuse instead of trusting it.
 */
export const STRICT_PREDICTION = true;

/**
 * True for empty, all-zero or obviously dummy values. Stealth addresses
 * derived from such config are unrecoverable, so stealth.core.js refuses
 * to derive from it.
 */
export function isPlaceholder(value) {
    if (!value) return true;
    const v = String(value).toLowerCase();
    return /^0x0*$/.test(v) || v.includes("0123456789abcdef0123456789abcdef");
}

export const USDC_DECIMALS = 6;

// Starknet selector_from_name("Transfer"); keys are [selector, from, to].
export const STARKNET_TRANSFER_SELECTOR =
    "0x99cd8bde557814842a3121e8ddfd433a539b8c9f14bf31ebf108d12e6196e9";

// Beanie's own Starknet relayer account (the SNIP-9 `caller`).
export const KEEPER_STARKNET_ADDRESS =
    "0x01d4a73b58909eb341e6357bd085fea917d71c386ebaecd770792f7b5a34615a";

const USDC_EIP712 = { name: "USD Coin", version: "2" };

export const CHAINS = {
    BASE: {
        key: "BASE",
        name: "Base",
        kind: "evm",
        chainId: 8453,
        rpc: "https://mainnet.base.org",
        usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        factory: "0x51E9813CAd0d94b0eBC8AedC27706bDE2a94d49A",
        explorerAddress: "https://basescan.org/address/",
        explorerBase: "https://basescan.org",
        eip712: USDC_EIP712,
        stealth: {
            // The stealth account factory (CREATE2 deployer). CONFIRM it is the
            // receiver factory address and not a separate contract.
            factory: "0x51E9813CAd0d94b0eBC8AedC27706bDE2a94d49A",
            entryPoint: "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
            cosigner: ZERO_ADDR, // TODO: real TEE cosigner address
            // TODO: creation bytecode (hex, WITHOUT constructor args) of the
            // stealth account. CREATE2 needs keccak256(creationCode ++ ctorArgs),
            // so a bare bytecode *hash* is not enough to compute the address.
            initCode: null,
        },
    },
    ETHEREUM: {
        key: "ETHEREUM",
        name: "Ethereum",
        kind: "evm",
        chainId: 1,
        rpc: "https://eth.llamarpc.com",
        usdc: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        factory: null, // TODO
        explorerAddress: "https://etherscan.io/address/",
        explorerBase: "https://etherscan.io",
        eip712: USDC_EIP712,
        stealth: null,
    },
    ARBITRUM: {
        key: "ARBITRUM",
        name: "Arbitrum",
        kind: "evm",
        chainId: 42161,
        rpc: "https://arb1.arbitrum.io/rpc",
        usdc: "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
        factory: null, // TODO
        explorerAddress: "https://arbiscan.io/address/",
        explorerBase: "https://arbiscan.io",
        eip712: USDC_EIP712,
        stealth: null,
    },
    MONAD: {
        key: "MONAD",
        name: "Monad",
        kind: "evm",
        chainId: 143, // VERIFY
        rpc: "https://rpc.monad.xyz", // VERIFY
        usdc: null, // TODO
        factory: null, // TODO
        explorerAddress: null,
        explorerBase: null,
        eip712: null, // TODO: USDC domain name/version on Monad
        stealth: null,
    },
    STARKNET: {
        key: "STARKNET",
        name: "Starknet",
        kind: "starknet",
        rpc: "https://starknet.drpc.org",
        usdc: "0x033068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb",
        factory: "0x074fc53d92ed14249d7d7f37a22d879ad6d5660c2f86bcf5ae74e3d22347e30c",
        explorerAddress: "https://starkscan.co/contract/",
        stealth: {
            classHash: "0x1764a400b3131c39a4ecb85199ac75ba2717c498d9a0245e932ec815674a003",
            cosignerPubKey: "0x0456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01", // TODO: dummy value
            shieldedPool: "0x040337b1af3c663e86e333bab5a4b28da8d4652a15a69beee2b677776ffe812a",
            // Invoke-v3 resource bounds for gasless claims. TUNE: they must cover
            // the UDC deploy + approve + shield_deposit execution.
            resourceBounds: {
                l1_gas: { max_amount: "0x2710", max_price_per_unit: "0x174876e800" },
                l2_gas: { max_amount: "0x1c9c380", max_price_per_unit: "0x2540be400" },
                l1_data_gas: { max_amount: "0x1b58", max_price_per_unit: "0x174876e800" },
            },
        },
    },
    SOLANA: {
        key: "SOLANA",
        name: "Solana",
        kind: "solana",
        rpc: "https://api.mainnet-beta.solana.com", // heavily rate-limited: rely on the proxy in production
        usdc: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        factory: null, // receivers are server-side throwaway keys: not predictable
        // Public key of SOLANA_KEEPER_PRIVATE_KEY: the fee payer of every gasless
        // payment. TODO: gasless Solana stays off until this is set.
        keeper: SOLANA_PLACEHOLDER_KEEPER, // PLACEHOLDER: replace. The backend rejects any other fee payer with 400.
        walletChain: "solana:mainnet", // Wallet Standard chain id
        // Beanie factory program (SOLANA_PROGRAM_ID). null = Solana receivers cannot be
        // discovered or verified, so lanes simply get no Solana receiver (fails closed).
        program: {
            id: "BsiBBPjkAiLJgQDNmjfajHeJjpFMzJqCNz2FtzZa2Hpg", // declare_id! (confirm it is the mainnet deployment)
            seeds: { config: "config", pending: "pending" }, // CONFIG_SEED / PENDING_SEED, confirmed from the program
        },
        explorerAddress: "https://solscan.io/account/",
        explorerBase: "https://solscan.io",
        stealth: null,
    },
};

export const CHAIN_KEYS = Object.keys(CHAINS);

/** Internal key form (UPPERCASE). */
export const wire = (key) => String(key || "").toUpperCase();

/** Form used inside API JSON bodies (see API_CHAIN_CASE). */
export const apiChain = (key) =>
    API_CHAIN_CASE === "upper" ? wire(key) : String(key || "").toLowerCase();

export const chainByKey = (key) => CHAINS[wire(key)] ?? null;
export const chainLabel = (key) => chainByKey(key)?.name ?? String(key);

/** Receiver address can be computed client-side from the factory. */
export const canPredict = (c) =>
    (c.kind === "evm" || c.kind === "starknet") && Boolean(c.factory && c.rpc);
export const laneChains = () => Object.values(CHAINS).filter(canPredict);

/** Solana receivers are random keypairs: found and checked on-chain, not predicted. */
export const canDiscover = (c) => c.kind === "solana" && Boolean(c.program?.id && c.usdc);
/** A payout can land here: predictable receivers, or Solana once its program is configured. */
export const canSettle = (c) => canPredict(c) || canDiscover(c);

/** The gasless /pay flow has a client-side signer for this chain. */
export const canGasless = (c) =>
    c.kind === "evm"
        ? Boolean(c.usdc && c.chainId && c.eip712)
        : c.kind === "starknet"
            ? Boolean(c.usdc && KEEPER_STARKNET_ADDRESS)
            : c.kind === "solana"
                ? Boolean(c.usdc && c.keeper)
                : false;

/** Stealth config is complete and free of placeholder values. */
export function stealthReady(c) {
    const s = c?.stealth;
    if (!s) return false;
    if (c.kind === "evm") {
        return (
            Boolean(s.factory && s.entryPoint) &&
            !isPlaceholder(s.cosigner) &&
            !isPlaceholder(s.initCode)
        );
    }
    if (c.kind === "starknet") {
        return Boolean(s.classHash && s.shieldedPool) && !isPlaceholder(s.cosignerPubKey);
    }
    return false;
}
export const stealthChains = () => Object.values(CHAINS).filter(stealthReady);

export function chainIcon(key, size = 24) {
    const k = wire(key);
    const open = `<svg viewBox="0 0 42 42" width="${size}" height="${size}" aria-hidden="true">`;
    if (k === "BASE") {
        return `${open}<circle cx="21" cy="21" r="21" fill="#0052ff"/><path d="M21 32.8c6.52 0 11.8-5.28 11.8-11.8S27.52 9.2 21 9.2c-5.82 0-10.66 4.21-11.62 9.75h15.2v4.1H9.38C10.34 28.59 15.18 32.8 21 32.8Z" fill="#fff"/></svg>`;
    }
    if (k === "STARKNET") {
        return `${open}<circle cx="21" cy="21" r="21" fill="#0c0c4d"/><path d="M21 8 32 21 21 34 10 21 21 8Z" fill="#ec796b"/></svg>`;
    }
    if (k === "SOLANA") {
        return `${open}<circle cx="21" cy="21" r="21" fill="#000"/><path d="M15 11h18l-3 5H12z" fill="#14f195"/><path d="M12 18h18l3 5H15z" fill="#00c2ff"/><path d="M15 25h18l-3 5H12z" fill="#9945ff"/></svg>`;
    }
    const letter = (chainLabel(k)[0] || "?").toUpperCase().replace(/[^A-Z?]/g, "?");
    return `${open}<circle cx="21" cy="21" r="21" fill="#64748b"/><text x="21" y="27" text-anchor="middle" font-size="18" fill="#fff" font-family="sans-serif">${letter}</text></svg>`;
}

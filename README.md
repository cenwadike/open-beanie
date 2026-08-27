# Beanie

A multichain, non-custodial, permissionless stablecoin payment gateway.
A merchant registers once and gets a single receiving identity; deposits
from any supported chain settle to them without Beanie ever holding
custody of merchant funds. Starknet is used as a **privacy layer** inside
that gateway: routing a payment through it delinks the deposit a
customer makes from the withdrawal a merchant receives, so the two
can't be tied together on-chain.

## What this is?

Every supported chain gets its own pair of contracts, but they share one
pattern: **a factory registers a merchant once, and that merchant gets a
dedicated, non-custodial instance whose destinations are pinned for
good.** No admin key can redirect funds after that instance is deployed.

### Starknet — the privacy leg

Payments routed through Starknet pass through two separate, unlinked
contracts so that a deposit and the matching withdrawal can't be
connected on-chain:

- **`ShieldInAnonymizer`** — one per merchant. Receives incoming funds,
  takes the protocol fee, and shields the remainder into a stealth note
  inside the STRK20 pool. Nothing about the note reveals which merchant
  it belongs to except a value only the merchant's key can derive.
- **`BridgeOutAnonymizer`** — one per merchant. Takes spent, unshielded
  funds out of the STRK20 pool and exits Starknet via CCTP V2 to the
  merchant's destination chain. It never touches the shield-in side, so
  there's no on-chain link between a specific deposit and a specific
  payout.
- **`MerchantFactory`** — deploys the `ShieldInAnonymizer` +
  `BridgeOutAnonymizer` pair per merchant via `deploy_syscall`, keyed by
  the merchant's public key, with an independent deployment nonce per
  merchant.

### EVM (Base, Ethereum) — direct receiving legs

Chains without a privacy layer get a plain non-custodial receiver: funds
arrive, the fee is taken, and the rest routes to the merchant directly.

- **`ChainXReceiver`** — an implementation contract cloned once per
  merchant with OpenZeppelin `Clones`. Collects funds, takes the fee — 
  split 90% to a single treasury address and 10% to whoever calls 
  `sweep()`, since nothing else triggers it automatically the way STRK20 
  pool nodes drive privacy_invoke — then either burns the net amount 
  through CCTP V2 to the merchant's chosen destination, or forwards it 
  directly to the merchant on the same chain.
- **`MerchantFactory` (Solidity)** — deploys deterministic clones per
  merchant and resolves the CCTP destination domain for Starknet, Base,
  Solana, or Ethereum from a chain name.
- **`MerchantWebhookRegistry` (Solidity)** — Allows merchants to register and updating their individual delivery endpoints on-chain. This structural layout facilitates dynamic, per-merchant notification webhooks entirely off the ledger state.

### Off-Chain Automated Infrastructure (`evm_keeper`)

To keep the transaction flow completely fluid on non-custodial EVM legs, a highly efficient **Rust background daemon** acts as the system's operational core:
- **Chunked State Discovery:** Continuously queries incoming network states via structured `eth_getLogs` block chunking, effortlessly bypassing rigid RPC provider limits (e.g., 10,000 block max thresholds).
- **Atomic Batch Sweeping:** Consolidates all merchant instances detecting new transactions during a poll cycle and groups them into a single `Multicall3.aggregate3()` execution. It sets `allowFailure: true`, preventing a single broken or racing instance from ruining the gas profile of adjacent batched operations.
- **Cryptographic Webhook Dispatch:** Resolves active receiver addresses back to their registered on-chain merchant profiles, fetches their webhook metadata, and dispatches authenticated JSON payloads. Webhooks are cryptographically bound using EIP-191 `personal_sign` over the keeper wallet's identity, allowing target nodes to securely confirm identity parity with the transaction sender.

## How a payment moves

1. A customer pays into whichever chain's receiving instance is
   associated with the merchant — one address per merchant, regardless
   of which chain the payment originates on.
2. The instance takes a **0.50% fee**. On Starknet this fee is shielded 
   into its own note under a single treasury key, never transferred. 
   On the EVM legs it's a plain transfer, split 90% to that same 
   treasury address and 10% to whoever called `sweep()`.
3. **On the privacy leg (Starknet):** the caller supplies an 
   **`ephemeral_pubkey;`** the contract hashes it against both the merchant's `merchant_pubkey` and the shared `treasury_pubkey` to derive two note IDs — one per note, bound to whichever static key it was hashed against. Only the corresponding key can ever reproduce its note. Withdrawal happens later, from the separate BridgeOutAnonymizer, so no on-chain record ties either note back to this deposit.
4. The net amount settles either by shielding into the STRK20 pool
   (Starknet shield-in), burning out via CCTP V2 `deposit_for_burn`
   (Starknet bridge-out, and the EVM legs when a cross-chain destination
   is set), or transferring directly on-chain (EVM legs, same-chain
   settlement).

## Access control

`privacy_invoke` on both Starknet contracts only executes when called by
the configured privacy pool address. Every cross-chain destination —
token, messenger, destination domain, mint recipient — is set once at
construction (Starknet) or `initialize()` (EVM) and there is no admin
path to change it afterward.

## Fee mechanics

The 0.50% protocol fee is taken once, at deposit 
— **`ShieldInAnonymizer`** on Starknet, **`sweep()`** on the EVM legs. 
-  **`BridgeOutAnonymizer`** and **`CCTP`** burn path takes maximum of 15bps on starknet and 2bps on EVM;

| | Starknet bridge-out | EVM legs |
|---|---|---|
| Fee | 50 bps, split 100 | 50 bps, split 90/10 |
| CCTP path | Fast Transfer | Fast Transfer |
| Max fee | 15 bps of amount | 2 bps of amount |
| Finality threshold | 1000 | 1000 |

## Tests

- **`tests/test.cairo`** — snforge tests against mock
  token/messenger contracts, covering: shield-in fee split, allowance,
  and stealth note derivation; access-control rejection on both
  anonymizers; and factory deployment plus duplicate-registration
  rejection.
- **`test/ReceiverFactory.t.sol`** — Foundry tests covering clone
  deployment, both the CCTP-burn and same-chain settlement paths through
  `sweep()`, idempotency on a repeated sweep, duplicate-registration
  rejection, and the `initialize()` one-shot guard.

## Local setup

### Starknet

```bash
# Pinned toolchain — matches starkware-libs/starknet-privacy's own
# workspace manifest, since the privacy package this depends on is
# pinned to it internally.
curl --proto '=https' --tlsv1.2 -sSf https://docs.swmansion.com/scarb/install.sh \
  | sh -s -- --version 2.17.0

# Local path dependency — referenced the same way StarkWare's own
# anonymizer packages reference it, no published registry version exists.
git clone --depth 1 https://github.com/starkware-libs/starknet-privacy.git ../starknet-privacy

scarb build
scarb test
```

**Status:** `scarb build` has not completed a clean run in every
environment this was tried in — dependency resolution against
`scarbs.xyz` needs outbound network access. Confirm that step succeeds
before relying on anything downstream of it, and run testnet integration
tests against the live STRK20 pool and Starknet CCTP contracts before
touching real funds.

### EVM Smart Contracts

```bash
forge install
forge test
```

### EVM Daemon Keeper (`evm_keeper`)

```bash
cd evm_keeper
cp .env.example .env # Configure your node provider URLs and contract deployments here
cargo check
cargo run
```

## File map

| File | Job |
|---|---|
| `starknet_beanie/src/merchant_factory.cairo` | Deploys a `ShieldInAnonymizer` + `BridgeOutAnonymizer` pair per merchant |
| `starknet_beanie/src/shield_in.cairo` | Per-merchant shield-in: fee split + stealth note derivation |
| `starknet_beanie/src/bridge_out.cairo` | Per-merchant bridge-out: CCTP Fast Transfer exit from Starknet |
| `starknet_beanie/tests/test.cairo` | snforge tests for all three Starknet contracts |
| `evm_beanie/src/MerchantFactory.sol` | Deploys a `ChainXReceiver` clone per merchant; resolves CCTP domain by chain name |
| `evm_beanie/src/ChainXReceiver.sol` | Per-merchant EVM receiver: fee split + CCTP burn or same-chain transfer |
| `evm_beanie/src/MerchantWebhookRegistry.sol` | On-chain registration directory storing individual merchant notification links |
| `evm_beanie/test/ReceiverFactory.t.sol` | Foundry tests for the EVM factory and receiver |
| `evm_keeper/src/main.rs` | Background scheduling runner controlling chunk logs and async execution loops |
| `evm_keeper/src/config.rs` | Robust structural schema mapping out the operational environment variables |
| `evm_keeper/src/sweep.rs` | Handles multi-target index scans, chunk loops, and Multicall3 compilation |
| `evm_keeper/src/webhook.rs` | Handles cryptographic text payload signatures via EIP-191 and HTTP POST delivery |

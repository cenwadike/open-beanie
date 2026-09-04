# Beanie

A multichain, non-custodial, permissionless stablecoin payment gateway. A merchant registers one destination address and chain and gets dedicated, non-custodial receiving instances; deposits settle to them without Beanie ever holding custody of merchant funds. Starknet is one of the receiving chains, and its privacy stack offers both an STRK20 privacy pool and deterministic **2-of-2 WebAuthn / Lit TEE Stealth Accounts**: routing a payment through stealth addresses delinks the customer from the claim while allowing gasless, passkey-secured claims with zero persistent state on the backend.

## What this is

Every supported chain gets its own receiver contract and its own factory, and both chains share one pattern: **a factory deploys a merchant a dedicated, non-custodial instance whose destinations are pinned for good at deploy time.** No admin key can redirect funds after that instance exists. The receiver contract itself never distinguishes how a deposit arrived — same statelessness on every chain.

### Contracts — EVM (Base, Ethereum)
- **`ChainXReceiver`** — an implementation contract cloned once per merchant with OpenZeppelin `Clones`. Collects funds, takes the fee (0.50%, split 90% to a single treasury address and 10% to whoever calls `sweep()`), then either burns the net amount through CCTP V2 to the merchant's chosen destination chain, or forwards it directly to the merchant on the same chain. `sweep()` is permissionless, idempotent, and atomic — a zero balance is a silent no-op.
- **`MerchantFactory`** — deploys deterministic clones per merchant (`Clones.cloneDeterministic`, capped at `MAX_RECEIVERS_PER_MERCHANT = 32` per merchant) and resolves the CCTP destination domain for Starknet, Base, Solana, or Ethereum from a chain name. Same-chain settlement is signaled by a zero recipient; cross-chain settlement requires a nonzero one.
- **`MerchantWebhookRegistry`** — the single webhook URL registry across **all** of Beanie, not just the EVM legs. Keyed by `address merchant`, permissionless (same trust model as `registerMerchant`, since it's called by Beanie's own sponsoring keeper wallet on the merchant's behalf, not by the merchant's own wallet).

### Contracts — Starknet
- **`StarknetReceiver`** — the same contract as `ChainXReceiver`, in Cairo. Same fee split (0.50%, 90/10), same permissionless/idempotent/atomic `sweep()`, same same-chain-vs-CCTP-burn branch keyed off a zero/nonzero mint recipient. It calls `TokenTransmitter.send_message` directly.
- **`MerchantFactory`** — deploys one `StarknetReceiver` instance per merchant via `deploy_syscall`, same cap and same register/predict/count interface shape as the EVM factory.
- **`StealthAccount`** — a counterfactual 2-of-2 Cairo account requiring both a client WebAuthn PRF signature $(r_1, s_1)$ and a Lit Protocol TEE Enclave co-signature $(r_2, s_2)$ to execute sweeping transfers.

## Privacy & Stealth Account Architecture

In addition to standard payments, Beanie supports client-side deterministic stealth payments on Starknet.

### 1. Client-Side Key Derivation (Passkey + PRF)
- A WebAuthn credential PRF extension derives a master secret directly inside the browser.
- Payment lanes derive index-specific stealth keys deterministically:
  $$\text{StealthPrivKey}_i = (\text{SpendMasterScalar} + \text{IndexScalar}_i) \pmod{\text{CurveOrder}}$$
- The counterfactual account address is derived off-chain:
  $$\text{Address} = H(\text{ClientPubKey}_i, \text{LitCosignerPubKey}, \text{ClassHash})$$

### 2. Zero-State Index Recovery & Scanning
- The browser scans ERC-20 `Transfer` events directly from Starknet RPC nodes using a **Gap Limit** algorithm.
- No database or backend persistence tracks payment lanes, stealth indices, or balances.

### 3. 2-of-2 Co-signing & Gasless Paymaster Execution
- To sweep funds, the client constructs the transaction and signs locally $(r_1, s_1)$.
- The payload is posted to `POST /api/v1/stealth/execute`.
- The backend routes the WebAuthn proof to the Lit Protocol TEE enclave to obtain the second signature $(r_2, s_2)$.
- The backend appends $(r_2, s_2)$, sponsors the gas fee, and broadcasts the completed 2-of-2 transaction directly to Starknet.

## How a payment moves

1. A customer pays into whichever chain's receiving instance is associated with the merchant — one predicted address per merchant per chain, returned by the factory's `predict*Address` view before deployment even happens.
2. The instance takes a 0.50% fee, split 90% of fee to a single treasury address and 10% to whoever calls `sweep()` — identical math on every chain.
3. The net amount settles either by burning out via CCTP V2 `deposit_for_burn` to a merchant-chosen destination chain, or transferring directly on-chain for same-chain settlement.
4. If the customer chose the stealth payment path on Starknet, funds land in a counterfactual 2-of-2 `StealthAccount`, which the recipient recovers and sweeps gaslessly via passkey authentication.

## Access control

Every cross-chain destination — token, messenger, destination domain, mint recipient — is set once at `initialize()` (EVM) or contract construction (Starknet) and there is no admin path to change it afterward, on either chain.

## Fee mechanics

| | Starknet | EVM |
|---|---|---|
| Protocol fee | 50 bps of gross | 50 bps of gross |
| Fee split | 90% treasury / 10% caller | 90% treasury / 10% caller |
| CCTP path | Fast Transfer | Fast Transfer |
| CCTP max fee | 15 bps of amount | 2 bps of amount |
| Finality threshold | 1000 | 1000 |

CCTP domains: Base = 6, Solana = 5, Starknet = 25, Ethereum = 0.

## Off-chain: the keeper

A single Rust daemon runs both chains' sweep loops concurrently from one process (`tokio::spawn` per chain, joined at the top level):
- **Registry discovery** — scans `ReceiverAnnounced` events on each chain's own factory (`eth_getLogs` on Base, the Starknet-native event query on Starknet) to build a receiver → merchant map per chain.
- **Webhook discovery** — scans the single, chain-agnostic `MerchantWebhookRegistry` (an EVM contract) for `WebhookUrlSet` events. Both chains' loops read from the same shared `merchant_webhook` map; only the Base loop needs to refresh it, since the registry only ever lives on one EVM chain regardless of which chain a given deposit originated on.
- **Deposit detection + sweep** — watches for `Transfer` events into known receivers, batches every receiver that saw activity in a poll cycle into one multicall sweep per chain, and resolves each deposit back to its merchant's webhook URL.
- **Webhook delivery** — signs every outbound payload before delivery, with a signature scheme that matches the origin chain: EIP-191 `personal_sign` for Base-originated deposits, Poseidon-hash + Starknet native signing for Starknet-originated ones. The receiving server gets `X-Signature-Scheme`, `X-Signature`, and `X-Signer-Address` headers to verify against, plus an `Idempotency-Key` set to the deposit's tx hash. Delivery retries with exponential backoff and treats 4xx responses as terminal (no retry) vs. everything else as retryable.

## Off-chain: `beanie_api`

The HTTP layer serving API requests and static assets.
- `POST /api/v1/lanes/init`: No signup, no login, no wallet connect — merchant address and target chain in, predicted receiver addresses back out immediately.
- `POST /api/v1/stealth/execute`: Co-signing and gasless relay proxy. Accepts client signatures $(r_1, s_1)$, requests Lit Protocol TEE enclave co-signatures $(r_2, s_2)$, pays gas via Paymaster, and submits the transaction to Starknet RPC.

This process also serves the frontend as a static-file fallback route — starting `beanie_api` brings up the API, the deploy worker, the stealth claim engine, and the customer/merchant-facing UI together.

## Frontend

Served from `beanie_api/public/`:
- `beanie.html` — lane-creation flow (pick a settlement chain, get a receiver address, poll for deposits).
- `pay.html` — customer-facing payment page for a shared lane link.
- `claim.html` — stealth payment recovery dashboard (`stealth-claim.js`). Performs passkey PRF key derivation, scans RPC logs for payment indices, and triggers gasless 2-of-2 stealth sweeps.

## Local setup

### Starknet

```bash
curl --proto '=https' --tlsv1.2 -sSf [https://docs.swmansion.com/scarb/install.sh](https://docs.swmansion.com/scarb/install.sh) \
  | sh -s -- --version 2.17.0

scarb build
scarb test

```

### EVM Smart Contracts

```bash
cd evm_beanie
forge install
forge test

```

### Keeper

```bash
cd beanie_keeper
cp .env.example .env
cargo check
cargo run

```

### API + Frontend

```bash
cd beanie_api
cp .env.example .env 
cargo run

```

## Tests

* **`test/ReceiverFactory.t.sol`** — Foundry tests covering clone deployment, CCTP-burn vs. same-chain paths, idempotency, and registration limits.
* **`tests/test.cairo`** — snforge tests for `StarknetReceiver`, `MerchantFactory`, and 2-of-2 `StealthAccount` signature validations.

## File map

| File | Job |
| --- | --- |
| `starknet_beanie/src/merchant_factory.cairo` | Deploys one `StarknetReceiver` instance per merchant |
| `starknet_beanie/src/receiver.cairo` | Per-merchant Starknet receiver: fee split + CCTP burn or same-chain transfer |
| `starknet_beanie/src/stealth_account.cairo` | 2-of-2 multi-sig account validating client + Lit TEE signatures |
| `starknet_beanie/tests/test.cairo` | snforge tests for factory, receiver, and stealth accounts |
| `evm_beanie/src/MerchantFactory.sol` | Deploys a `ChainXReceiver` clone per merchant; resolves CCTP domain |
| `evm_beanie/src/ChainXReceiver.sol` | Per-merchant EVM receiver: fee split + CCTP burn or same-chain transfer |
| `evm_beanie/src/MerchantWebhookRegistry.sol` | Single, chain-agnostic webhook URL registry |
| `beanie_keeper/src/main.rs` | Dual-chain sweep loops (Base + Starknet), run concurrently from one process |
| `beanie_keeper/src/webhook.rs` | Dual signature scheme (EIP-191 / Starknet-native) + signed webhook delivery |
| `beanie_api/src/main.rs` | Boots EVM + Starknet providers, deploy worker, and Axum HTTP routes |
| `beanie_api/src/routes.rs` | `POST /api/v1/lanes/init`, `POST /api/v1/stealth/execute`, and static fallback |
| `beanie_api/public/stealth-claim.js` | Client-side PRF key derivation, gap-limit scanner, and claim engine |
| `beanie_api/public/` | Frontend — lane creation (`beanie.html`), payment (`pay.html`), and claim (`claim.html`) |

```
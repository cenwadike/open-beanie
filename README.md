# Beanie

A multichain, non-custodial, permissionless stablecoin payment gateway. A merchant registers one destination address and chain and gets dedicated, non-custodial receiving instances; deposits settle to them without Beanie ever holding custody of merchant funds. Starknet is one of the receiving chains, and its STRK20 privacy pool is available to any customer paying a Beanie merchant: routing a payment through an unshielding transfer delinks the deposit a customer makes from the deposit the receiver contract sees. Payments can also land on a stealth address

## What this is

Every supported chain gets its own receiver contract and its own factory, and both chains share one pattern: **a factory deploys a merchant a dedicated, non-custodial instance whose destinations are pinned for good at deploy time.** No admin key can redirect funds after that instance exists. The receiver contract itself never distinguishes how a deposit arrived — same statelessness on every chain.

### Contracts — EVM (Base, Ethereum)
- **`ChainXReceiver`** — an implementation contract cloned once per merchant with OpenZeppelin `Clones`. Collects funds, takes the fee (0.50%, split 90% to a single treasury address and 10% to whoever calls `sweep()`), then either burns the net amount through CCTP V2 to the merchant's chosen destination chain, or forwards it directly to the merchant on the same chain. `sweep()` is permissionless, idempotent, and atomic — a zero balance is a silent no-op.
- **`MerchantFactory`** — deploys deterministic clones per merchant (`Clones.cloneDeterministic`, capped at `MAX_RECEIVERS_PER_MERCHANT = 32` per merchant) and resolves the CCTP destination domain for Starknet, Base, Solana, or Ethereum from a chain name. Same-chain settlement is signaled by a zero recipient; cross-chain settlement requires a nonzero one.
- **`MerchantWebhookRegistry`** — the single webhook URL registry across **all** of Beanie, not just the EVM legs. Keyed by `address merchant`, permissionless (same trust model as `registerMerchant`, since it's called by Beanie's own sponsoring keeper wallet on the merchant's behalf, not by the merchant's own wallet).

### Contracts — Starknet
- **`StarknetReceiver`** — the same contract as `ChainXReceiver`, in Cairo. Same fee split (0.50%, 90/10), same permissionless/idempotent/atomic `sweep()`, same same-chain-vs-CCTP-burn branch keyed off a zero/nonzero mint recipient. It calls `TokenTransmitter.send_message` directly 
- **`MerchantFactory`** — deploys one `StarknetReceiver` instance per merchant via `deploy_syscall`, same cap and same register/predict/count interface shape as the EVM factory.

## STRK20 integration

`StarknetReceiver` doesn't know or care how a deposit arrived. A customer can send USDC to a merchant's Starknet receiver address two ways:
- **Transfer to Pool controlled address** — inbound privacy, funds land as an ordinary balance then get automatically shielded.
- **An STRK20 unshielding transfer** — the customer spends a shielded pool note and withdraws directly to the receiver's address, using the pool's own standard withdrawal primitive. 

On-chain, these looks like any other shield or unshield transaction; nothing ties it back to which note or which prior shield-in funded it.

Both land as the same plain ERC20-shaped balance, indistinguishable to `sweep()`. The privacy step, when the payer wants it, happens entirely client-side in the payer's own wallet before Beanie's contract ever sees the funds 

Beanie doesn't build, deploy, or maintain any privacy-specific logic to make this work, it composes directly with the pool's core deposit lifecycle (shield → private transfer → **withdraw to any public address** → shield) exactly as documented.

## How a payment moves

1. A customer pays into whichever chain's receiving instance is associated with the merchant — one predicted address per merchant per chain, returned by the factory's `predict*Address` view before deployment even happens.
2. The instance takes a 0.50% fee, split 90% of fee to a single treasury address and 10% to whoever calls `sweep()` — identical math on every chain.
3. The net amount settles either by burning out via CCTP V2 `deposit_for_burn` to a merchant-chosen destination chain, or transferring directly on-chain for same-chain settlement.
4. If the customer chose the private path on Starknet, the delinking already happened before step 1 finished — it's not a separate step in this flow at all.

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
- **Registry discovery** — scans `MerchantRegistered` events on each chain's own factory (`eth_getLogs` on Base, the Starknet-native event query on Starknet) to build a receiver → merchant map per chain.
- **Webhook discovery** — scans the single, chain-agnostic `MerchantWebhookRegistry` (an EVM contract) for `WebhookUrlSet` events. Both chains' loops read from the same shared `merchant_webhook` map; only the Base loop needs to refresh it, since the registry only ever lives on one EVM chain regardless of which chain a given deposit originated on.
- **Deposit detection + sweep** — watches for `Transfer` events into known receivers, batches every receiver that saw activity in a poll cycle into one multicall sweep per chain, and resolves each deposit back to its merchant's webhook URL.
- **Webhook delivery** — signs every outbound payload before delivery, with a signature scheme that matches the origin chain: EIP-191 `personal_sign` for Base-originated deposits, Poseidon-hash + Starknet native signing for Starknet-originated ones. The receiving server gets `X-Signature-Scheme`, `X-Signature`, and `X-Signer-Address` headers to verify against, plus an `Idempotency-Key` set to the deposit's tx hash. Delivery retries with exponential backoff and treats 4xx responses as terminal (no retry) vs. everything else as retryable.

## Off-chain: `beanie_api`

The only HTTP surface in Beanie. One route, `POST /api/v1/lanes/init`: no signup, no login, no wallet connect, no signed message — a merchant address and a target chain in, a set of predicted receiver addresses back out immediately. Actual on-chain registration happens in the background, paid for by whoever runs this process, across however many chains the request's `source_chains` list names — Starknet and EVM registrations both go through the same deployment worker and the same webhook registry, uniformly, with no chain-specific branch beyond what each chain's own RPC/SDK requires.

Because `registerMerchant`/`register_merchant` are already permissionless on-chain, this API is a convenience layer, not a gatekeeper — anyone who doesn't trust a given operator's rate limits can call either factory directly with their own wallet and skip it entirely. This same process also serves the frontend as a static-file fallback route — starting `beanie_api` alone brings up the API, the deploy worker, and the customer/merchant-facing UI together.

## Frontend

Served from `beanie_api/public/`. `beanie.html` is the lane-creation flow (pick a settlement chain, get a receiver address, poll for deposits); `pay.html` is the customer-facing payment page for a shared lane link. Balance polling reads directly from public RPCs — a balance increase on a Starknet receiver is reported as "deposit detected" without distinguishing a plain transfer from an unshielding one, since the contract itself doesn't distinguish them either.

## Local setup

### Starknet

```bash
curl --proto '=https' --tlsv1.2 -sSf [https://docs.swmansion.com/scarb/install.sh](https://docs.swmansion.com/scarb/install.sh) \
  | sh -s -- --version 2.17.0

scarb build
scarb test

```

No external pool-package dependency is required to build or test the Starknet contracts — `StarknetReceiver` and its factory only depend on `openzeppelin` (for the ERC20 dispatcher interface) and Starknet's own core library.

### EVM Smart Contracts

```bash
cd evm_beanie
forge install
forge test

```

### Keeper

```bash
cd beanie_keeper
cp .env.example .env   # RPC URLs, factory/token/registry addresses for BOTH chains
cargo check
cargo run

```

### API + frontend

```bash
cd beanie_api
cp .env.example .env   # RPC URLs, factory addresses (both chains), keeper keys
cargo run

```

## Tests

* **`test/ReceiverFactory.t.sol`** — Foundry tests covering clone deployment, both the CCTP-burn and same-chain settlement paths through `sweep()`, idempotency on a repeated sweep, duplicate-registration rejection, and the `initialize()` one-shot guard.
* **`tests/test.cairo`** — snforge tests for `StarknetReceiver` and its `MerchantFactory`, covering the same shape of cases as the Solidity suite: fee split, CCTP-burn vs. same-chain settlement, idempotent re-sweep, the receiver cap, and the one-shot `initialize()` guard.

## File map

| File | Job |
| --- | --- |
| `starknet_beanie/src/merchant_factory.cairo` | Deploys one `StarknetReceiver` instance per merchant |
| `starknet_beanie/src/receiver.cairo` | Per-merchant Starknet receiver: fee split + CCTP burn or same-chain transfer |
| `starknet_beanie/tests/test.cairo` | snforge tests for the Starknet factory + receiver |
| `evm_beanie/src/MerchantFactory.sol` | Deploys a `ChainXReceiver` clone per merchant; resolves CCTP domain by chain name |
| `evm_beanie/src/ChainXReceiver.sol` | Per-merchant EVM receiver: fee split + CCTP burn or same-chain transfer |
| `evm_beanie/src/MerchantWebhookRegistry.sol` | The single, chain-agnostic webhook URL registry |
| `evm_beanie/test/ReceiverFactory.t.sol` | Foundry tests for the EVM factory and receiver |
| `beanie_keeper/src/main.rs` | Dual-chain sweep loops (Base + Starknet), run concurrently from one process |
| `beanie_keeper/src/config.rs` | Per-chain environment/config schema |
| `beanie_keeper/src/sweep_evm.rs` | Base-side log scanning, chunk loops, and Multicall3 sweep compilation |
| `beanie_keeper/src/sweep_starknet.rs` | Starknet-side event scanning and multicall sweep compilation |
| `beanie_keeper/src/webhook.rs` | Dual signature scheme (EIP-191 / Starknet-native) + signed webhook delivery |
| `beanie_api/src/main.rs` | Boots the EVM + Starknet clients, the deploy worker, and the HTTP server |
| `beanie_api/src/routes.rs` | `POST /api/v1/lanes/init` + static frontend fallback |
| `beanie_api/src/worker.rs` | Background on-chain registration across both chains, plus webhook registration |
| `beanie_api/public/` | Frontend — lane creation (`beanie.html`) and the customer payment page (`pay.html`) |

```

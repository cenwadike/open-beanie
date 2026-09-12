# EVM Keeper → Chainlink CRE Port

This repository documents and ports the off-chain Rust EVM keeper logic to a Chainlink Runtime Environment (CRE) workflow using CRE trigger and write primitives.

---

## Architecture Overview

The system consists of three smart contracts and a CRE workflow relay:

* **`ChainXReceiver.sol`**: Minimal Proxy clone (EIP-1167) deployed per merchant. Exposes `sweep()`, which splits receiver balances into net and fee portions, transfers net amounts or burns via CCTP (`ITokenMessengerV2.depositForBurn`), and pays caller fees to `tx.origin`.
* **`MerchantFactory.sol`**: Deterministically clones `ChainXReceiver` instances via CREATE2 (`registerMerchant`, `announceReceiver`, `predictReceiverAddress`).
* **`MerchantWebhookRegistry.sol`**: Maps merchant addresses to notification webhook URLs (`setWebhookUrl`).
* **`CREKeeperReceiver.sol`**: Implements the Chainlink CRE `IReceiver` interface (`onReport`), validating the Keystone Forwarder identity/workflow signature and forwarding batched execution payloads via `Multicall3.aggregate3()`.

---


### Configuration Schema (`config.ts`)

```typescript
import { z } from "zod";

export const configSchema = z.object({
  chainSelectorName: z.string(),
  isTestnet: z.boolean().default(false),
  rpcUrl: z.string(),
  factoryAddress: z.string(),
  webhookRegistryAddress: z.string(),
  tokenAddress: z.string(),
  creKeeperReceiverAddress: z.string(),
  registryStartBlock: z.number(),
  webhookRegistryStartBlock: z.number(),
  logChunkBlocks: z.number().default(2000),
  depositScanBlocks: z.number().default(100),
  schedule: z.string().default("*/30 * * * * *"),
});

export type Config = z.infer<typeof configSchema>;

```

### Production Configuration Example (`config.json`)

```json
{
  "chainSelectorName": "ethereum-mainnet-base-1",
  "isTestnet": false,
  "rpcUrl": "[https://mainnet.base.org](https://mainnet.base.org)",
  "factoryAddress": "0x51E9813CAd0d94b0eBC8AedC27706bDE2a94d49A",
  "webhookRegistryAddress": "0x4389822B42deA140EaCcb060C648eECff42BBb61",
  "tokenAddress": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
  "creKeeperReceiverAddress": "0x46D7E780Dd2DA8A1034b54D6Af4297b9eB06e3dd",
  "registryStartBlock": 51005159,
  "webhookRegistryStartBlock": 51005159,
  "logChunkBlocks": 2000,
  "depositScanBlocks": 100,
  "schedule": "*/30 * * * * *"
}

```

---

## 3. Log Discovery Safety Bounds

To guarantee that log pagination never triggers call quota errors, ensure `getLogsChunked` limits total HTTP calls:

```typescript
export function 
  maxCalls: number,
  requester: HttpRequester,
  rpcUrl: string,
  address: string,
  topics: (string | string[] | null)[],
  fromBlock: number,
  toBlock: number,
  chunkSize: number,
): Log[] {
  const logs: Log[] = [];
  
  // Guard against scanning far behind current tip
  const minAllowedStart = Math.max(fromBlock, toBlock - (chunkSize * maxCalls));
  let start = minAllowedStart;
  let callsMade = 0;

  while (start <= toBlock && callsMade < maxCalls) {
    const end = Math.min(start + chunkSize - 1, toBlock);
    logs.push(
      ...rpcCall<Log[]>(requester, rpcUrl, "eth_getLogs", [
        {
          address,
          topics,
          fromBlock: `0x${start.toString(16)}`,
          toBlock: `0x${end.toString(16)}`,
        },
      ]),
    );
    start = end + 1;
    callsMade++;
  }

  return logs;
}

```

---

## 4. Local Simulation

Execute workflow simulations locally using the CRE CLI:

```bash
cre workflow simulate sweep_flow

```

### Bypassing Call Limits During Local Testing

If you need to bypass quotas while testing non-block-range logic locally:

```bash
cre workflow simulate sweep_flow --skip-type-checks --limits=none

```

> **Warning:** Using `--limits=none` masks execution quota failures locally. Ensure `registryStartBlock`, `webhookRegistryStartBlock`, and `logChunkBlocks` are properly configured prior to mainnet deployment.

---

## 5. Mainnet Deployment Steps

### Step 1: Deploy On-Chain Contracts

1. Deploy `MerchantFactory.sol` and `MerchantWebhookRegistry.sol` to Base Mainnet.
2. Deploy `CREKeeperReceiver.sol`, configuring the Keystone Forwarder address and workflow configuration in the constructor.
3. Record transaction deployment block heights (e.g., `51005159`).

### Step 2: Configure Environment JSON

1. Update `config.json` with target addresses (`factoryAddress`, `webhookRegistryAddress`, `tokenAddress`, `creKeeperReceiverAddress`).
2. Update `registryStartBlock` and `webhookRegistryStartBlock` to match your deployment block height.
3. Ensure `logChunkBlocks` is set to `2000`.

### Step 3: Configure CRE Production Secrets

Set required production secrets (such as webhook signing keys):

```bash
cre secrets set --env mainnet WEBHOOK_SIGNING_KEY "your-production-signing-key"

```

### Step 4: Validate Workflow Definition

Run the CRE syntax check:

```bash
cre workflow check

```

### Step 5: Deploy Workflow to Mainnet

Deploy the workflow to the target CRE mainnet environment:

```bash
cre workflow deploy --env mainnet

```

### Step 6: Post-Deployment Verification

1. Verify deployment status:
```bash
cre workflow status

```


2. Monitor execution cycles on the CRE dashboard to verify that total HTTP requests remain well under the 15-call quota ceiling per tick.

```

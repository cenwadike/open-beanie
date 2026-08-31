# Beanie Lanes API

Beanie Lanes API is a zero-friction, single-endpoint backend service built with Rust and Axum. It enables instant payment lane creation and deterministic multi-chain receiver address prediction across EVM chains (e.g., Base) and Starknet.

Registration is completely permissionless and requires no login, wallet connections, or signatures from the merchant. The API derives deterministic receiver contract addresses instantly while managing on-chain contract deployments and webhook registrations in background queues.

---

## Features

* **Deterministic Address Prediction:** Instantly calculate payment receiver addresses before on-chain transactions confirm.
* **Permissionless Design:** Direct contract calls are supported; the API serves as an funded convenience layer.
* **Background Worker Processing:** On-chain deployment tasks are queued in an asynchronous Tokio channel with automatic retries for transient errors.
* **Multi-Chain Support:** Pre-configured for Base (EVM) and Starknet contracts.
* **Dual-Layer Rate Limiter:** Protects the operator's gas funds using per-IP and per-merchant rate limits.
* **Clean-URL Static Server:** Built-in Axum handler serving canonical frontend routes and static assets from `public/`.

---

## Architecture Overview

```
                          POST /api/v1/lanes/init
                                     │
                                     ▼
                      ┌──────────────────────────────┐
                      │    Axum API Route Handler    │
                      └──────────────┬───────────────┘
                                     │
           ┌─────────────────────────┴─────────────────────────┐
           ▼                                                   ▼
┌──────────────────────┐                           ┌──────────────────────┐
│  Contract View Call  │                           │ Background Worker    │
│  (Instant Prediction)│                           │ (mpsc Deploy Queue)  │
└──────────┬───────────┘                           └──────────┬───────────┘
           │                                                  │
           ▼                                                  ▼
┌────────────────────────┐                         ┌──────────────────────┐
│ Json(InitLaneResponse) │                         │ On-Chain Deployment  │
└────────────────────────┘                         └──────────────────────┘

```

---

## Getting Started

### Prerequisites

* [Rust](https://www.rust-lang.org/) (2021 edition or newer)
* RPC access for EVM (Base) and Starknet nodes
* Funded keeper wallets on target networks

### Environment Variables

Create a `.env` file in the root directory:

```env
# Server Configuration
LISTEN_ADDR=0.0.0.0:8080
RATE_LIMIT_PER_HOUR=5

# EVM Configuration
EVM_RPC_URL=https://base-mainnet.g.alchemy.com/v2/YOUR_API_KEY
EVM_FACTORY_ADDRESS=0x...
KEEPER_PRIVATE_KEY=0x...
WEBHOOK_REGISTRY_ADDRESS=0x...

# Starknet Configuration
STARKNET_RPC_URL=https://starknet-mainnet.g.alchemy.com/v2/YOUR_API_KEY
STARKNET_FACTORY_ADDRESS=0x...
STARKNET_PRIVATE_KEY=0x...
STARKNET_ACCOUNT_ADDRESS=0x...
STARKNET_CHAIN_ID=0x534e5f4d41494e

```

### Build & Run

```bash
# Development
cargo run

# Production Build
cargo build --release
./target/release/beanie-api

```

---

## API Reference

### Health Check

```http
GET /health

```

**Response (`200 OK`):**

```
ok

```

---

### Initialize Payment Lane

```http
POST /api/v1/lanes/init
Content-Type: application/json
Idempotency-Key: optional-uuid-string

```

#### Request Body

| Field | Type | Description |
| --- | --- | --- |
| `merchant_address` | `string` | Target wallet address on settlement chain. |
| `target_chain` | `string` | Settlement destination chain (`BASE` or `STARKNET`). |
| `source_chains` | `array[string]` | Supported incoming payment chains (e.g., `["BASE", "STARKNET"]`). |
| `webhook_url` | `string` *(optional)* | Webhook callback URL for payment notifications. |

```json
{
  "merchant_address": "0x0000000000000000000000000000000000000000",
  "target_chain": "BASE",
  "source_chains": ["BASE", "STARKNET"],
  "webhook_url": "https://example.com/webhook"
}

```

#### Response (`200 OK`)

```json
{
  "lanes": [
    {
      "chain": "BASE",
      "address": "0x1234567890abcdef1234567890abcdef12345678"
    },
    {
      "chain": "STARKNET",
      "address": "0x0987654321fedcba0987654321fedcba0987654321fedcba"
    }
  ]
}

```

#### Error Responses

| Status Code | Reason |
| --- | --- |
| `400 Bad Request` | Invalid chain, missing address, or malformed settlement format. |
| `429 Too Many Requests` | IP or merchant wallet rate limit exceeded. |
| `500 Internal Server Error` | RPC call or on-chain prediction failure. |
# Keeper Service (Base & Starknet)

A high-performance Rust service designed to monitor, track, and process multi-chain sweeps and webhooks across Base (EVM) and Starknet networks.

---

## Overview

The Keeper Service automates cross-chain deposit indexing, merchant registry polling, and webhook delivery. Built using asynchronous Rust with Tokio, it maintains isolated event loops and independent block watermarks for each supported execution environment.

### Core Features

* **Multi-Chain Event Monitoring:** Concurrent monitoring of deposit and registry events on EVM (Base) and Cairo-based (Starknet) execution environments.
* **Deterministic Watermarking:** Dedicated block-indexing watermarks (`registry_watermark`, `webhook_watermark`, `deposit_watermark`) to prevent race conditions during state synchronization.
* **Universal Webhooks Registry:** Dynamic merchant webhook discovery using Base `EventFilter` queries and `Event` decoding.
* **Network-Aware Signer Context:** Explicit domain separation binding signers dynamically to network identifiers (`chain_id::BASE` = `X-Signature-Scheme::eip191` vs `chain_id::SEPOLIA` = `X-Signature-Scheme::starknet-poseidon`).

---

## Architecture & Project Structure

```text
src/
├── config.rs          # Environment loading and unified runtime config
├── sweep_evm.rs       # EVM (Base) log filtering, contract bindings, and tx dispatch
├── sweep_starknet.rs  # Starknet provider/account abstraction and event indexers
└── webhook.rs         # Async webhook delivery engine and payload handling

```

---

## Prerequisites

Ensure the following tooling and dependencies are installed before building:
    
```bash
    cargo check
```

---

## Environment Configuration

Configure key settings using environment variables. Create a `.env` file or export variables prior to starting the binary:

### Base (EVM) Configuration

```bash
    cat env.example
```

### Starknet Configuration

```bash
    cat env.example
```

---

## Getting Started

### Building from Source

To compile the service binary:

```bash
cargo build --release

```

---

## Testing

use library:

```toml
[dependencies]
beanie_keeper = { path = "../beanie_keeper" }

```

```rust
use beanie_keeper::*;

pub fn main() {
    ...
   let deposit = beanie_keeper::config::Deposit {
        tx_hash: tx_hash.clone(),
        from_address: task.from_address.clone(),
        receiver: format!("{:?}", receiver_addr),
        amount_raw: task.amount_raw.clone(),
        block_number: receipt
            .block_number
            .map(|b| b.as_u64())
            .unwrap_or(0),
    };
    ...
}

```
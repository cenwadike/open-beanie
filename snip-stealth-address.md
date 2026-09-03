---
snip: <to be assigned>
title: Starknet Non-Interactive Stealth Account Standard
description: Dual-key stealth address generation, counterfactual 2-of-2 account derivation, and event-based scanning on Starknet.
author: Chinedu E. Nwadike (@cenwadike)
discussions-to: <link to community forum thread>
status: Draft
type: Standards Track
category: SRC
created: 2026-09-01
---

## Abstract

This standard specifies non-interactive stealth address generation and claim execution for Starknet. Adapting the Dual-Key Stealth Address Protocol (DKSAP) to Starknet's architecture, this document standardizes STARK curve scalar derivation, counterfactual 2-of-2 co-signed account address calculation via native Account Abstraction (SNIP-6), and an event-driven payment indexing scheme that eliminates the need for persistent on-chain registry contracts.

## Motivation

No standard currently exists on Starknet for direct, sender-non-interactive, counterfactual stealth accounts. Existing proposals such as SNIP-43 focus on Bech32m encodings for pooled shielded transfers (MASP notes requiring zero-knowledge spend proofs), which represent a fundamentally different primitive. Other privacy efforts rely on pooled commitment-nullifier designs. 

Standardizing a non-interactive account derivation scheme and off-chain indexing mechanism enables wallets (e.g., Ready, Braavos) and dApps to natively generate, scan, and claim one-time payments without deploying dedicated registry contracts or introducing custom centralized state.

## Specification

### 1. Cryptographic Primitives & Constants

- **Curve**: STARK curve over $\mathbb{F}_q$ where $q = 2^{251} + 17 \cdot 2^{192} + 1$.
- **Base Point**: $G = (x_G, y_G) \in E(\mathbb{F}_q)$.
- **Key Derivation Hash**: $\text{HKDF-SHA256}(\text{ikm}, \text{info})$.
- **Scalar Mapping**: $s = \text{BigInt}(\text{hash\_bytes}) \pmod{\text{CURVE\_ORDER}}$.

### 2. Stealth Meta-Address & Identifiers

A recipient publishes a Stealth Meta-Address comprising two STARK-curve public keys:

$$\text{Meta-Address} = (K_{\text{spend}}, K_{\text{view}})$$

where $K_{\text{spend}} = k_{\text{spend}} \cdot G$ and $K_{\text{view}} = k_{\text{view}} \cdot G$ for private scalars $k_{\text{spend}}, k_{\text{view}} \in [1, q-1]$.

- `scheme_id = 1`: Single-signer DKSAP account derivation.
- `scheme_id = 2`: Dual-signer 2-of-2 (Client + Cosigner/TEE) DKSAP account derivation.

### 3. Derivation & Execution Specifications

#### A. One-Time Key Derivation (Sender / Indexer)

Given recipient meta-address $(K_{\text{spend}}, K_{\text{view}})$ and derivation index $i$:

1. Derive client scalar: $s_i = \text{HKDF}(K_{\text{master}}, \text{lane\_index\_info})$.
2. Compute ephemeral client stealth public key: $K_{\text{client\_stealth}} = K_{\text{spend}} + s_i \cdot G$.

#### B. Counterfactual Account Address Derivation

Starknet contract addresses are calculated deterministically using the standard protocol formula:

$$\text{calldata\_hash} = \text{PedersenArray}([K_{\text{client\_stealth}}, K_{\text{cosigner}}])$$

$$\text{StealthAddress} = \text{Pedersen}(\text{"STARKNET\_CONTRACT\_ADDRESS"}, \text{deployer}=0, \text{salt}=x(K_{\text{client\_stealth}}), \text{class\_hash}, \text{calldata\_hash}) \pmod{2^{251}}$$

*Note: In accordance with Starknet core specifications, address derivation strictly utilizes Pedersen hashing.*

#### C. Private Key Recovery & Claim Execution (Recipient)

1. Compute recipient private scalar: $k_{\text{stealth\_client}} = (k_{\text{spend\_master}} + s_i) \pmod q$.
2. Verify public key alignment: $k_{\text{stealth\_client}} \cdot G = K_{\text{client\_stealth}}$.
3. Construct claim execution transaction payload locally, signing transaction hash $H_{\text{tx}}$ with $k_{\text{stealth\_client}}$ to yield signature pair $(r_1, s_1)$.
4. Submit transaction hash $H_{\text{tx}}$ and signature $(r_1, s_1)$ to the co-signer for secondary signature generation $(r_2, s_2)$ and paymaster relay execution.

### 4. Account Interface Requirements

Stealth accounts deployed under this standard MUST implement `ISRC6` (SNIP-6) and validate dual-signatures formatted as `[r1, s1, r2, s2]`:

```cairo
#[starknet::interface]
pub trait IStealthAccount<TContractState> {
    fn __execute__(ref self: TContractState, calls: Array<Call>) -> Array<Span<felt252>>;
    fn __validate__(ref self: TContractState, calls: Array<Call>) -> felt252;
    fn is_valid_signature(self: @TContractState, hash: felt252, signature: Array<felt252>) -> felt252;
}

```

#### Event-Driven Indexing (Registry-Less Discovery)

Payment discovery relies exclusively on querying transfer events emitted by underlying token contracts (e.g., ERC-20 `Transfer(from, to, amount)`). Scanners derive counterfactual addresses across a defined gap limit and filter log topics directly via RPC node queries (`getEvents`), eliminating persistent on-chain registry contracts.

## Rationale

* **Registry-Less Design**: Eliminating an explicit announcer or registry contract reduces on-chain footprint, removes storage fees, and relies on existing event infrastructure.
* **Pedersen Address Derivation**: Enforces strict alignment with Starknet's native `calculateContractAddressFromHash` execution path.
* **2-of-2 Dual-Signer Model**: Separates client signature generation from gas payment and co-signing relayers (e.g., TEE / Lit Protocol), enabling gasless claims for fresh stealth accounts without funding ETH/STRK for gas prior to claiming.

## Backwards Compatibility

This proposal is additive and fully compatible with SNIP-6 (Account Standard). It integrates with SNIP-43 unified address encodings (`strku`) by reserving a dedicated Type-Length-Value (TLV) payload identifier (`0x02 = Dual-Signer Stealth Meta-Address`).

## Security Considerations

* **Co-Signer Enclaves**: In Scheme 2 setups, the co-signer MUST NOT generate signature $(r_2, s_2)$ without verifying valid authorization from the account controller (e.g., WebAuthn PRF assertion).
* **Gap Limit Scanning**: Indexers MUST enforce a standard gap limit to prevent skipped balances across non-sequential address indices.
* **Transaction Malleability**: The signature array `[r1, s1, r2, s2]` MUST be validated in strict order during `__validate__` execution to prevent replay or invalid signature injection.

## Copyright

Copyright and related rights waived via [CC0](https://www.google.com/search?q=../LICENSE.md).

```

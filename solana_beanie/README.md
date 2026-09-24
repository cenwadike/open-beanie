# Deposit Receiver Factory (Solana)

One program instance serves unlimited merchants. The **receiver** is the deposit address handed to exchanges and wallets; everything else derives from it.

| Thing | Derivation |
|---|---|
| `receiver` | on-curve pubkey (vanity-ground off-chain) |
| `receiver_token_account` | `ATA(receiver, mint)` |
| `pending_registration` | PDA `[pending, receiver]`: small pinned holder of the signed registration tx |
| `receiver_config` | PDA `[config, receiver]`: **exists ⇔ registered** (no status flag) |
| staging account (CCTP burn) | `ATA(receiver_config, mint)` |
| `MerchantRegistry` | PDA `[registry, merchant]`, appended at registration |
| `FactoryConfig` | PDA `[factory]` |

Exchanges derive the ATA from the owner, so the receiver stays `AccountOwner` forever. `receiver_config` never replaces it; it only holds **delegate** (`u64::MAX`) and **CloseAccount** authority.

## Instructions

| Instruction | Who signs | What it does |
|---|---|---|
| `initialize_factory` | payer | one-time config: mint, treasury, 3 distinct CCTP domains |
| `announce_merchant(merchant, chain, recipient, receiver, reg_tx)` | payer only | validates the route, `init`s `[pending, receiver]` with the signed `reg_tx`, emits all derived addresses. **Creates no config, touches no shared state** |
| `register_merchant(merchant, chain, recipient)` | payer + receiver (pre-signed) | `init`s `[config, receiver]`, approve + CloseAccount authority to config, appends to the registry, closes the pinned account (rent → payer) |
| `sweep()` | anyone | 0.50% fee (10% of it to the caller, rest to treasury), net to vault or CCTP burn. Needs the config to exist |

## Client sequence

1. Grind a vanity `receiver` in-process (CSPRNG per attempt, discard non-matches).
2. **tx1, payer signs only** (plus a throwaway nonce keypair): create a durable nonce account with `authority = payer`, create `ATA(receiver)`, create the staging ATA.
3. Read the nonce value. Sign **one** tx with `receiver` + payer: `[AdvanceNonce(payer), register_merchant(...)]`, fee payer = payer. Discard the key. Its lifetime is grind → tx1 confirmed → one signature; it signs `register_merchant` only.
4. Send the payer-only `announce_merchant` tx carrying those signed bytes. Keep an offline copy of the bytes too.
5. Read back before disclosing: the stored blob equals your bytes, and the derived addresses in `MerchantAnnounced` match yours. Simulate the blob (`sigVerify: false`) against live state.
6. Any keeper reads the blob from `[pending, receiver]` and broadcasts it unchanged.
7. Disclose the address only after a finalized read-back: `[config, receiver]` exists, `delegate == config`, `closeAuthority == config`, `owner == receiver`.
8. **Close the nonce.** In a later tx the payer (the nonce authority) sends `WithdrawNonce(all lamports) → payer`. Many nonces can be batched into one tx. Nothing on-chain is involved.

If step 4 or 5 fails, discard the address: nobody has seen it. The payer can still withdraw that nonce (step 8) to recover its rent; that also kills the stored blob, so only do it when abandoning.

## Why it is shaped this way

- **Idempotent, once.** `init` on `[config, receiver]` is the only thing that gates registration. A second registration fails and repeats no effect (no second registry entry, no delegate replacement). `RM-SAD-2` proves it with a second validly signed tx.
- **No status flag, no realloc.** Config exists with data and the right seeds, or it doesn't.
- **Announce needs no receiver signature.** Any tampered or squatted announce fails the client's read-back and the address is discarded unseen. A squatter can't block registration either: the stored blob is only a holder, and the offline copy still registers (`AN-HAPPY-3`).
- **`receiver_token_account` and the staging account are not stored.** Both derive from `receiver` + `mint`.
- **The nonce is closed by the payer, not inside the reg tx.** A full `WithdrawNonce` requires the stored nonce to differ from the cluster's current one (Agave docs: https://docs.anza.xyz/implemented-proposals/durable-tx-nonces; system program source returns `NonceBlockhashNotExpired`). `AdvanceNonce` sets the stored nonce to the current one, so in the same tx the withdraw is rejected and can't ride in the reg tx. Making the payer the nonce authority lets it withdraw in a follow-up tx with no program change, no config fields and no extra instruction. `NC-SAD-1` asserts this on your validator.

## Failure matrix

| Event | Outcome |
|---|---|
| Crash before signing | nothing to recover; address never shown |
| Signed, announce never lands or read-back mismatches | orphan nonce/ATA rent only; address never shown |
| Deposit lands before registration | safe: once registered the delegate is `u64::MAX`, so the whole balance is sweepable |
| Blob broadcast early or by a stranger | registers exactly what was signed |
| Blob replayed | rejected: nonce already advanced |
| Someone tries to advance the nonce | only the nonce authority (payer) can; neither a stranger nor a leaked receiver key can pre-empt the blob |
| Program upgraded while blobs are pending | can invalidate them: freeze the upgrade authority or don't change `register_merchant` |
| Mint freezes the receiver ATA before registration | `approve` fails and the nonce is burned: issuer-level edge |

## Known residuals

- **Registry cap is enforced at registration.** If a merchant's registry fills between your simulation and the broadcast, the tx fails and burns its nonce while the key is gone. The address was never disclosed, so nothing is stranded, but the address is lost (`RM-SAD-8`). Keep the cap high enough, or shard by merchant.
- **The payer key is the nonce authority.** A compromised payer key can advance or close nonces and kill pending blobs (grief, not theft; the address is undisclosed until `Active`). Use a payer key you already protect as the operator key.
- **Nonce rent is returned only if the payer runs step 8.** It is a client-side follow-up, not enforced on-chain.
- **Squatting an announce** wastes one address and costs the squatter rent, nothing else.

## Size budget (estimates, verify with `AN-HAPPY-1`)

Signed reg tx ≈ 740 bytes; announce tx ≈ 1150 of 1232 bytes. That leaves room for a compute-budget instruction but little else. `MAX_REG_TX_BYTES` (1024) is a sanity cap; the packet limit is the real one.

## Key hygiene

The receiver key remains owner-authority forever, so anyone who ever holds it can outrun the delegate. Keep the grinder and signer in one process, never write the key to disk, logs, swap or core dumps, and use a runtime that can genuinely zeroize (JS cannot). A pre-ground pool of keys at rest contradicts the short-exposure goal.

## Tests

```
anchor build
anchor test          # Anchor.toml: mocha timeout -t 1000000
```

`NC-*` cover the nonce close. `SW-CC-HAPPY-1` (real CCTP CPI) is `it.skip`; the validator flags are in the test file.

## Status

Written but **not compiled or run** here (no Rust/Anchor toolchain; the test file only passed a TypeScript syntax check). Verify first:

- `RM-HAPPY-1` / `RM-SAD-2`: `init` of config plus `close = payer` on the pinned account in one instruction.
- `AN-HAPPY-1`: measured announce tx size ≤ 1232.
- `SW-SAD-6`: Anchor surfaces `AccountNotInitialized` for a config that doesn't exist.
- IDL camelCase account names match the ones used in the tests.
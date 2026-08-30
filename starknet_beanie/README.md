```markdown
# Starknet Beanie (`starknet_beanie`)

Smart contracts for Beanie's Starknet leg written in Cairo. Handles deterministic merchant receiver deployments, fee collection, local settlement, cross-chain CCTP v2 teleports, and merchant receiver querying.

---

## Contracts Overview

* **`StarknetReceiver.cairo`**: Merchant-specific receiver contract. Receives USDC deposits, calculates fee splits (0.50% total: 90% protocol treasury, 10% caller/keeper incentive), and executes atomic settlement—either via same-chain ERC-20 transfer or cross-chain CCTP `deposit_for_burn` to the merchant's target destination domain.
* **`MerchantFactory.cairo`**: Deploys deterministic `StarknetReceiver` instances using `deploy_syscall` with salt calculated from the merchant address and nonce. Tracks merchant receiver counts and valid destination domains (`BASE`, `SOLANA`, `ETHEREUM`).

---

## Setup & Testing

### Prerequisites

* [Scarb](https://docs.swmansion.com/scarb/) (Cairo package manager)
* [Starknet Foundry](https://foundry-rs.github.io/starknet-foundry/) (`scarb`, snforge`, `sncast`)

### Installation & Compilation

```bash
cd starknet_beanie
scarb build --no-warnings

```

### Running Tests

Run the Cairo test suite using `scarb`:

```bash
scarb test --no-warnings

```

---

## Environment Configuration

Configure your `snfoundry.toml` or set environment variables for deployment via `sncast`:

```toml
[sncast.mainnet]
account = "deployer_account"
accounts-file = "~/.starknet_accounts/starknet_open_zeppelin_accounts.json"
url = "[https://starknet-mainnet.public.blastapi.io](https://starknet-mainnet.public.blastapi.io)"

```

---

## Deployment (Starknet Mainnet)

### Canonical Starknet Mainnet Parameters

* **USDC Address:** `0x33068f6539f8e6e6b131e6b2b814e6c34a5224bc66947c47dab9dfee93b35fb`
* **TokenMessengerV2 (CCTP):** `0x7d421b9ca8aa32df259965cda8acb93f7599f69209a41872ae84638b2a20f2a`
* **CCTP Domain IDs:** `BASE=6`, `SOLANA=5`, `ETHEREUM=0`

### Step-by-Step Deployment Commands

**1. Declare `StarknetReceiver` Class Hash:**

```bash
sncast --profile mainnet declare \
  --contract-name StarknetReceiver

```

*Note down the returned `class_hash`.*

**2. Declare `MerchantFactory` Contract:**

```bash
sncast --profile mainnet declare \
  --contract-name MerchantFactory

```

**3. Deploy `MerchantFactory`:**

```bash
sncast --profile mainnet deploy \
  --class-hash <MERCHANT_FACTORY_CLASS_HASH> \
  --constructor-calldata \
    <STARKNET_RECEIVER_CLASS_HASH> \
    0x053c91253bc9682c04929ca02ed00b3e423f6710d2ee7e0d5ebb06f3ecf368a9 \
    <TREASURY_ADDRESS> \
    0x00a30b2c1f440523e428c949c894d0fb31f8ebaf22e11a141b7f03eb3eb7eb47 \
    6 \
    5 \
    0

```

```

```

# EVM Beanie (`evm_beanie`)

Smart contracts for Beanie's EVM leg. Handles deterministic merchant receiver deployment, fee collection, local settlement, cross-chain CCTP v2 teleports, and merchant webhook registry.

---

## Contracts Overview

* **`ChainXReceiver.sol`**: Merchant-specific receiver clone implementation. Receives USDC deposits, calculates fee splits (0.50% total: 90% protocol treasury, 10% caller/keeper incentive), and executes atomic settlement—either via same-chain `ERC20.transfer` or cross-chain CCTP `depositForBurn`.
* **`MerchantFactory.sol`**: Deploys deterministic ERC-1167 minimal proxies (`ChainXReceiver`) per merchant using `CREATE2`. Tracks merchant receiver registries, nonce counts, and domain validation across supported chains (`STARKNET`, `BASE`, `SOLANA`, `ETHEREUM`).
* **`MerchantWebhookRegistry.sol`**: Global registry contract mapping merchant addresses to custom HTTP notification endpoint URLs.

---

## Setup & Testing

### Prerequisites

* [Foundry](https://getfoundry.sh/) (`forge`, `cast`)

### Installation

```bash
cd evm_beanie
forge install
```

### Running Tests

Run the full test suite with warnings denied:

```Bash
forge test --deny-warnings
```

### Environment Configuration

Create a `.env` file or export the required environment variables in your terminal:

```Bash
# Deployer & Protocol Roles
export PRIVATE_KEY="0x..."               # Deployer wallet private key (funded with ETH on Base)
export TREASURY_ADDRESS="0x..."          # Address to receive 90% protocol fee share

# RPC & Verification
export BASE_RPC_URL="[https://mainnet.base.org](https://mainnet.base.org)"
export BASESCAN_API_KEY="YOUR_API_KEY"   # Optional, for contract verification
```

## Deployment (Base Mainnet)

### Canonical Base Mainnet Parameters

- **`USDC Address`**: 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913

- **`TokenMessengerV2 (CCTP)`**: 0x1682Ae6375C4E4A97e4B583BC394c3c0cf8B63f9

- **`CCTP Domain IDs`**: ETHEREUM=0, BASE=6, SOLANA=5, STARKNET=25

### Execution Commands

1. Dry Run Simulation (Local Fork):

```Bash
forge script script/DeployBaseMainnet.s.sol:DeployBaseMainnet \
  --rpc-url $BASE_RPC_URL```

2. On-Chain Deployment & Verification:

```Bash
forge script script/DeployBaseMainnet.s.sol:DeployBaseMainnet \
  --rpc-url $BASE_RPC_URL \
  --broadcast \
  --verify \
  --etherscan-api-key $BASESCAN_API_KEY \
  --legacy
```
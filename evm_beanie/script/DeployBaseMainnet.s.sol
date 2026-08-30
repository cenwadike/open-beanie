// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

import "forge-std/Script.sol";
import "../src/ChainXReceiver.sol";
import "../src/MerchantFactory.sol";
import "../src/MerchantWebhookRegistry.sol";

contract DeployBaseMainnet is Script {
    // Base Mainnet Constants
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant TOKEN_MESSENGER =
        0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d;

    // CCTP Destination Domains
    uint32 constant ETH_DOMAIN = 0;
    uint32 constant BASE_DOMAIN = 6;
    uint32 constant SOLANA_DOMAIN = 5;
    uint32 constant STARKNET_DOMAIN = 25;

    function run() external {
        uint256 deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        address treasury = vm.envAddress("TREASURY_ADDRESS");

        require(treasury != address(0), "Treasury address required");

        vm.startBroadcast(deployerPrivateKey);

        // 1. Deploy Implementation
        ChainXReceiver implementation = new ChainXReceiver();
        console.log(
            "ChainXReceiver Implementation deployed to:",
            address(implementation)
        );

        // 2. Deploy Factory
        MerchantFactory factory = new MerchantFactory(
            address(implementation),
            USDC,
            treasury,
            TOKEN_MESSENGER,
            STARKNET_DOMAIN,
            BASE_DOMAIN,
            SOLANA_DOMAIN,
            ETH_DOMAIN
        );
        console.log("MerchantFactory deployed to:", address(factory));

        // 3. Deploy Webhook Registry
        MerchantWebhookRegistry webhookRegistry = new MerchantWebhookRegistry(
            address(factory)
        );
        console.log(
            "MerchantWebhookRegistry deployed to:",
            address(webhookRegistry)
        );

        vm.stopBroadcast();
    }
}

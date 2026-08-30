// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

interface IMerchantFactoryView {
    function getReceiverCount(address merchant) external view returns (uint256);
}

/// The single webhook URL registry for all of Beanie — not per-chain. A merchant might
/// register a receiver on Base, Starknet, both, or (once Solana lands) all three.
contract MerchantWebhookRegistry {
    IMerchantFactoryView factory;
    mapping(address => string) public webhookUrl;
    error EmptyUrl();
    error NotRegistered();

    event WebhookUrlSet(address indexed merchant, string url);

    constructor(address _factory) {
        factory = IMerchantFactoryView(_factory);
    }

    function setWebhookUrl(address merchant, string calldata url) external {
        if (factory.getReceiverCount(merchant) == 0) revert NotRegistered();
        webhookUrl[msg.sender] = url;
        emit WebhookUrlSet(msg.sender, url);
    }
}

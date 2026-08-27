// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMerchantFactoryView {
    function getReceiver(address merchant) external view returns (address);
}

/// Lets a merchant point their own webhook endpoint — self-service, wallet-authenticated,
/// no signup, same no-admin-key posture as the rest of Beanie. Kept as its own contract
/// rather than folded into MerchantFactory: registering a receiver and pointing a URL are
/// different concerns with very different update frequencies (one-time vs. whenever the
/// merchant's infrastructure changes).
contract MerchantWebhookRegistry {
    IMerchantFactoryView public immutable factory;

    mapping(address => string) public webhookUrl;

    event WebhookUrlSet(address indexed merchant, string url);

    error EmptyUrl();
    error NotRegistered();

    constructor(address _factory) {
        factory = IMerchantFactoryView(_factory);
    }

    /// Callable only by the merchant themselves — msg.sender IS the auth, no separate
    /// login. Requires the caller to already have a receiver deployed via MerchantFactory,
    /// so this can't be used to squat a URL against an address that never registered.
    function setWebhookUrl(string calldata url) external {
        if (bytes(url).length == 0) revert EmptyUrl();
        if (factory.getReceiver(msg.sender) == address(0))
            revert NotRegistered();
        webhookUrl[msg.sender] = url;
        emit WebhookUrlSet(msg.sender, url);
    }
}

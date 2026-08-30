// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

/// The single webhook URL registry for all of Beanie — not per-chain. A merchant might
/// register a receiver on Base, Starknet, both, or (once Solana lands) all three; this
/// contract doesn't care, and deliberately can't check receiver existence per chain:
/// an EVM contract has no way to read Starknet state, so any factory-existence gate
/// here could only ever cover EVM registrations, silently locking out Starknet-only
/// merchants. Rather than build an asymmetric check, this drops the gate entirely.
///
/// That's a smaller loss than it looks: the gate never actually authenticated the
/// caller anyway — this contract is called by Beanie's own sponsoring keeper wallet on
/// the merchant's behalf (see worker.rs), same as registerMerchant(); msg.sender was
/// never the merchant. So it stopped nobody from setting a webhook for a real
/// merchant's address, only from setting one for a nonexistent address, which does no
/// harm since nothing ever looks that URL up. Same trust model as the rest of Beanie:
/// permissionless, no admin key. Off-chain rate limiting is the actual spam control
/// (beanie_api's DualRateLimiter), not an on-chain check that can't generalize across
/// chains anyway.
contract MerchantWebhookRegistry {
    mapping(address => string) public webhookUrl;

    // Indexed address, not string — a string here would store as a keccak256 hash in
    // the topic, not a recoverable address, which would break evm_keeper's existing
    // `Address::from(log.topics[1])` decode in discover_webhook_urls().
    event WebhookUrlSet(address indexed merchant, string url);

    error EmptyUrl();

    function setWebhookUrl(address merchant, string calldata url) external {
        if (bytes(url).length == 0) revert EmptyUrl();
        webhookUrl[merchant] = url;
        emit WebhookUrlSet(merchant, url);
    }
}

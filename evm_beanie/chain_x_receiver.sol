// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/*
 * Chain-X Receiver (EVM leg — Base, Ethereum, or any CCTP V2 EVM chain)
 *
 * Same three contracts, same shape, same security model as the Starknet
 * and Solana legs:
 *   - every destination pinned immutable at initialize()
 *   - sweep() is permissionless, idempotent, atomic
 *   - fee destinations checked by exact address match
 *   - net leg burns via CCTP instead of a local transfer
 *
 * This is the simplest of the three receivers. EVM's CCTP interface needs
 * no fresh keypairs and no rent-funded event accounts the way Solana's
 * does — depositForBurn is one plain external call.
 *
 * Verified against: github.com/circlefin/evm-cctp-contracts,
 * src/v2/TokenMessengerV2.sol — signature matches exactly, including the
 * Starknet-side interface given earlier (CCTP V2 uses one consistent
 * function shape across EVM chains and Starknet).
 */

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

interface ITokenMessengerV2 {
    function depositForBurn(
        uint256 amount,
        uint32 destinationDomain,
        bytes32 mintRecipient,
        address burnToken,
        bytes32 destinationCaller,
        uint256 maxFee,
        uint32 minFinalityThreshold
    ) external;
}

contract ChainXReceiver {
    using SafeERC20 for IERC20;

    uint256 public constant FEE_BPS = 50; // 0.50%
    uint256 public constant BPS_DENOM = 10_000;

    // ── Immutable after initialize(). Nothing below can ever change —
    // same rationale as the Starknet and Solana versions: an updatable
    // destination is a redirect vector, so there is deliberately no admin
    // function to touch any of this after the one-time setup call.
    bool public initialized;
    address public token;
    address public walletA;
    address public walletB;

    address public tokenMessenger;
    uint32 public cctpDestinationDomain; // Starknet's CCTP domain
    bytes32 public cctpMintRecipient;    // the merchant's Starknet receiver,
                                          // encoded as bytes32 — Starknet
                                          // addresses fit in a felt252

    event Initialized(
        address token, address walletA, address walletB,
        uint32 cctpDestinationDomain, bytes32 cctpMintRecipient
    );
    event Swept(
        uint256 grossAmount, uint256 netAmount, uint256 feeAmount,
        uint256 feeToWalletA, uint256 feeToWalletB
    );

    error AlreadyInitialized();
    error NotInitialized();
    error WalletsMustDiffer();
    error ZeroAddress();

    /// Run once after deploy. Pins every destination, including the CCTP
    /// bridge target, permanently.
    function initialize(
        address _token,
        address _walletA,
        address _walletB,
        address _tokenMessenger,
        uint32 _cctpDestinationDomain,
        bytes32 _cctpMintRecipient
    ) external {
        if (initialized) revert AlreadyInitialized();
        if (_token == address(0) || _walletA == address(0) || _walletB == address(0)
            || _tokenMessenger == address(0)) revert ZeroAddress();
        if (_cctpMintRecipient == bytes32(0)) revert ZeroAddress();
        if (_walletA == _walletB) revert WalletsMustDiffer();

        initialized = true;
        token = _token;
        walletA = _walletA;
        walletB = _walletB;
        tokenMessenger = _tokenMessenger;
        cctpDestinationDomain = _cctpDestinationDomain;
        cctpMintRecipient = _cctpMintRecipient;

        emit Initialized(_token, _walletA, _walletB, _cctpDestinationDomain, _cctpMintRecipient);
    }

    /// PERMISSIONLESS, IDEMPOTENT, atomic. Zero balance -> silent no-op.
    ///   fee = balance * FEE_BPS / BPS_DENOM  -> split 60/40 to walletA/B
    ///   net = balance - fee                  -> burned via CCTP, minted to
    ///                                            the pinned Starknet receiver
    function sweep() external returns (uint256 net, uint256 toA, uint256 toB, uint256 fee) {
        if (!initialized) revert NotInitialized();

        uint256 balance = IERC20(token).balanceOf(address(this));
        if (balance == 0) {
            return (0, 0, 0, 0); // idempotent, same as the Starknet and Solana versions
        }

        // Solidity 0.8+ reverts on overflow/underflow natively — no
        // separate checked_* calls needed, same as Cairo's u256.
        fee = (balance * FEE_BPS) / BPS_DENOM;
        net = balance - fee;
        toA = (fee * 60) / 100;
        toB = fee - toA;

        if (toA > 0) IERC20(token).safeTransfer(walletA, toA);
        if (toB > 0) IERC20(token).safeTransfer(walletB, toB);

        // net → CCTP burn, targeting the pinned Starknet receiver.
        // No PDA-signing trick needed here — this contract IS the token
        // holder, so it just approves and calls directly.
        IERC20(token).forceApprove(tokenMessenger, net);
        ITokenMessengerV2(tokenMessenger).depositForBurn(
            net,
            cctpDestinationDomain,
            cctpMintRecipient,
            token,
            bytes32(0), // destination_caller: 0 = permissionless mint on Starknet
            0,          // max_fee: 0 = standard finality, no fast-burn premium
            2000        // min_finality_threshold: standard
        );

        emit Swept(balance, net, fee, toA, toB);
    }
}

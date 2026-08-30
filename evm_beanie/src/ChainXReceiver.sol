// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

/*
 * Chain-X Receiver (EVM leg — Base, Ethereum, or any CCTP V2 EVM chain)
 *
 * Same three contracts, same shape, same security model as the Starknet
 * and Solana legs:
 *   - every destination pinned immutable at initialize()
 *   - sweep() is permissionless, idempotent, atomic
 *   - net leg burns via CCTP instead of a local transfer
 *
 * Fee split: sweep() has no dedicated relayer the way Starknet's
 * privacy_invoke does (that one is driven by STRK20 pool nodes), so the
 * caller who triggers it here is paid directly out of the fee — 10% of
 * fee to msg.sender, the remaining 90% to a single treasury address.
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

    uint256 public constant FEE_BPS = 50; // 0.50% of gross
    uint256 public constant BPS_DENOM = 10_000;
    uint256 public constant CALLER_SHARE_BPS = 1_000; // 10% of the fee, not of gross

    // ── Immutable after initialize(). Nothing below can ever change —
    // same rationale as the Starknet and Solana versions: an updatable
    // destination is a redirect vector, so there is deliberately no admin
    // function to touch any of this after the one-time setup call.
    bool public initialized;
    address public token;
    address public treasury;

    address public tokenMessenger;
    uint32 public cctpDestinationDomain;
    bytes32 public cctpMintRecipient; // cross-chain mint recipient (0 = same-chain)
    // encoded as ChainX address into bytes32

    // on-chain merchant address for same-chain settlement
    address public merchant;

    event Initialized(
        address token,
        address treasury,
        uint32 cctpDestinationDomain,
        bytes32 cctpMintRecipient
    );
    event Swept(
        uint256 grossAmount,
        uint256 netAmount,
        uint256 feeAmount,
        uint256 feeToCaller,
        uint256 feeToTreasury
    );

    error AlreadyInitialized();
    error NotInitialized();
    error ZeroAddress();

    /// Run once after deploy. Pins every destination, including the CCTP
    /// bridge target, permanently.
    function initialize(
        address _token,
        address _treasury,
        address _tokenMessenger,
        uint32 _cctpDestinationDomain,
        bytes32 _cctpMintRecipient,
        address _merchant
    ) external {
        if (initialized) revert AlreadyInitialized();
        if (
            _token == address(0) ||
            _treasury == address(0) ||
            _tokenMessenger == address(0)
        ) revert ZeroAddress();
        // _cctpMintRecipient may be zero to indicate same-chain settlement
        if (_merchant == address(0)) revert ZeroAddress();

        initialized = true;
        token = _token;
        treasury = _treasury;
        tokenMessenger = _tokenMessenger;
        cctpDestinationDomain = _cctpDestinationDomain;
        cctpMintRecipient = _cctpMintRecipient;
        merchant = _merchant;

        emit Initialized(
            _token,
            _treasury,
            _cctpDestinationDomain,
            _cctpMintRecipient
        );
    }

    /// PERMISSIONLESS, IDEMPOTENT, atomic. Zero balance -> silent no-op.
    ///   fee        = balance * FEE_BPS / BPS_DENOM
    ///   feeToCaller = fee * CALLER_SHARE_BPS / BPS_DENOM   (10% of fee, to msg.sender)
    ///   feeToTreasury = fee - feeToCaller
    ///   net = balance - fee -> burned via CCTP, or transferred same-chain
    function sweep()
        external
        returns (
            uint256 net,
            uint256 feeToCaller,
            uint256 feeToTreasury,
            uint256 fee
        )
    {
        if (!initialized) revert NotInitialized();

        uint256 balance = IERC20(token).balanceOf(address(this));
        if (balance == 0) {
            return (0, 0, 0, 0); // idempotent, same as the Starknet and Solana versions
        }

        fee = (balance * FEE_BPS) / BPS_DENOM;
        net = balance - fee;
        feeToCaller = (fee * CALLER_SHARE_BPS) / BPS_DENOM;
        feeToTreasury = fee - feeToCaller;

        if (feeToCaller > 0)
            IERC20(token).safeTransfer(msg.sender, feeToCaller);
        if (feeToTreasury > 0)
            IERC20(token).safeTransfer(treasury, feeToTreasury);

        // Settlement behavior:
        // - If `cctpMintRecipient` != 0: cross-chain via CCTP burn to that recipient.
        // - If `cctpMintRecipient` == 0: same-chain settlement to `merchant` address.
        if (net > 0 && cctpMintRecipient != bytes32(0)) {
            // Derive the 2 bps FAST CCTP teleport (2 / 10,000 = 0.0002 or 0.02%)
            // 0.05 bps Over reference maximum CCTP FAST EVM finality
            uint256 max_fee = (balance * 2) / 10_000;

            IERC20(token).approve(tokenMessenger, net);
            ITokenMessengerV2(tokenMessenger).depositForBurn(
                net,
                cctpDestinationDomain,
                cctpMintRecipient,
                token,
                bytes32(0), // destination_caller: 0 = permissionless mint on Starknet
                max_fee,
                1000 // min_finality_threshold: standard
            );
        } else {
            IERC20(token).safeTransfer(merchant, net);
        }

        emit Swept(balance, net, fee, feeToCaller, feeToTreasury);
    }
}

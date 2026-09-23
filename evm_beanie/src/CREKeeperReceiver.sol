// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

/*
 * CREKeeperReceiver
 *
 * CRE's write path never calls your contracts directly — the KeystoneForwarder
 * calls onReport(bytes metadata, bytes report) on a designated IReceiver
 * contract, full stop. Multicall3.aggregate3(), ChainXReceiver.sweep(), and
 * ReceiverFactory.registerMerchant() are not IReceiver entry points, so this
 * thin relay exists purely to be the thing the Forwarder calls, decode the
 * call batch the workflow already computed off-chain, and forward it into
 * Multicall3 in one shot.
 *
 * NOTE: this hand-rolls IReceiver for clarity. In production, extend the
 * SDK's ReceiverTemplate instead — it already implements the forwarder /
 * workflow-ID / workflow-owner checks below correctly and is the documented,
 * audited path. This file exists to show exactly what those checks are
 * doing, not to replace that template.
 */

interface IReceiver {
    function onReport(bytes calldata metadata, bytes calldata report) external;
}

interface IMulticall3 {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }
    struct Result {
        bool success;
        bytes returnData;
    }
    function aggregate3(
        Call3[] calldata calls
    ) external payable returns (Result[] memory);
}

contract CREKeeperReceiver is IReceiver {
    address public immutable forwarder;
    address public immutable multicall3;

    // Set once at deploy, same immutable-after-init posture as every other
    // contract in this system — no admin function to change these later.
    bytes32 public immutable expectedWorkflowId;
    address public immutable expectedWorkflowOwner;

    event BatchExecuted(uint256 callCount, bool[] successes);

    error InvalidForwarder(address got, address expected);
    error InvalidWorkflow();

    constructor(
        address _forwarder,
        address _multicall3,
        bytes32 _expectedWorkflowId,
        address _expectedWorkflowOwner
    ) {
        forwarder = _forwarder;
        multicall3 = _multicall3;
        expectedWorkflowId = _expectedWorkflowId;
        expectedWorkflowOwner = _expectedWorkflowOwner;
    }

    /// Called by the KeystoneForwarder once the DON has reached consensus
    /// and the Forwarder has validated the report's signatures. This
    /// function is the ENTIRE trust boundary — everything past this point
    /// assumes the caller is genuinely the trusted Forwarder relaying a
    /// genuinely consensus-approved report.
    function onReport(
        bytes calldata metadata,
        bytes calldata report
    ) external override {
        if (msg.sender != forwarder)
            revert InvalidForwarder(msg.sender, forwarder);

        // metadata carries (workflowId, workflowName, workflowOwner) per the
        // CRE report envelope — decode and pin it, same reasoning as pinning
        // destinations at initialize() everywhere else in this system: an
        // unpinned workflow identity is a redirect vector for WHICH workflow
        // is allowed to move funds through this relay.
        (bytes32 workflowId, , address workflowOwner) = abi.decode(
            metadata,
            (bytes32, bytes10, address)
        );
        if (
            workflowId != expectedWorkflowId ||
            workflowOwner != expectedWorkflowOwner
        ) {
            revert InvalidWorkflow();
        }

        // The report itself is just the ABI-encoded Call3[] the workflow
        // computed off-chain — same shape as what evm_keeper.rs builds by
        // hand today, just decoded here instead of constructed in Rust.
        IMulticall3.Call3[] memory calls = abi.decode(
            report,
            (IMulticall3.Call3[])
        );

        IMulticall3.Result[] memory results = IMulticall3(multicall3)
            .aggregate3(calls);

        bool[] memory successes = new bool[](results.length);
        for (uint256 i = 0; i < results.length; i++) {
            successes[i] = results[i].success;
        }
        emit BatchExecuted(calls.length, successes);
    }
}

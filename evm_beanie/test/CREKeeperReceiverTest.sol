// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

import "forge-std/Test.sol";
import "../src/CREKeeperReceiver.sol";
import "../src/ChainXReceiver.sol";
import "../src/ReceiverFactory.sol";
import "@openzeppelin/contracts/token/ERC20/ERC20.sol";

// Mock USDC token for testing balance transfers and approvals
contract MockUSDC is ERC20 {
    constructor() ERC20("USD Coin", "USDC") {
        _mint(msg.sender, 1_000_000 * 10 ** 6);
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

// Mock Multicall3 contract
contract MockMulticall3 is IMulticall3 {
    function aggregate3(
        Call3[] calldata calls
    ) external payable override returns (Result[] memory results) {
        results = new Result[](calls.length);
        for (uint256 i = 0; i < calls.length; i++) {
            (bool success, bytes memory returnData) = calls[i].target.call(
                calls[i].callData
            );
            if (!success && !calls[i].allowFailure) {
                revert("Multicall3: call failed");
            }
            results[i] = Result(success, returnData);
        }
    }
}

contract CREKeeperReceiverTest is Test {
    CREKeeperReceiver public creReceiver;
    MockMulticall3 public multicall;
    ChainXReceiver public receiverImpl;
    ReceiverFactory public factory;
    MockUSDC public token;

    // Identities & Roles
    address public forwarder = address(0x1111);
    address public unauthorizedForwarder = address(0x9999);
    address public treasury = address(0x2222);
    address public tokenMessenger = address(0x3333);
    address public merchant = address(0x4444);
    address public relayer = address(0x5555);

    // CRE Metadata parameters
    bytes32 public constant WORKFLOW_ID = keccak256("WORKFLOW_V1");
    address public constant WORKFLOW_OWNER = address(0x6666);
    bytes10 public constant WORKFLOW_NAME = "SWEER_JOB";

    address public deployedReceiver;

    function setUp() public {
        token = new MockUSDC();
        multicall = new MockMulticall3();

        // 1. Deploy Implementation & Factory
        receiverImpl = new ChainXReceiver();
        factory = new ReceiverFactory(
            address(receiverImpl),
            address(token),
            treasury,
            tokenMessenger,
            1, // Starknet Domain
            2, // Base Domain
            3, // Solana Domain
            4 // Eth Domain
        );

        // 2. Deploy CRE Keeper Receiver entrypoint
        creReceiver = new CREKeeperReceiver(
            forwarder,
            address(multicall),
            WORKFLOW_ID,
            WORKFLOW_OWNER
        );

        // 3. Register a Same-Chain Merchant Receiver via Factory
        deployedReceiver = factory.registerMerchant(
            merchant,
            bytes32(0),
            bytes32(0)
        );
    }

    // ── CRE REPORT & ACCESS CONTROL TESTS ────────────────────────────────

    function test_OnReport_ExecutesBatchSweep() public {
        // Fund merchant's receiver contract with 10,000 USDC
        uint256 sweepAmount = 10_000 * 10 ** 6;
        token.mint(deployedReceiver, sweepAmount);

        // Encode the sweep() call targeting the deployed receiver
        IMulticall3.Call3[] memory calls = new IMulticall3.Call3[](1);
        calls[0] = IMulticall3.Call3({
            target: deployedReceiver,
            allowFailure: false,
            callData: abi.encodeWithSelector(ChainXReceiver.sweep.selector)
        });

        // Construct CRE Report Envelope (Metadata & Report)
        bytes memory metadata = abi.encode(
            WORKFLOW_ID,
            WORKFLOW_NAME,
            WORKFLOW_OWNER
        );
        bytes memory report = abi.encode(calls);

        // Expect BatchExecuted event from CREKeeperReceiver
        bool[] memory expectedSuccesses = new bool[](1);
        expectedSuccesses[0] = true;
        vm.expectEmit(true, true, true, true);
        emit CREKeeperReceiver.BatchExecuted(1, expectedSuccesses);

        // Execute report via valid Forwarder
        vm.prank(forwarder, relayer); // tx.origin set to `relayer`
        creReceiver.onReport(metadata, report);

        // Verify fee distribution (0.5% total fee = 50 USDC)
        // Relayer gets 10% of fee (5 USDC), Treasury gets 90% of fee (45 USDC), Merchant gets net (9,950 USDC)
        assertEq(token.balanceOf(relayer), 5 * 10 ** 6);
        assertEq(token.balanceOf(treasury), 45 * 10 ** 6);
        assertEq(token.balanceOf(merchant), 9_950 * 10 ** 6);
    }

    function test_RevertIf_UnauthorizedForwarder() public {
        bytes memory metadata = abi.encode(
            WORKFLOW_ID,
            WORKFLOW_NAME,
            WORKFLOW_OWNER
        );
        IMulticall3.Call3[] memory calls = new IMulticall3.Call3[](0);
        bytes memory report = abi.encode(calls);

        vm.prank(unauthorizedForwarder);
        vm.expectRevert(
            abi.encodeWithSelector(
                CREKeeperReceiver.InvalidForwarder.selector,
                unauthorizedForwarder,
                forwarder
            )
        );
        creReceiver.onReport(metadata, report);
    }

    function test_RevertIf_InvalidWorkflowId() public {
        bytes32 wrongWorkflowId = keccak256("BAD_WORKFLOW");
        bytes memory metadata = abi.encode(
            wrongWorkflowId,
            WORKFLOW_NAME,
            WORKFLOW_OWNER
        );

        IMulticall3.Call3[] memory calls = new IMulticall3.Call3[](0);
        bytes memory report = abi.encode(calls);

        vm.prank(forwarder);
        vm.expectRevert(CREKeeperReceiver.InvalidWorkflow.selector);
        creReceiver.onReport(metadata, report);
    }

    function test_RevertIf_InvalidWorkflowOwner() public {
        address wrongOwner = address(0xDEAD);
        bytes memory metadata = abi.encode(
            WORKFLOW_ID,
            WORKFLOW_NAME,
            wrongOwner
        );

        IMulticall3.Call3[] memory calls = new IMulticall3.Call3[](0);
        bytes memory report = abi.encode(calls);

        vm.prank(forwarder);
        vm.expectRevert(CREKeeperReceiver.InvalidWorkflow.selector);
        creReceiver.onReport(metadata, report);
    }

    // ── INTEGRATION & EDGE CASE TESTS ─────────────────────────────────────

    function test_OnReport_MulticallHandlesZeroBalanceSweepGracefully() public {
        // Merchant receiver has 0 balance
        IMulticall3.Call3[] memory calls = new IMulticall3.Call3[](1);
        calls[0] = IMulticall3.Call3({
            target: deployedReceiver,
            allowFailure: false,
            callData: abi.encodeWithSelector(ChainXReceiver.sweep.selector)
        });

        bytes memory metadata = abi.encode(
            WORKFLOW_ID,
            WORKFLOW_NAME,
            WORKFLOW_OWNER
        );
        bytes memory report = abi.encode(calls);

        // Sweep should execute idempotently without reverting
        vm.prank(forwarder);
        creReceiver.onReport(metadata, report);

        assertEq(token.balanceOf(merchant), 0);
    }
}

// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "forge-std/Test.sol";
import "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import "../src/StealthAccount.sol";
import "../src/StealthFactory.sol";

interface IEntryPointV07 {
    function handleOps(
        PackedUserOperation[] calldata ops,
        address payable beneficiary
    ) external;
    function getUserOpHash(
        PackedUserOperation calldata userOp
    ) external view returns (bytes32);
    function getNonce(
        address sender,
        uint192 key
    ) external view returns (uint256);
}

contract ForkToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/**
 * Runs the account + factory against the REAL canonical EntryPoint v0.7.
 *
 *   BASE_RPC_URL=https://mainnet.base.org forge test \
 *       --match-path test/StealthAccountFork.t.sol -vvv
 *
 * Skipped automatically when BASE_RPC_URL is not set. Any chain that has the
 * v0.7 EntryPoint works (set the env var to that chain's RPC).
 */
contract StealthAccountForkTest is Test {
    using MessageHashUtils for bytes32;

    address constant ENTRY_POINT = 0x0000000071727De22E5E9d8BAf0edAc6f37da032;
    IEntryPointV07 ep = IEntryPointV07(ENTRY_POINT);

    bool forked;
    StealthAccountFactory factory;
    ForkToken token;

    uint256 clientPk = 0xA11CE;
    uint256 cosignerPk = 0xB0B;
    uint256 otherPk = 0xBAD;
    address client;
    address cosigner;
    address bundler = address(0xB0DE);
    address recipient = address(0xCAFE);
    bytes32 constant SALT = keccak256("fork-salt");

    function setUp() public {
        string memory url = vm.envOr("BASE_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url);
        require(
            ENTRY_POINT.code.length > 0,
            "EntryPoint v0.7 not deployed on this chain"
        );
        forked = true;

        client = vm.addr(clientPk);
        cosigner = vm.addr(cosignerPk);
        factory = new StealthAccountFactory(ENTRY_POINT, cosigner);
        token = new ForkToken();
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    function _initCode() internal view returns (bytes memory) {
        return
            abi.encodePacked(
                address(factory),
                abi.encodeCall(
                    StealthAccountFactory.createAccount,
                    (client, SALT)
                )
            );
    }

    function _transferCall(
        uint256 amount
    ) internal view returns (bytes memory) {
        return
            abi.encodeCall(
                StealthAccount.execute,
                (
                    address(token),
                    0,
                    abi.encodeCall(ForkToken.transfer, (recipient, amount))
                )
            );
    }

    /// v0.7 packing: verificationGasLimit(16) | callGasLimit(16); maxPriorityFee(16) | maxFee(16)
    function _op(
        address sender,
        uint256 nonce,
        bytes memory initCode,
        bytes memory callData
    ) internal pure returns (PackedUserOperation memory) {
        return
            PackedUserOperation({
                sender: sender,
                nonce: nonce,
                initCode: initCode,
                callData: callData,
                accountGasLimits: bytes32(
                    (uint256(1_500_000) << 128) | uint256(200_000)
                ),
                preVerificationGas: 100_000,
                gasFees: bytes32((uint256(1 gwei) << 128) | uint256(1 gwei)),
                paymasterAndData: "",
                signature: ""
            });
    }

    function _sign(
        bytes32 userOpHash,
        uint256 pkClient,
        uint256 pkCosigner
    ) internal pure returns (bytes memory) {
        bytes32 h = userOpHash.toEthSignedMessageHash(); // same digest the worker has both signers sign
        (uint8 v1, bytes32 r1, bytes32 s1) = vm.sign(pkClient, h);
        (uint8 v2, bytes32 r2, bytes32 s2) = vm.sign(pkCosigner, h);
        return abi.encodePacked(r1, s1, v1, r2, s2, v2);
    }

    function _submit(PackedUserOperation memory op) internal {
        PackedUserOperation[] memory ops = new PackedUserOperation[](1);
        ops[0] = op;
        vm.prank(bundler, bundler);
        ep.handleOps(ops, payable(bundler));
    }

    /// Funds the counterfactual account and submits a signed first claim.
    function _firstClaim(uint256 amount) internal returns (address sender) {
        sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        vm.deal(sender, 0.01 ether); // no paymaster here: account prefunds its own gas

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(),
            _transferCall(amount)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, cosignerPk);
        _submit(op);
    }

    /// Mirrors `evm_user_op_hash` in the Rust worker.
    function _localHash(
        PackedUserOperation memory op
    ) internal view returns (bytes32) {
        bytes32 inner = keccak256(
            abi.encode(
                op.sender,
                op.nonce,
                keccak256(op.initCode),
                keccak256(op.callData),
                op.accountGasLimits,
                op.preVerificationGas,
                op.gasFees,
                keccak256(op.paymasterAndData)
            )
        );
        return keccak256(abi.encode(inner, ENTRY_POINT, block.chainid));
    }

    // ------------------------------------------------------------------
    // tests
    // ------------------------------------------------------------------

    function test_Fork_LocalUserOpHashMatchesEntryPoint() public {
        vm.skip(!forked);
        address sender = factory.getAddress(client, SALT);
        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(),
            _transferCall(1)
        );
        assertEq(_localHash(op), ep.getUserOpHash(op));
    }

    function test_Fork_FirstClaimDeploysViaRealEntryPoint() public {
        vm.skip(!forked);
        address sender = _firstClaim(400e6);

        assertGt(sender.code.length, 0);
        assertEq(token.balanceOf(recipient), 400e6);
        assertEq(token.balanceOf(sender), 600e6);
        assertEq(ep.getNonce(sender, 0), 1);
    }

    function test_Fork_SecondClaimWithEmptyInitCode() public {
        vm.skip(!forked);
        address sender = _firstClaim(400e6);

        PackedUserOperation memory op = _op(
            sender,
            1,
            "",
            _transferCall(100e6)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, cosignerPk);
        _submit(op);

        assertEq(token.balanceOf(recipient), 500e6);
        assertEq(ep.getNonce(sender, 0), 2);
    }

    function test_Fork_InitCodeAfterDeploymentIsRejected() public {
        vm.skip(!forked);
        address sender = _firstClaim(400e6);

        PackedUserOperation memory op = _op(
            sender,
            1,
            _initCode(),
            _transferCall(1)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, cosignerPk);

        vm.expectRevert(
            abi.encodeWithSignature(
                "FailedOp(uint256,string)",
                uint256(0),
                "AA10 sender already constructed"
            )
        );
        _submit(op);
    }

    function test_Fork_BadCosignerIsRejectedAndNothingDeploys() public {
        vm.skip(!forked);
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        vm.deal(sender, 0.01 ether);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(),
            _transferCall(400e6)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, otherPk);

        vm.expectRevert(
            abi.encodeWithSignature(
                "FailedOp(uint256,string)",
                uint256(0),
                "AA24 signature error"
            )
        );
        _submit(op);

        assertEq(sender.code.length, 0);
        assertEq(token.balanceOf(recipient), 0);
    }

    function test_Fork_ReplayedSignatureIsRejected() public {
        vm.skip(!forked);
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        vm.deal(sender, 0.01 ether);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(),
            _transferCall(400e6)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, cosignerPk);
        _submit(op);

        // Same op again: nonce 0 is spent (and initCode is stale), so it cannot execute twice
        vm.expectRevert();
        _submit(op);
        assertEq(token.balanceOf(recipient), 400e6);
    }

    function test_Fork_TamperedCallDataInvalidatesSignature() public {
        vm.skip(!forked);
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        vm.deal(sender, 0.01 ether);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(),
            _transferCall(1e6)
        );
        op.signature = _sign(ep.getUserOpHash(op), clientPk, cosignerPk);
        op.callData = _transferCall(999e6); // attacker swaps the amount after signing

        vm.expectRevert(
            abi.encodeWithSignature(
                "FailedOp(uint256,string)",
                uint256(0),
                "AA24 signature error"
            )
        );
        _submit(op);
    }
}

// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "forge-std/Test.sol";
import "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import "../src/StealthAccount.sol";
import "../src/StealthFactory.sol";

contract MockToken {
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

/// Mimics what the real EntryPoint v0.7 does with `initCode`, then validates and executes.
/// (The real EntryPoint is exercised in StealthAccountFork.t.sol.)
contract MockEntryPoint {
    error AccountNotCreated();
    error SenderMismatch();
    error BadSignature();
    error CallFailed();

    function handleOp(
        PackedUserOperation calldata op,
        bytes32 userOpHash
    ) external {
        if (op.initCode.length > 0) {
            if (op.sender.code.length > 0)
                revert("AA10 sender already constructed");
            address factory = address(bytes20(op.initCode[:20]));
            (bool ok, bytes memory ret) = factory.call(op.initCode[20:]);
            if (!ok || ret.length < 32) revert AccountNotCreated();
            if (abi.decode(ret, (address)) != op.sender)
                revert SenderMismatch();
        }
        if (IAccount(op.sender).validateUserOp(op, userOpHash, 0) != 0)
            revert BadSignature();
        (bool success, ) = op.sender.call(op.callData);
        if (!success) revert CallFailed();
    }
}

contract StealthAccountFactoryTest is Test {
    using MessageHashUtils for bytes32;

    MockEntryPoint ep;
    StealthAccountFactory factory;
    MockToken token;

    uint256 clientPk = 0xA11CE;
    uint256 cosignerPk = 0xB0B;
    uint256 otherPk = 0xBAD;

    address client;
    address cosigner;
    address recipient = address(0xCAFE);
    bytes32 constant SALT = keccak256("salt-1");

    function setUp() public {
        client = vm.addr(clientPk);
        cosigner = vm.addr(cosignerPk);
        ep = new MockEntryPoint();
        factory = new StealthAccountFactory(address(ep), cosigner);
        token = new MockToken();
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    function _initCode(
        StealthAccountFactory f,
        address c,
        bytes32 s
    ) internal pure returns (bytes memory) {
        return
            abi.encodePacked(
                address(f),
                abi.encodeCall(StealthAccountFactory.createAccount, (c, s))
            );
    }

    function _sign(
        bytes32 hash,
        uint256 pkClient,
        uint256 pkCosigner
    ) internal pure returns (bytes memory) {
        bytes32 h = hash.toEthSignedMessageHash();
        (uint8 v1, bytes32 r1, bytes32 s1) = vm.sign(pkClient, h);
        (uint8 v2, bytes32 r2, bytes32 s2) = vm.sign(pkCosigner, h);
        return abi.encodePacked(r1, s1, v1, r2, s2, v2);
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
                    abi.encodeCall(MockToken.transfer, (recipient, amount))
                )
            );
    }

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
                accountGasLimits: bytes32(0),
                preVerificationGas: 0,
                gasFees: bytes32(0),
                paymasterAndData: "",
                signature: ""
            });
    }

    // ------------------------------------------------------------------
    // constructor
    // ------------------------------------------------------------------

    function test_Constructor_StoresImmutables() public view {
        assertEq(factory.entryPoint(), address(ep));
        assertEq(factory.cosigner(), cosigner);
    }

    function test_Constructor_RevertsOnZeroAddress() public {
        vm.expectRevert(StealthAccountFactory.ZeroAddress.selector);
        new StealthAccountFactory(address(0), cosigner);

        vm.expectRevert(StealthAccountFactory.ZeroAddress.selector);
        new StealthAccountFactory(address(ep), address(0));
    }

    // ------------------------------------------------------------------
    // getAddress / createAccount
    // ------------------------------------------------------------------

    function test_CreateAccount_DeploysAtPredictedAddress() public {
        address predicted = factory.getAddress(client, SALT);
        assertEq(predicted.code.length, 0);

        address created = factory.createAccount(client, SALT);

        assertEq(created, predicted);
        assertGt(created.code.length, 0);
    }

    function test_CreateAccount_SetsImmutablesCorrectly() public {
        StealthAccount acct = StealthAccount(
            payable(factory.createAccount(client, SALT))
        );
        assertEq(acct.entryPoint(), address(ep));
        assertEq(acct.clientPubkey(), client);
        assertEq(acct.cosignerPubkey(), cosigner);
    }

    function test_CreateAccount_IsIdempotent() public {
        address first = factory.createAccount(client, SALT);
        bytes32 codehash = first.codehash;

        address second = factory.createAccount(client, SALT);

        assertEq(second, first);
        assertEq(second.codehash, codehash);
    }

    function test_CreateAccount_IsPermissionless_ButStillBoundToFactoryCosigner()
        public
    {
        vm.prank(address(0xBEEF)); // anyone can trigger deployment
        StealthAccount acct = StealthAccount(
            payable(factory.createAccount(client, SALT))
        );

        assertEq(acct.cosignerPubkey(), cosigner);
        assertEq(acct.clientPubkey(), client);
    }

    function test_GetAddress_IndependentOfCaller() public {
        address a = factory.getAddress(client, SALT);
        vm.prank(address(0xBEEF));
        address b = factory.getAddress(client, SALT);
        assertEq(a, b);
    }

    function test_GetAddress_DiffersPerClientAndSalt() public view {
        address base = factory.getAddress(client, SALT);
        assertTrue(base != factory.getAddress(vm.addr(otherPk), SALT));
        assertTrue(base != factory.getAddress(client, keccak256("salt-2")));
    }

    function test_GetAddress_DiffersPerCosigner() public {
        StealthAccountFactory other = new StealthAccountFactory(
            address(ep),
            vm.addr(otherPk)
        );
        assertTrue(
            factory.getAddress(client, SALT) != other.getAddress(client, SALT)
        );
    }

    function test_CreateAccount_RevertsForZeroClient() public {
        vm.expectRevert(StealthAccount.ZeroAddress.selector);
        factory.createAccount(address(0), SALT);
    }

    function test_PreDeployFundsSurviveDeployment() public {
        address predicted = factory.getAddress(client, SALT);
        token.mint(predicted, 1_000e6);
        vm.deal(predicted, 1 ether);

        factory.createAccount(client, SALT);

        assertEq(token.balanceOf(predicted), 1_000e6);
        assertEq(predicted.balance, 1 ether);
    }

    function testFuzz_GetAddressMatchesCreateAccount(
        address c,
        bytes32 s
    ) public {
        vm.assume(c != address(0));
        address predicted = factory.getAddress(c, s);
        assertEq(factory.createAccount(c, s), predicted);
    }

    // ------------------------------------------------------------------
    // initCode wire format (what the client SDK must produce)
    // ------------------------------------------------------------------

    function test_InitCode_Layout() public view {
        bytes memory ic = _initCode(factory, client, SALT);
        assertEq(address(bytes20(ic)), address(factory));
        assertEq(ic.length, 20 + 4 + 32 + 32); // factory + selector + 2 words
    }

    // ------------------------------------------------------------------
    // End-to-end via the mock EntryPoint: first claim deploys + transfers
    // ------------------------------------------------------------------

    function test_FirstClaim_DeploysAndTransfers() public {
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        assertEq(sender.code.length, 0);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(factory, client, SALT),
            _transferCall(400e6)
        );
        bytes32 h = keccak256("userOpHash-1");
        op.signature = _sign(h, clientPk, cosignerPk);

        ep.handleOp(op, h);

        assertGt(sender.code.length, 0);
        assertEq(token.balanceOf(recipient), 400e6);
        assertEq(token.balanceOf(sender), 600e6);
    }

    function test_SecondClaim_WithEmptyInitCode_Succeeds() public {
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);

        PackedUserOperation memory op1 = _op(
            sender,
            0,
            _initCode(factory, client, SALT),
            _transferCall(400e6)
        );
        bytes32 h1 = keccak256("userOpHash-1");
        op1.signature = _sign(h1, clientPk, cosignerPk);
        ep.handleOp(op1, h1);

        PackedUserOperation memory op2 = _op(
            sender,
            1,
            "",
            _transferCall(100e6)
        );
        bytes32 h2 = keccak256("userOpHash-2");
        op2.signature = _sign(h2, clientPk, cosignerPk);
        ep.handleOp(op2, h2);

        assertEq(token.balanceOf(recipient), 500e6);
    }

    function test_InitCodeAfterDeployment_Reverts() public {
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);
        factory.createAccount(client, SALT);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(factory, client, SALT),
            _transferCall(1)
        );
        bytes32 h = keccak256("userOpHash-x");
        op.signature = _sign(h, clientPk, cosignerPk);

        vm.expectRevert(bytes("AA10 sender already constructed"));
        ep.handleOp(op, h);
    }

    function test_FirstClaim_WrongCosigner_RevertsAndLeavesNoAccount() public {
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(factory, client, SALT),
            _transferCall(400e6)
        );
        bytes32 h = keccak256("userOpHash-1");
        op.signature = _sign(h, clientPk, otherPk); // not our cosigner

        vm.expectRevert(MockEntryPoint.BadSignature.selector);
        ep.handleOp(op, h);

        assertEq(sender.code.length, 0); // whole op rolled back, funds untouched
        assertEq(token.balanceOf(sender), 1_000e6);
    }

    function test_FirstClaim_WrongClientKey_Reverts() public {
        address sender = factory.getAddress(client, SALT);
        token.mint(sender, 1_000e6);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(factory, client, SALT),
            _transferCall(400e6)
        );
        bytes32 h = keccak256("userOpHash-1");
        op.signature = _sign(h, otherPk, cosignerPk);

        vm.expectRevert(MockEntryPoint.BadSignature.selector);
        ep.handleOp(op, h);
    }

    function test_FirstClaim_SenderMismatch_Reverts() public {
        // initCode deploys the account for `client`, but the op claims another sender
        PackedUserOperation memory op = _op(
            address(0x1234),
            0,
            _initCode(factory, client, SALT),
            _transferCall(1)
        );
        bytes32 h = keccak256("userOpHash-1");
        op.signature = _sign(h, clientPk, cosignerPk);

        vm.expectRevert(MockEntryPoint.SenderMismatch.selector);
        ep.handleOp(op, h);
    }

    /// Why the factory pins the cosigner: an account from a factory with a DIFFERENT
    /// cosigner cannot be driven with OUR cosigner's signature.
    function test_AccountFromForeignFactory_RejectsOurCosignature() public {
        StealthAccountFactory foreign = new StealthAccountFactory(
            address(ep),
            vm.addr(otherPk)
        );
        address sender = foreign.getAddress(client, SALT);
        token.mint(sender, 1_000e6);

        PackedUserOperation memory op = _op(
            sender,
            0,
            _initCode(foreign, client, SALT),
            _transferCall(400e6)
        );
        bytes32 h = keccak256("userOpHash-1");
        op.signature = _sign(h, clientPk, cosignerPk); // our cosigner, foreign account

        vm.expectRevert(MockEntryPoint.BadSignature.selector);
        ep.handleOp(op, h);
        assertEq(token.balanceOf(recipient), 0);
    }
}

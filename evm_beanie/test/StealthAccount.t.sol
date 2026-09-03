// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "forge-std/Test.sol";
import "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import "../src/StealthAccount.sol";

contract MockTarget {
    uint256 public valueReceived;
    bytes public dataReceived;
    uint256 public count;

    function mockCall(uint256 amount) external payable returns (uint256) {
        valueReceived += msg.value;
        count += amount;
        return count;
    }

    function failingCall() external pure {
        revert("MOCK_FAILURE");
    }

    receive() external payable {
        valueReceived += msg.value;
    }
}

contract StealthAccountTest is Test {
    using MessageHashUtils for bytes32;

    StealthAccount public account;
    MockTarget public target;

    address public entryPoint = address(0x4337);

    // Private keys for signers
    uint256 public clientPrivateKey = 0xA11CE;
    uint256 public cosignerPrivateKey = 0xB0B;
    uint256 public unauthorizedPrivateKey = 0xBAD;

    address public clientPubkey;
    address public cosignerPubkey;
    address public unauthorizedPubkey;

    function setUp() public {
        clientPubkey = vm.addr(clientPrivateKey);
        cosignerPubkey = vm.addr(cosignerPrivateKey);
        unauthorizedPubkey = vm.addr(unauthorizedPrivateKey);

        account = new StealthAccount(entryPoint, clientPubkey, cosignerPubkey);
        target = new MockTarget();

        // Fund account for gas / execution tests
        vm.deal(address(account), 10 ether);
    }

    // =========================================================================
    // Helper Functions
    // =========================================================================

    function _signHash(
        bytes32 ethSignedHash,
        uint256 pk
    ) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, ethSignedHash);
        return abi.encodePacked(r, s, v);
    }

    function _create2Of2Signature(
        bytes32 hash,
        uint256 clientPk,
        uint256 cosignerPk
    ) internal pure returns (bytes memory) {
        bytes32 ethHash = hash.toEthSignedMessageHash();
        bytes memory clientSig = _signHash(ethHash, clientPk);
        bytes memory cosignerSig = _signHash(ethHash, cosignerPk);
        return abi.encodePacked(clientSig, cosignerSig);
    }

    function _getEmptyUserOp()
        internal
        view
        returns (PackedUserOperation memory userOp)
    {
        userOp = PackedUserOperation({
            sender: address(account),
            nonce: 0,
            initCode: "",
            callData: "",
            accountGasLimits: bytes32(0),
            preVerificationGas: 0,
            gasFees: bytes32(0),
            paymasterAndData: "",
            signature: ""
        });
    }

    // =========================================================================
    // Constructor & Initialization Tests
    // =========================================================================

    function test_Constructor_Success() public view {
        assertEq(account.entryPoint(), entryPoint);
        assertEq(account.clientPubkey(), clientPubkey);
        assertEq(account.cosignerPubkey(), cosignerPubkey);
    }

    function test_Constructor_RevertZeroAddress() public {
        vm.expectRevert(StealthAccount.ZeroAddress.selector);
        new StealthAccount(address(0), clientPubkey, cosignerPubkey);

        vm.expectRevert(StealthAccount.ZeroAddress.selector);
        new StealthAccount(entryPoint, address(0), cosignerPubkey);

        vm.expectRevert(StealthAccount.ZeroAddress.selector);
        new StealthAccount(entryPoint, clientPubkey, address(0));
    }

    // =========================================================================
    // ERC-4337 validateUserOp Tests
    // =========================================================================

    function test_ValidateUserOp_Success() public {
        PackedUserOperation memory userOp = _getEmptyUserOp();
        bytes32 userOpHash = keccak256("userOpHash");

        userOp.signature = _create2Of2Signature(
            userOpHash,
            clientPrivateKey,
            cosignerPrivateKey
        );

        vm.prank(entryPoint);
        uint256 validationData = account.validateUserOp(userOp, userOpHash, 0);

        assertEq(validationData, 0); // SIG_VALIDATION_SUCCESS
    }

    function test_ValidateUserOp_PaysMissingAccountFunds() public {
        PackedUserOperation memory userOp = _getEmptyUserOp();
        bytes32 userOpHash = keccak256("userOpHash");
        userOp.signature = _create2Of2Signature(
            userOpHash,
            clientPrivateKey,
            cosignerPrivateKey
        );

        uint256 missingFunds = 1 ether;
        uint256 initialEntryPointBalance = entryPoint.balance;

        vm.prank(entryPoint);
        uint256 validationData = account.validateUserOp(
            userOp,
            userOpHash,
            missingFunds
        );

        assertEq(validationData, 0);
        assertEq(entryPoint.balance, initialEntryPointBalance + missingFunds);
    }

    function test_ValidateUserOp_InvalidSignature_ReturnsFailureFlag() public {
        PackedUserOperation memory userOp = _getEmptyUserOp();
        bytes32 userOpHash = keccak256("userOpHash");

        // Signed with wrong client key
        userOp.signature = _create2Of2Signature(
            userOpHash,
            unauthorizedPrivateKey,
            cosignerPrivateKey
        );

        vm.prank(entryPoint);
        uint256 validationData = account.validateUserOp(userOp, userOpHash, 0);

        assertEq(validationData, 1); // SIG_VALIDATION_FAILED
    }

    function test_ValidateUserOp_InvalidLength_ReturnsFailureFlag() public {
        PackedUserOperation memory userOp = _getEmptyUserOp();
        bytes32 userOpHash = keccak256("userOpHash");

        userOp.signature = hex"1234567890"; // Invalid length != 130 bytes

        vm.prank(entryPoint);
        uint256 validationData = account.validateUserOp(userOp, userOpHash, 0);

        assertEq(validationData, 1);
    }

    function test_ValidateUserOp_RevertIfUnauthorizedCaller() public {
        PackedUserOperation memory userOp = _getEmptyUserOp();
        bytes32 userOpHash = keccak256("userOpHash");

        vm.prank(address(0xDEAD));
        vm.expectRevert(StealthAccount.Unauthorized.selector);
        account.validateUserOp(userOp, userOpHash, 0);
    }

    // =========================================================================
    // Execution Tests (Legacy & ERC-7579)
    // =========================================================================

    function test_Execute_Single_Success() public {
        bytes memory data = abi.encodeWithSelector(
            MockTarget.mockCall.selector,
            42
        );

        vm.prank(entryPoint);
        bytes memory result = account.execute(address(target), 0.5 ether, data);

        assertEq(target.valueReceived(), 0.5 ether);
        assertEq(target.count(), 42);
        assertEq(abi.decode(result, (uint256)), 42);
    }

    function test_Execute_SelfCall_Success() public {
        bytes memory data = abi.encodeWithSelector(
            MockTarget.mockCall.selector,
            10
        );

        vm.prank(address(account)); // Account calling itself
        account.execute(address(target), 0, data);

        assertEq(target.count(), 10);
    }

    function test_Execute_RevertUnauthorized() public {
        vm.prank(address(0xBAD));
        vm.expectRevert(StealthAccount.Unauthorized.selector);
        account.execute(address(target), 0, "");
    }

    function test_Execute_RevertOnTargetFailure() public {
        bytes memory data = abi.encodeWithSelector(
            MockTarget.failingCall.selector
        );

        vm.prank(entryPoint);
        vm.expectRevert("MOCK_FAILURE");
        account.execute(address(target), 0, data);
    }

    function test_ExecuteBatch_Success() public {
        Execution[] memory executions = new Execution[](2);
        executions[0] = Execution({
            target: address(target),
            value: 0.1 ether,
            callData: abi.encodeWithSelector(MockTarget.mockCall.selector, 1)
        });
        executions[1] = Execution({
            target: address(target),
            value: 0.2 ether,
            callData: abi.encodeWithSelector(MockTarget.mockCall.selector, 2)
        });

        vm.prank(entryPoint);
        bytes[] memory returnData = account.executeBatch(executions);

        assertEq(returnData.length, 2);
        assertEq(target.valueReceived(), 0.3 ether);
        assertEq(target.count(), 3);
    }

    function test_ExecuteFromExecutor_ModeSingle_Success() public {
        bytes32 mode = bytes32(
            0x0000000000000000000000000000000000000000000000000000000000000000
        );
        bytes memory calldataPayload = abi.encode(
            address(target),
            uint256(0.5 ether),
            abi.encodeWithSelector(MockTarget.mockCall.selector, 100)
        );

        vm.prank(entryPoint);
        bytes[] memory returnData = account.executeFromExecutor(
            mode,
            calldataPayload
        );

        assertEq(returnData.length, 1);
        assertEq(target.valueReceived(), 0.5 ether);
        assertEq(target.count(), 100);
    }

    function test_ExecuteFromExecutor_ModeBatch_Success() public {
        bytes32 mode = bytes32(
            0x0100000000000000000000000000000000000000000000000000000000000000
        );

        Execution[] memory executions = new Execution[](2);
        executions[0] = Execution(
            address(target),
            0.1 ether,
            abi.encodeWithSelector(MockTarget.mockCall.selector, 5)
        );
        executions[1] = Execution(
            address(target),
            0.2 ether,
            abi.encodeWithSelector(MockTarget.mockCall.selector, 10)
        );

        bytes memory calldataPayload = abi.encode(executions);

        vm.prank(entryPoint);
        bytes[] memory returnData = account.executeFromExecutor(
            mode,
            calldataPayload
        );

        assertEq(returnData.length, 2);
        assertEq(target.count(), 15);
    }

    function test_ExecuteFromExecutor_UnsupportedMode_Reverts() public {
        bytes32 invalidMode = bytes32(
            0x0200000000000000000000000000000000000000000000000000000000000000
        );

        vm.prank(entryPoint);
        vm.expectRevert(StealthAccount.UnsupportedExecutionMode.selector);
        account.executeFromExecutor(invalidMode, "");
    }

    // =========================================================================
    // ERC-1271 isValidSignature Tests
    // =========================================================================

    function test_IsValidSignature_Success() public view {
        bytes32 msgHash = keccak256("MessageToSign");
        bytes memory signature = _create2Of2Signature(
            msgHash,
            clientPrivateKey,
            cosignerPrivateKey
        );

        bytes4 magicValue = account.isValidSignature(msgHash, signature);
        assertEq(magicValue, bytes4(0x1626ba7e)); // ERC1271_MAGIC_VALUE
    }

    function test_IsValidSignature_InvalidCosigner_Fails() public view {
        bytes32 msgHash = keccak256("MessageToSign");
        bytes memory signature = _create2Of2Signature(
            msgHash,
            clientPrivateKey,
            unauthorizedPrivateKey // Wrong TEE key
        );

        bytes4 magicValue = account.isValidSignature(msgHash, signature);
        assertEq(magicValue, bytes4(0xffffffff)); // ERC1271_INVALID
    }

    function test_IsValidSignature_InvalidLength_Fails() public view {
        bytes32 msgHash = keccak256("MessageToSign");
        bytes memory shortSig = new bytes(65);

        bytes4 magicValue = account.isValidSignature(msgHash, shortSig);
        assertEq(magicValue, bytes4(0xffffffff));
    }

    // =========================================================================
    // Native Funds Receipt
    // =========================================================================

    function test_ReceiveNativeETH() public {
        uint256 balanceBefore = address(account).balance;
        (bool ok, ) = address(account).call{value: 1 ether}("");
        assertTrue(ok);
        assertEq(address(account).balance, balanceBefore + 1 ether);
    }
}

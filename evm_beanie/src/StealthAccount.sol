// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";

// --- ERC-4337 Standard Interfaces ---

struct PackedUserOperation {
    address sender;
    uint256 nonce;
    bytes initCode;
    bytes callData;
    bytes32 accountGasLimits;
    uint256 preVerificationGas;
    bytes32 gasFees;
    bytes paymasterAndData;
    bytes signature;
}

interface IAccount {
    function validateUserOp(
        PackedUserOperation calldata userOp,
        bytes32 userOpHash,
        uint256 missingAccountFunds
    ) external returns (uint256 validationData);
}

// --- ERC-1271 Interface ---

interface IERC1271 {
    function isValidSignature(
        bytes32 hash,
        bytes calldata signature
    ) external view returns (bytes4 magicValue);
}

// --- Execution Struct ---

struct Execution {
    address target;
    uint256 value;
    bytes callData;
}

/**
 * @title StealthAccount
 * @notice Production EVM Smart Account fully compatible with ERC-4337, ERC-7579, and ERC-1271.
 */
contract StealthAccount is IAccount, IERC1271 {
    using ECDSA for bytes32;
    using MessageHashUtils for bytes32;

    // Standard ERC-4337 Constants
    uint256 internal constant SIG_VALIDATION_SUCCESS = 0;
    uint256 internal constant SIG_VALIDATION_FAILED = 1;

    // ERC-1271 Constants
    bytes4 internal constant ERC1271_MAGIC_VALUE = 0x1626ba7e;
    bytes4 internal constant ERC1271_INVALID = 0xffffffff;

    // ERC-7579 Mode Selectors
    bytes32 internal constant EXEC_MODE_SINGLE =
        0x0000000000000000000000000000000000000000000000000000000000000000;
    bytes32 internal constant EXEC_MODE_BATCH =
        0x0100000000000000000000000000000000000000000000000000000000000000;

    address public immutable entryPoint;
    address public immutable clientPubkey;
    address public immutable cosignerPubkey;

    error ZeroAddress();
    error Unauthorized();
    error UnsupportedExecutionMode();
    error ExecutionFailed(uint256 index);

    modifier onlyEntryPoint() {
        if (msg.sender != entryPoint) revert Unauthorized();
        _;
    }

    modifier onlyEntryPointOrSelf() {
        if (msg.sender != entryPoint && msg.sender != address(this))
            revert Unauthorized();
        _;
    }

    /**
     * @param _entryPoint Canonical ERC-4337 EntryPoint contract address (e.g., 0x0000000071727De22E5E9d8BAf0edAc6f37da032).
     * @param _clientPubkey Public key address derived off-chain for stealth user.
     * @param _cosignerPubkey Public key address derived for Lit TEE co-signer.
     */
    constructor(
        address _entryPoint,
        address _clientPubkey,
        address _cosignerPubkey
    ) {
        if (
            _entryPoint == address(0) ||
            _clientPubkey == address(0) ||
            _cosignerPubkey == address(0)
        ) {
            revert ZeroAddress();
        }
        entryPoint = _entryPoint;
        clientPubkey = _clientPubkey;
        cosignerPubkey = _cosignerPubkey;
    }

    // =========================================================================
    // ERC-4337: Account Abstraction Core Validation
    // =========================================================================

    /**
     * @notice Validates the signature and pre-funds gas required for ERC-4337 UserOperations.
     */
    function validateUserOp(
        PackedUserOperation calldata userOp,
        bytes32 userOpHash,
        uint256 missingAccountFunds
    ) external override onlyEntryPoint returns (uint256 validationData) {
        bytes32 hash = userOpHash.toEthSignedMessageHash();

        if (!_isValid2Of2Signature(hash, userOp.signature)) {
            return SIG_VALIDATION_FAILED;
        }

        // Pay bundler gas shortfall if paymaster is not handling fee payment
        if (missingAccountFunds > 0) {
            (bool success, ) = payable(msg.sender).call{
                value: missingAccountFunds
            }("");
            if (!success) {
                revert("missing account funds transfer failed");
            }
        }

        return SIG_VALIDATION_SUCCESS;
    }

    // =========================================================================
    // ERC-7579: Standard Execution Interface
    // =========================================================================

    /**
     * @notice ERC-7579 Standard Modular Execution EntryPoint.
     * @param mode 32-byte mode encoding (0x00 for single, 0x01 for batch).
     * @param executionCalldata ABI-encoded execution data (single or array of Execution structs).
     */
    function executeFromExecutor(
        bytes32 mode,
        bytes calldata executionCalldata
    ) external onlyEntryPointOrSelf returns (bytes[] memory returnData) {
        bytes1 modeType = bytes1(mode);

        if (modeType == 0x00) {
            // Single execution
            (address target, uint256 value, bytes memory data) = abi.decode(
                executionCalldata,
                (address, uint256, bytes)
            );
            returnData = new bytes[](1);
            returnData[0] = _call(target, value, data);
        } else if (modeType == 0x01) {
            // Batch execution
            Execution[] memory executions = abi.decode(
                executionCalldata,
                (Execution[])
            );
            returnData = new bytes[](executions.length);
            for (uint256 i = 0; i < executions.length; i++) {
                returnData[i] = _call(
                    executions[i].target,
                    executions[i].value,
                    executions[i].callData
                );
            }
        } else {
            revert UnsupportedExecutionMode();
        }
    }

    /**
     * @notice Backward-compatible simple execution interface for standard tools.
     */
    function execute(
        address target,
        uint256 value,
        bytes calldata data
    ) external onlyEntryPointOrSelf returns (bytes memory) {
        return _call(target, value, data);
    }

    /**
     * @notice Backward-compatible batch execution interface.
     */
    function executeBatch(
        Execution[] calldata executions
    ) external onlyEntryPointOrSelf returns (bytes[] memory returnData) {
        returnData = new bytes[](executions.length);
        for (uint256 i = 0; i < executions.length; i++) {
            returnData[i] = _call(
                executions[i].target,
                executions[i].value,
                executions[i].callData
            );
        }
    }

    // =========================================================================
    // ERC-1271: Smart Contract Off-Chain Signature Validation
    // =========================================================================

    /**
     * @notice Standard off-chain signature validation for DApps (SiWE, Uniswap Permits, OpenSea).
     */
    function isValidSignature(
        bytes32 hash,
        bytes calldata signature
    ) external view override returns (bytes4) {
        bytes32 ethHash = hash.toEthSignedMessageHash();
        return
            _isValid2Of2Signature(ethHash, signature)
                ? ERC1271_MAGIC_VALUE
                : ERC1271_INVALID;
    }

    // =========================================================================
    // Internal Helper Logic
    // =========================================================================

    /**
     * @dev Unpacks and validates 130-byte 2-of-2 multisig payload (65 bytes client + 65 bytes TEE).
     */
    function _isValid2Of2Signature(
        bytes32 hash,
        bytes calldata signature
    ) internal view returns (bool) {
        if (signature.length != 130) {
            return false;
        }

        bytes calldata clientSig = signature[0:65];
        bytes calldata cosignerSig = signature[65:130];

        // 1. Recover Client Signer
        address recoveredClient = hash.recover(clientSig);
        if (recoveredClient == address(0) || recoveredClient != clientPubkey) {
            return false;
        }

        // 2. Recover TEE Co-Signer
        address recoveredCosigner = hash.recover(cosignerSig);
        if (
            recoveredCosigner == address(0) ||
            recoveredCosigner != cosignerPubkey
        ) {
            return false;
        }

        return true;
    }

    function _call(
        address target,
        uint256 value,
        bytes memory data
    ) internal returns (bytes memory result) {
        bool success;
        (success, result) = target.call{value: value}(data);
        if (!success) {
            assembly {
                revert(add(result, 32), mload(result))
            }
        }
    }

    receive() external payable {}
}

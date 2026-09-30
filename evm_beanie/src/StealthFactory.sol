// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {StealthAccount} from "./StealthAccount.sol";

/**
 * @title StealthAccountFactory
 * @notice CREATE2 factory used through ERC-4337 `initCode`:
 *
 *   initCode = factoryAddress (20 bytes)
 *           ++ abi.encodeCall(StealthAccountFactory.createAccount, (client, salt))
 *
 * `entryPoint` and `cosigner` are pinned in the factory, so a client can only
 * choose its own key and salt. Every account this factory makes is therefore
 * bound to TEE/MPC cosigner for this chain. Deploy one factory per
 * chain, with `cosigner` = the address the worker logs at startup
 * ("chain X ready, cosigner 0x...").
 */
contract StealthAccountFactory {
    address public immutable entryPoint;
    address public immutable cosigner;

    error ZeroAddress();

    constructor(address _entryPoint, address _cosigner) {
        if (_entryPoint == address(0) || _cosigner == address(0))
            revert ZeroAddress();
        entryPoint = _entryPoint;
        cosigner = _cosigner;
    }

    /// @dev Idempotent: returns the existing account if already deployed.
    function createAccount(
        address client,
        bytes32 salt
    ) external returns (address account) {
        account = getAddress(client, salt);
        if (account.code.length > 0) return account;
        account = address(
            new StealthAccount{salt: _salt(client, salt)}(
                entryPoint,
                client,
                cosigner
            )
        );
    }

    /// @notice Counterfactual address. The client SDK uses this as `sender`.
    function getAddress(
        address client,
        bytes32 salt
    ) public view returns (address) {
        bytes32 initHash = keccak256(
            abi.encodePacked(
                type(StealthAccount).creationCode,
                abi.encode(entryPoint, client, cosigner)
            )
        );
        return
            address(
                uint160(
                    uint256(
                        keccak256(
                            abi.encodePacked(
                                bytes1(0xff),
                                address(this),
                                _salt(client, salt),
                                initHash
                            )
                        )
                    )
                )
            );
    }

    function _salt(
        address client,
        bytes32 salt
    ) internal pure returns (bytes32) {
        return keccak256(abi.encode(client, salt));
    }
}

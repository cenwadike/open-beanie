// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {StealthAccount} from "./StealthAccount.sol";

/**
 * @title StealthAccountFactory
 * @notice Immutable CREATE2 factory. No owner, no setters.
 *
 * The co-signer is a per-account parameter, so any provider (or the client
 * itself) can run its own co-signer. It is part of the constructor args and
 * therefore part of the CREATE2 init-code hash: the account address commits to
 * (entryPoint, client, cosigner, salt), and nobody can deploy a different
 * co-signer at the same address.
 *
 * Usable through ERC-4337 `initCode`:
 *   initCode = factoryAddress (20 bytes)
 *           ++ abi.encodeCall(createAccount, (client, cosigner, salt))
 *
 * Deploy one factory per chain.
 */
contract StealthAccountFactory {
    address public immutable entryPoint;

    error ZeroAddress();

    constructor(address _entryPoint) {
        if (_entryPoint == address(0)) revert ZeroAddress();
        entryPoint = _entryPoint;
    }

    /// @dev Idempotent: returns the existing account if already deployed.
    function createAccount(
        address client,
        address cosigner,
        bytes32 salt
    ) external returns (address account) {
        address predicted = getAddress(client, cosigner, salt);
        if (predicted.code.length > 0) return predicted;
        account = address(
            new StealthAccount{salt: _salt(client, cosigner, salt)}(
                entryPoint,
                client,
                cosigner
            )
        );
        assert(account == predicted);
    }

    /// @notice Counterfactual address. Must use the SAME constructor args as
    ///         the deployment above, including the cosigner.
    function getAddress(
        address client,
        address cosigner,
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
                                _salt(client, cosigner, salt),
                                initHash
                            )
                        )
                    )
                )
            );
    }

    function _salt(
        address client,
        address cosigner,
        bytes32 salt
    ) internal pure returns (bytes32) {
        return keccak256(abi.encode(client, cosigner, salt));
    }
}

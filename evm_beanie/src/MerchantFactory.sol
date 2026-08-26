// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Clones} from "@openzeppelin/contracts/proxy/Clones.sol";

interface IChainXReceiver {
    function initialize(
        address _token,
        address _walletA,
        address _walletB,
        address _tokenMessenger,
        uint32 _cctpDestinationDomain, // matches ChainXReceiver exactly
        bytes32 _cctpMintRecipient,
        address _merchant
    ) external;
}

contract MerchantFactory {
    using Clones for address;

    address public immutable receiverImplementation;
    address public token;
    address public walletA;
    address public walletB;
    address public tokenMessenger;

    mapping(address => uint256) public merchantNonces;
    mapping(address => address) public merchantReceiver;
    mapping(bytes32 => bool) validDomains;
    mapping(bytes32 => uint32) destinationDomain; // keyed by chain name

    event MerchantRegistered(address indexed merchant, address receiver);

    error AlreadyRegistered();

    constructor(
        address _receiverImplementation,
        address _token,
        address _walletA,
        address _walletB,
        address _tokenMessenger,
        uint32 _starknetDestinationDomain,
        uint32 _baseDestinationDomain,
        uint32 _solanaDestinationDomain,
        uint32 _ethDestinationDomain
    ) {
        require(_receiverImplementation != address(0), "zero impl");
        require(
            _baseDestinationDomain != _solanaDestinationDomain &&
                _baseDestinationDomain != _starknetDestinationDomain &&
                _baseDestinationDomain != _ethDestinationDomain &&
                _solanaDestinationDomain != _starknetDestinationDomain &&
                _solanaDestinationDomain != _ethDestinationDomain &&
                _starknetDestinationDomain != _ethDestinationDomain,
            "provide unique cctp domains"
        );
        receiverImplementation = _receiverImplementation;
        token = _token;
        walletA = _walletA;
        walletB = _walletB;
        tokenMessenger = _tokenMessenger;

        validDomains["STARKNET"] = true;
        validDomains["BASE"] = true;
        validDomains["SOLANA"] = true;
        validDomains["ETHEREUM"] = true;

        destinationDomain["STARKNET"] = _starknetDestinationDomain;
        destinationDomain["BASE"] = _baseDestinationDomain;
        destinationDomain["SOLANA"] = _solanaDestinationDomain;
        destinationDomain["ETHEREUM"] = _ethDestinationDomain;
    }

    function registerMerchant(
        address merchant,
        bytes32 cctpMintChain, // "STARKNET" || "BASE" || "SOLANA" || "ETHEREUM"
        bytes32 cctpMintRecipient
    ) external returns (address) {
        if (merchantReceiver[merchant] != address(0))
            revert AlreadyRegistered();
        require(validDomains[cctpMintChain] == true, "invalid");

        uint256 nonce = merchantNonces[merchant];
        bytes32 salt = keccak256(abi.encodePacked(merchant, nonce));

        address clone = Clones.cloneDeterministic(receiverImplementation, salt);

        // look up the numeric CCTP domain for the requested chain
        uint32 domain = destinationDomain[cctpMintChain];

        IChainXReceiver(clone).initialize(
            token,
            walletA,
            walletB,
            tokenMessenger,
            domain,
            cctpMintRecipient,
            merchant
        );

        merchantReceiver[merchant] = clone;
        merchantNonces[merchant] = nonce + 1;

        emit MerchantRegistered(merchant, clone);
        return clone;
    }

    function predictReceiverAddress(
        address merchant
    ) external view returns (address) {
        uint256 nonce = merchantNonces[merchant];
        bytes32 salt = keccak256(abi.encodePacked(merchant, nonce));
        return
            Clones.predictDeterministicAddress(
                receiverImplementation,
                salt,
                address(this)
            );
    }

    function getReceiver(address merchant) external view returns (address) {
        return merchantReceiver[merchant];
    }
}

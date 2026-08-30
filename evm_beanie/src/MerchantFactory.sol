// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

import {Clones} from "@openzeppelin/contracts/proxy/Clones.sol";

interface IChainXReceiver {
    function initialize(
        address _token,
        address _treasury,
        address _tokenMessenger,
        uint32 _cctpDestinationDomain, // matches ChainXReceiver exactly
        bytes32 _cctpMintRecipient,
        address _merchant
    ) external;
}

contract MerchantFactory {
    using Clones for address;

    uint256 public constant MAX_RECEIVERS_PER_MERCHANT = 32;

    address public immutable receiverImplementation;
    address public token;
    address public treasury;
    address public tokenMessenger;

    mapping(address => uint256) public merchantNonces;
    mapping(address => address[]) private merchantReceiversMap;
    mapping(bytes32 => bool) public validDomains;
    mapping(bytes32 => uint32) public destinationDomain; // keyed by chain name

    event MerchantRegistered(address indexed merchant, address receiver);

    error MaximumReceiversExceeded();
    error InvalidDomain();
    error IndexOutOfBounds();

    constructor(
        address _receiverImplementation,
        address _token,
        address _treasury,
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
        treasury = _treasury;
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
        bytes32 cctpMintChain, // "" || "STARKNET" || "BASE" || "SOLANA" || "ETHEREUM"
        bytes32 cctpMintRecipient // "" || byte32(merchant)
    ) external returns (address) {
        if (
            merchantReceiversMap[merchant].length >= MAX_RECEIVERS_PER_MERCHANT
        ) {
            revert MaximumReceiversExceeded();
        }
        if (cctpMintChain != bytes32(0)) {
            require(validDomains[cctpMintChain], InvalidDomain());
            require(
                cctpMintRecipient != bytes32(0),
                "Cross-chain requires destination recipient"
            );
        } else {
            require(
                cctpMintRecipient == bytes32(0),
                "Same-chain recipient must be zero"
            );
        }
        uint256 nonce = merchantNonces[merchant];
        bytes32 salt = keccak256(abi.encodePacked(merchant, nonce));

        address clone = Clones.cloneDeterministic(receiverImplementation, salt);

        uint32 domain = destinationDomain[cctpMintChain];

        IChainXReceiver(clone).initialize(
            token,
            treasury,
            tokenMessenger,
            domain,
            cctpMintRecipient,
            merchant
        );

        merchantReceiversMap[merchant].push(clone);
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

    function getReceiverCount(
        address merchant
    ) external view returns (uint256) {
        return merchantReceiversMap[merchant].length;
    }

    function getMerchantReceivers(
        address merchant
    ) external view returns (address[] memory) {
        return merchantReceiversMap[merchant];
    }

    function getMerchantReceiverAt(
        address merchant,
        uint256 index
    ) external view returns (address) {
        if (index >= merchantReceiversMap[merchant].length) {
            revert IndexOutOfBounds();
        }
        return merchantReceiversMap[merchant][index];
    }
}

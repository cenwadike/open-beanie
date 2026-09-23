// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.24;

import {Clones} from "@openzeppelin/contracts/proxy/Clones.sol";

interface IChainXReceiver {
    function initialize(
        address _token,
        address _treasury,
        address _tokenMessenger,
        uint32 _cctpDestinationDomain,
        bytes32 _cctpMintRecipient,
        address _merchant
    ) external;
}

contract ReceiverFactory {
    using Clones for address;

    uint256 public constant MAX_RECEIVERS_PER_MERCHANT = 32;

    address public immutable receiverImplementation;
    address public token;
    address public treasury;
    address public tokenMessenger;

    mapping(address => address[]) private merchantReceiversMap;
    mapping(bytes32 => bool) public validDomains;
    mapping(bytes32 => uint32) public destinationDomain; // keyed by chain name

    event MerchantRegistered(address indexed merchant, address receiver);
    event ReceiverAnnounced(
        address indexed merchant,
        address indexed receiver,
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    );

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

    /// The receiver's address is a pure function of (merchant, cctpMintChain,
    /// cctpMintRecipient)
    /// Registering the same route twice reverts (CREATE2 onto a live address).
    /// MAX_RECEIVERS_PER_MERCHANT therefore caps distinct routes per merchant.
    function registerMerchant(
        address merchant,
        bytes32 cctpMintChain, // 0 (same-chain) || "STARKNET" || "BASE" || "SOLANA" || "ETHEREUM"
        bytes32 cctpMintRecipient // 0 (same-chain) || recipient on the destination chain
    ) external returns (address) {
        if (
            merchantReceiversMap[merchant].length >= MAX_RECEIVERS_PER_MERCHANT
        ) {
            revert MaximumReceiversExceeded();
        }
        _checkRoute(cctpMintChain, cctpMintRecipient);

        address clone = Clones.cloneDeterministic(
            receiverImplementation,
            _salt(merchant, cctpMintChain, cctpMintRecipient)
        );

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

        emit MerchantRegistered(merchant, clone);
        return clone;
    }

    /// @notice Cheap on-chain announcement of the receiver address for this
    /// merchant and route. Does not deploy. The event carries the route so the
    /// keeper can register it later from the log alone.
    function announceReceiver(
        address merchant,
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    ) external {
        _checkRoute(cctpMintChain, cctpMintRecipient);
        address receiver = _predict(merchant, cctpMintChain, cctpMintRecipient);
        emit ReceiverAnnounced(
            merchant,
            receiver,
            cctpMintChain,
            cctpMintRecipient
        );
    }

    function predictReceiverAddress(
        address merchant,
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    ) external view returns (address) {
        return _predict(merchant, cctpMintChain, cctpMintRecipient);
    }

    function _checkRoute(
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    ) private view {
        if (cctpMintChain != bytes32(0)) {
            if (validDomains[cctpMintChain] == false) revert InvalidDomain();
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
    }

    function _salt(
        address merchant,
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    ) private pure returns (bytes32) {
        return
            keccak256(
                abi.encodePacked(merchant, cctpMintChain, cctpMintRecipient)
            );
    }

    function _predict(
        address merchant,
        bytes32 cctpMintChain,
        bytes32 cctpMintRecipient
    ) private view returns (address) {
        return
            Clones.predictDeterministicAddress(
                receiverImplementation,
                _salt(merchant, cctpMintChain, cctpMintRecipient),
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

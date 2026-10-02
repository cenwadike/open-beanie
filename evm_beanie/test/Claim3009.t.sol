// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import {StealthAccount} from "../src/StealthAccount.sol";
import {StealthAccountFactory} from "../src/StealthFactory.sol";

interface IUSDC {
    function balanceOf(address) external view returns (uint256);
    function DOMAIN_SEPARATOR() external view returns (bytes32);
    function authorizationState(
        address authorizer,
        bytes32 nonce
    ) external view returns (bool);
}

interface IMulticall3 {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }
    struct Result {
        bool success;
        bytes returnData;
    }

    function aggregate3(
        Call3[] calldata calls
    ) external payable returns (Result[] memory);
}

/// Run: BASE_RPC_URL=<base mainnet rpc> forge test --match-contract Claim3009Test -vv
///
/// Pins the exact bytes the worker and client SDK must reproduce:
///   D      = EIP-712 digest of USDC TransferWithAuthorization
///   signed = EIP-191(D)            (StealthAccount.isValidSignature wraps D itself)
///   sig    = client65 || cosigner65
///   tx     = Multicall3.aggregate3([createAccount, transferWithAuthorization(bytes overload)])
contract Claim3009Test is Test {
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913; // Base
    address constant MULTICALL3 = 0xcA11bde05977b3631167028862bE2a173976CA11;
    address constant ENTRY_POINT = 0x0000000071727De22E5E9d8BAf0edAc6f37da032;

    bytes32 constant TRANSFER_TYPEHASH =
        keccak256(
            "TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)"
        );

    uint256 constant CLIENT_PK = 0xC11E47;
    uint256 constant COSIGNER_PK = 0xC0514E2;
    uint256 constant OTHER_PK = 0xBADBAD;
    bytes32 constant SALT = keccak256("salt-1");
    uint256 constant FUNDED = 1_000e6;

    struct Auth {
        address from;
        address to;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 nonce;
    }

    StealthAccountFactory factory;
    address client;
    address cosigner;
    address acct;
    address recipient;
    address relayer;

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"));

        client = vm.addr(CLIENT_PK);
        cosigner = vm.addr(COSIGNER_PK);
        recipient = makeAddr("recipient");
        relayer = makeAddr("relayer");

        factory = new StealthAccountFactory(ENTRY_POINT);
        acct = factory.getAddress(client, cosigner, SALT);

        // Account holds only USDC, never ETH: that is the whole point of this path.
        deal(USDC, acct, FUNDED);
        assertEq(acct.balance, 0);
        assertEq(acct.code.length, 0, "counterfactual: no code yet");
    }

    // ---------------------------------------------------------------- helpers

    function _auth(
        uint256 value,
        bytes32 nonce
    ) internal view returns (Auth memory) {
        return
            Auth({
                from: acct,
                to: recipient,
                value: value,
                validAfter: 0,
                validBefore: block.timestamp + 1 hours,
                nonce: nonce
            });
    }

    /// EIP-712 digest D, read domain separator from the token itself.
    function _digest(Auth memory a) internal view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                TRANSFER_TYPEHASH,
                a.from,
                a.to,
                a.value,
                a.validAfter,
                a.validBefore,
                a.nonce
            )
        );
        return
            keccak256(
                abi.encodePacked(
                    "\x19\x01",
                    IUSDC(USDC).DOMAIN_SEPARATOR(),
                    structHash
                )
            );
    }

    /// Both stealth signers sign EIP-191(D), not D.
    function _sign191(
        uint256 pk,
        bytes32 d
    ) internal pure returns (bytes memory) {
        bytes32 h = MessageHashUtils.toEthSignedMessageHash(d);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, h);
        return abi.encodePacked(r, s, v);
    }

    function _sig130(
        Auth memory a,
        uint256 clientPk,
        uint256 cosignerPk
    ) internal view returns (bytes memory) {
        bytes32 d = _digest(a);
        return bytes.concat(_sign191(clientPk, d), _sign191(cosignerPk, d));
    }

    function _transferCall(
        Auth memory a,
        bytes memory sig
    ) internal pure returns (bytes memory) {
        return
            abi.encodeWithSignature(
                "transferWithAuthorization(address,address,uint256,uint256,uint256,bytes32,bytes)",
                a.from,
                a.to,
                a.value,
                a.validAfter,
                a.validBefore,
                a.nonce,
                sig
            );
    }

    function _batch(
        Auth memory a,
        bytes memory sig,
        bool withDeploy
    ) internal view returns (IMulticall3.Call3[] memory calls) {
        calls = new IMulticall3.Call3[](withDeploy ? 2 : 1);
        uint256 i;
        if (withDeploy) {
            calls[i++] = IMulticall3.Call3({
                target: address(factory),
                allowFailure: false,
                callData: abi.encodeCall(
                    StealthAccountFactory.createAccount,
                    (client, cosigner, SALT)
                )
            });
        }
        calls[i] = IMulticall3.Call3({
            target: USDC,
            allowFailure: false,
            callData: _transferCall(a, sig)
        });
    }

    function _relay(IMulticall3.Call3[] memory calls) internal {
        vm.prank(relayer);
        IMulticall3(MULTICALL3).aggregate3(calls);
    }

    // ------------------------------------------------------------------ tests

    /// Mirrors the worker's startup check: our locally built domain must equal the token's.
    function test_domainSeparatorMatchesConfig() public view {
        bytes32 expected = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256(bytes("USD Coin")),
                keccak256(bytes("2")),
                block.chainid,
                USDC
            )
        );
        assertEq(expected, IUSDC(USDC).DOMAIN_SEPARATOR());
    }

    function test_addr_counterfactualEqualsDeployed() public {
        address deployed = factory.createAccount(client, cosigner, SALT);
        assertEq(deployed, acct);
        assertGt(acct.code.length, 0);
    }

    function test_idempotent_createAccountTwice() public {
        address a1 = factory.createAccount(client, cosigner, SALT);
        address a2 = factory.createAccount(client, cosigner, SALT);
        assertEq(a1, a2);
    }

    function test_claim_happy() public {
        bytes32 nonce = keccak256("n1");
        Auth memory a = _auth(250e6, nonce);
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            true
        );

        _relay(calls);

        assertEq(IUSDC(USDC).balanceOf(recipient), 250e6);
        assertEq(IUSDC(USDC).balanceOf(acct), FUNDED - 250e6);
        assertGt(acct.code.length, 0, "deployed in the same tx");
        assertTrue(IUSDC(USDC).authorizationState(acct, nonce));
        assertEq(relayer.balance, 0); // relayer only needed gas, nothing else
    }

    /// Second claim: account already deployed, createAccount is a no-op, new nonce.
    function test_claim_secondClaimAfterDeploy() public {
        Auth memory a1 = _auth(100e6, keccak256("n1"));
        _relay(_batch(a1, _sig130(a1, CLIENT_PK, COSIGNER_PK), true));

        Auth memory a2 = _auth(50e6, keccak256("n2"));
        _relay(_batch(a2, _sig130(a2, CLIENT_PK, COSIGNER_PK), false));

        assertEq(IUSDC(USDC).balanceOf(recipient), 150e6);
    }

    function test_replay_sameNonceReverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            true
        );
        _relay(calls);

        vm.expectRevert();
        _relay(calls);
    }

    function test_wrongCosigner_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, OTHER_PK),
            true
        );

        vm.expectRevert();
        _relay(calls);
    }

    function test_wrongClient_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, OTHER_PK, COSIGNER_PK),
            true
        );

        vm.expectRevert();
        _relay(calls);
    }

    /// 1-of-2 shapes must all fail: client only (65B), cosigner only (65B), client twice (130B).
    function test_oneOfTwo_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        bytes32 d = _digest(a);
        bytes memory c = _sign191(CLIENT_PK, d);
        bytes memory k = _sign191(COSIGNER_PK, d);

        IMulticall3.Call3[] memory onlyClient = _batch(a, c, true);
        vm.expectRevert();
        _relay(onlyClient);

        IMulticall3.Call3[] memory onlyCosigner = _batch(a, k, true);
        vm.expectRevert();
        _relay(onlyCosigner);

        IMulticall3.Call3[] memory clientTwice = _batch(
            a,
            bytes.concat(c, c),
            true
        );
        vm.expectRevert();
        _relay(clientTwice);

        // swapped order is also invalid
        IMulticall3.Call3[] memory swapped = _batch(
            a,
            bytes.concat(k, c),
            true
        );
        vm.expectRevert();
        _relay(swapped);
    }

    /// Signing raw D instead of EIP-191(D) (the EOA habit) must fail for the stealth account.
    function test_rawDigestSignature_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        bytes32 d = _digest(a);
        (uint8 v1, bytes32 r1, bytes32 s1) = vm.sign(CLIENT_PK, d);
        (uint8 v2, bytes32 r2, bytes32 s2) = vm.sign(COSIGNER_PK, d);
        bytes memory sig = bytes.concat(
            abi.encodePacked(r1, s1, v1),
            abi.encodePacked(r2, s2, v2)
        );
        IMulticall3.Call3[] memory calls = _batch(a, sig, true);

        vm.expectRevert();
        _relay(calls);
    }

    /// A relayer cannot redirect funds: `to` is inside what was signed.
    function test_relayerCannotChangeTo() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        bytes memory sig = _sig130(a, CLIENT_PK, COSIGNER_PK);
        a.to = makeAddr("attacker");
        IMulticall3.Call3[] memory calls = _batch(a, sig, true);

        vm.expectRevert();
        _relay(calls);
    }

    function test_relayerCannotChangeValue() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        bytes memory sig = _sig130(a, CLIENT_PK, COSIGNER_PK);
        a.value = 900e6;
        IMulticall3.Call3[] memory calls = _batch(a, sig, true);

        vm.expectRevert();
        _relay(calls);
    }

    function test_expired_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            true
        );

        vm.warp(a.validBefore + 1);
        vm.expectRevert();
        _relay(calls);
    }

    function test_notYetValid_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        a.validAfter = block.timestamp + 10 minutes;
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            true
        );

        vm.expectRevert();
        _relay(calls);
    }

    /// ERC-1271 needs code: transfer without createAccount first must fail
    /// (USDC falls back to ecrecover, which rejects a 130-byte signature).
    function test_noDeploy_reverts() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            false
        );

        vm.expectRevert();
        _relay(calls);
    }

    /// aggregate3 is atomic: a failing transfer must not leave a deployed account behind.
    function test_failedClaim_leavesNoAccount() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, OTHER_PK),
            true
        );

        vm.prank(relayer);
        (bool ok, ) = MULTICALL3.call(
            abi.encodeCall(IMulticall3.aggregate3, (calls))
        );

        assertFalse(ok);
        assertEq(acct.code.length, 0);
        assertEq(IUSDC(USDC).balanceOf(acct), FUNDED);
    }

    /// Anyone can relay: a different relayer gets the same result (no msg.sender dependence).
    function test_anyoneCanRelay() public {
        Auth memory a = _auth(100e6, keccak256("n1"));
        IMulticall3.Call3[] memory calls = _batch(
            a,
            _sig130(a, CLIENT_PK, COSIGNER_PK),
            true
        );

        vm.prank(makeAddr("stranger"));
        IMulticall3(MULTICALL3).aggregate3(calls);

        assertEq(IUSDC(USDC).balanceOf(recipient), 100e6);
    }

    /// A different account (other client, same cosigner) cannot be spent with this client's signature.
    function test_otherClientAccount_isDifferentAddress() public view {
        address other = factory.getAddress(
            vm.addr(OTHER_PK),
            vm.addr(COSIGNER_PK),
            SALT
        );
        assertTrue(other != acct);
    }
}

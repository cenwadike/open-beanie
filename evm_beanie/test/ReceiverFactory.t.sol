// SPDX-License-Identifier: AGPL-3
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/ChainXReceiver.sol";
import "../src/ReceiverFactory.sol";
import "../src/ChainXReceiver.sol" as R;

interface IERC20Minimal {
    function balanceOf(address) external view returns (uint256);

    function allowance(address, address) external view returns (uint256);
}

contract MockToken is IERC20 {
    string public name = "Mock";
    string public symbol = "MCK";
    uint8 public decimals = 18;

    mapping(address => uint256) public balances;
    mapping(address => mapping(address => uint256)) public allowances;

    function totalSupply() external pure override returns (uint256) {
        return 0;
    }

    function balanceOf(
        address account
    ) external view override returns (uint256) {
        return balances[account];
    }

    function transfer(
        address to,
        uint256 amount
    ) external override returns (bool) {
        address from = msg.sender;
        require(balances[from] >= amount, "insufficient");
        balances[from] -= amount;
        balances[to] += amount;
        return true;
    }

    function allowance(
        address owner,
        address spender
    ) external view override returns (uint256) {
        return allowances[owner][spender];
    }

    function approve(
        address spender,
        uint256 amount
    ) external override returns (bool) {
        allowances[msg.sender][spender] = amount;
        return true;
    }

    function transferFrom(
        address from,
        address to,
        uint256 amount
    ) external override returns (bool) {
        uint256 allowed = allowances[from][msg.sender];
        require(allowed >= amount, "allowance");
        require(balances[from] >= amount, "balance");
        allowances[from][msg.sender] = allowed - amount;
        balances[from] -= amount;
        balances[to] += amount;
        return true;
    }

    // helper mint for tests
    function mint(address to, uint256 amount) external {
        balances[to] += amount;
    }
}

contract MockMessenger {
    uint256 public lastAmount;
    uint32 public lastDestinationDomain;
    bytes32 public lastMintRecipient;
    address public lastBurnToken;
    uint256 public lastMaxFee;
    uint256 public lastApprovedAllowance;

    function depositForBurn(
        uint256 amount,
        uint32 destinationDomain,
        bytes32 mintRecipient,
        address burnToken,
        bytes32,
        uint256 maxFee,
        uint32
    ) external {
        lastAmount = amount;
        lastDestinationDomain = destinationDomain;
        lastMintRecipient = mintRecipient;
        lastBurnToken = burnToken;
        lastMaxFee = maxFee;

        // mimic the real TokenMessengerV2: pull the burn amount from the
        // caller under the allowance it just approved
        lastApprovedAllowance = IERC20(burnToken).allowance(
            msg.sender,
            address(this)
        );

        MockToken(burnToken).transferFrom(msg.sender, address(this), amount);
    }
}

contract ReceiverFactoryTest is Test {
    MockToken token;
    MockMessenger messenger;
    ChainXReceiver implementation;
    ReceiverFactory factory;

    address treasury = address(0x100);
    uint32 _starknetDestinationDomain = 21;
    uint32 _solanaDestinationDomain = 5;
    uint32 _baseDestinationDomain = 3;
    uint32 _ethDestinationDomain = 0;

    // Helper function to dynamically generate a valid CCTP recipient
    function getMintRecipient(
        address merchant
    ) internal pure returns (bytes32) {
        return bytes32(uint256(uint160(merchant)));
    }

    function setUp() public {
        token = new MockToken();
        messenger = new MockMessenger();
        implementation = new ChainXReceiver();

        factory = new ReceiverFactory(
            address(implementation),
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            _baseDestinationDomain,
            _solanaDestinationDomain,
            _ethDestinationDomain
        );
    }

    function test_register_and_sweep_burn_path_happy() public {
        address merchant = address(0xABC);
        bytes32 validRecipient = getMintRecipient(merchant);

        // deploy clone via factory
        address clone = factory.registerMerchant(
            merchant,
            "STARKNET",
            validRecipient
        );

        // ensure factory stored it
        assertEq(factory.getReceiverCount(merchant), 1);
        assertEq(factory.getMerchantReceiverAt(merchant, 0), clone);

        ReceiverFactory f2 = new ReceiverFactory(
            address(implementation),
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            _baseDestinationDomain,
            _solanaDestinationDomain,
            _ethDestinationDomain
        );

        address clone2 = f2.registerMerchant(
            merchant,
            "STARKNET",
            validRecipient
        );

        // mint tokens into clone2
        token.mint(clone2, 10000);

        // Set relayer as both msg.sender AND tx.origin
        address relayer = address(0x5555);
        vm.prank(relayer, relayer);

        (
            uint256 net,
            uint256 toCaller,
            uint256 toTreasury,
            uint256 fee
        ) = ChainXReceiver(clone2).sweep();

        // fee = 10000 * 50 / 10000 = 50; net = 9950
        // feeToCaller = 50 * 1000 / 10000 = 5; feeToTreasury = 45
        assertEq(fee, 50);
        assertEq(net, 9950);
        assertEq(toCaller, 5);
        assertEq(toTreasury, 45);

        // relayer (tx.origin) and treasury both received their share
        assertEq(token.balanceOf(relayer), toCaller);
        assertEq(token.balanceOf(treasury), toTreasury);

        // messenger recorded the burn
        assertEq(messenger.lastAmount(), net);
        assertEq(messenger.lastBurnToken(), address(token));

        // clone approved messenger for net
        uint256 lastApprovedAllowance = messenger.lastApprovedAllowance();
        assertEq(lastApprovedAllowance, net);

        // idempotent: calling sweep again with zero balance returns zeros
        (uint256 net2, uint256 a2, uint256 b2, uint256 fee2) = ChainXReceiver(
            clone2
        ).sweep();
        assertEq(net2, 0);
        assertEq(a2, 0);
        assertEq(b2, 0);
        assertEq(fee2, 0);
    }

    function test_register_and_sweep_transfer_path_happy() public {
        address merchant = address(0x123);

        ReceiverFactory f3 = new ReceiverFactory(
            address(implementation),
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            _baseDestinationDomain,
            _solanaDestinationDomain,
            _ethDestinationDomain
        );

        // zero chain and zero recipient => same-chain settlement path
        address clone = f3.registerMerchant(merchant, bytes32(0), bytes32(0));

        token.mint(clone, 1_000);

        (
            uint256 net,
            uint256 toCaller,
            uint256 toTreasury,
            uint256 fee
        ) = ChainXReceiver(clone).sweep();

        // fee = 1000 * 50 / 10000 = 5; net = 995
        // feeToCaller = 5 * 1000 / 10000 = 0 (rounds down at this size); feeToTreasury = 5
        assertEq(fee, 5);
        assertEq(net, 995);
        assertEq(toCaller, 0);
        assertEq(toTreasury, 5);

        assertEq(token.balanceOf(address(this)), toCaller);
        assertEq(token.balanceOf(treasury), toTreasury);

        // merchant should receive net directly, no CCTP call
        assertEq(token.balanceOf(merchant), net);
        assertEq(messenger.lastAmount(), 0);
    }

    function test_multiple_registrations_and_views() public {
        address merchant = address(0x555);
        bytes32 validRecipient = getMintRecipient(merchant);

        address clone1 = factory.registerMerchant(
            merchant,
            "STARKNET",
            validRecipient
        );
        address clone2 = factory.registerMerchant(
            merchant,
            "SOLANA",
            validRecipient
        );

        assertEq(factory.getReceiverCount(merchant), 2);

        address[] memory receivers = factory.getMerchantReceivers(merchant);
        assertEq(receivers.length, 2);
        assertEq(receivers[0], clone1);
        assertEq(receivers[1], clone2);

        assertEq(factory.getMerchantReceiverAt(merchant, 0), clone1);
        assertEq(factory.getMerchantReceiverAt(merchant, 1), clone2);
    }

    function test_revert_exceed_max_receivers() public {
        address merchant = address(0x777);

        // Each route is a distinct (chain, recipient) pair, so vary the
        // recipient to get MAX_RECEIVERS_PER_MERCHANT distinct deterministic
        // addresses. Reusing one exact route would legitimately revert on
        // CREATE2 collision (registering the same route twice is intentionally
        // blocked), so that's not a valid way to reach the cap.
        for (uint256 i = 0; i < 32; i++) {
            bytes32 recipient = bytes32(uint256(i + 1));
            factory.registerMerchant(merchant, "STARKNET", recipient);
        }

        assertEq(factory.getReceiverCount(merchant), 32);

        vm.expectRevert(ReceiverFactory.MaximumReceiversExceeded.selector);
        factory.registerMerchant(merchant, "STARKNET", bytes32(uint256(999)));
    }

    function test_revert_invalid_domain() public {
        address merchant = address(0x888);
        bytes32 validRecipient = getMintRecipient(merchant);

        vm.expectRevert(ReceiverFactory.InvalidDomain.selector);
        factory.registerMerchant(merchant, "STARKNET2", validRecipient);
    }

    function test_revert_get_index_out_of_bounds() public {
        address merchant = address(0x999);
        bytes32 validRecipient = getMintRecipient(merchant);
        factory.registerMerchant(merchant, "BASE", validRecipient);

        vm.expectRevert(ReceiverFactory.IndexOutOfBounds.selector);
        factory.getMerchantReceiverAt(merchant, 1);
    }

    // test announced receiver is the same as the one that will be deployed next
    function test_announce_receiver_match_prediction_and_deployment() public {
        address merchant = address(0xAAA);
        bytes32 validRecipient = getMintRecipient(merchant);

        // 1. Predict + announce (nonce still 0)
        address predicted = factory.predictReceiverAddress(
            merchant,
            "STARKNET",
            validRecipient
        );

        vm.expectEmit(true, true, true, true);
        emit ReceiverFactory.ReceiverAnnounced(
            merchant,
            predicted,
            "STARKNET",
            validRecipient
        );
        factory.announceReceiver(merchant, "STARKNET", validRecipient);

        // 2. Register actually deploys the same address and advances the nonce
        address deployed = factory.registerMerchant(
            merchant,
            "STARKNET",
            validRecipient
        );

        // 3. Deployed must equal the address we announced
        assertEq(deployed, predicted);

        // Predicting the same route again is a pure function of
        // (merchant, cctpMintChain, cctpMintRecipient) and must return the
        // identical address — it does not advance just because a receiver
        // was deployed.
        address samePredicted = factory.predictReceiverAddress(
            merchant,
            "STARKNET",
            validRecipient
        );
        assertEq(samePredicted, deployed);

        // Attempting to register the exact same route again hits a live
        // CREATE2 address and must revert (by design: a route maps to
        // exactly one receiver).
        vm.expectRevert();
        factory.registerMerchant(merchant, "STARKNET", validRecipient);
    }

    function test_initialize_guard_and_storage() public {
        // Deploy clone manually from implementation to test initialize guards
        address merchant = address(0xABC);
        bytes32 validRecipient = getMintRecipient(merchant);

        ChainXReceiver r = new ChainXReceiver();
        r.initialize(
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            validRecipient,
            merchant
        );
        vm.expectRevert();
        r.initialize(
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            validRecipient,
            merchant
        );
    }
}

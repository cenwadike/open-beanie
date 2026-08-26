// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/ChainXReceiver.sol";
import "../src/MerchantFactory.sol";
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
    MerchantFactory factory;

    address treasury = address(0x100);
    uint32 _starknetDestinationDomain = 21;
    uint32 _solanaDestinationDomain = 5;
    uint32 _baseDestinationDomain = 3;
    uint32 _ethDestinationDomain = 0;
    bytes32 defaultMintRecipient = bytes32(uint256(uint160(address(0xBEEF))));

    function setUp() public {
        token = new MockToken();
        messenger = new MockMessenger();
        implementation = new ChainXReceiver();

        factory = new MerchantFactory(
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
        // deploy clone via factory
        address clone = factory.registerMerchant(
            merchant,
            "STARKNET",
            defaultMintRecipient
        );

        // ensure factory stored it
        assertEq(factory.getReceiver(merchant), clone);

        MerchantFactory f2 = new MerchantFactory(
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
            defaultMintRecipient
        );

        // mint tokens into clone2
        token.mint(clone2, 10000);

        // call sweep on clone2 — msg.sender here is this test contract,
        // which is the "caller" that gets the 10%-of-fee incentive
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

        // caller (this test contract) and treasury both received their share
        assertEq(token.balanceOf(address(this)), toCaller);
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

        MerchantFactory f3 = new MerchantFactory(
            address(implementation),
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            _baseDestinationDomain,
            _solanaDestinationDomain,
            _ethDestinationDomain
        );

        // zero recipient => same-chain settlement path
        address clone = f3.registerMerchant(merchant, "STARKNET", bytes32(0));

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

    function test_register_duplicate_reverts() public {
        address merchant = address(0x555);
        factory.registerMerchant(merchant, "STARKNET", defaultMintRecipient);
        vm.expectRevert();
        factory.registerMerchant(merchant, "STARKNET", defaultMintRecipient);
    }

    function test_initialize_guard_and_storage() public {
        // Deploy clone manually from implementation to test initialize guards
        address merchant = address(0xABC);
        ChainXReceiver r = new ChainXReceiver();
        r.initialize(
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            defaultMintRecipient,
            merchant
        );
        vm.expectRevert();
        r.initialize(
            address(token),
            treasury,
            address(messenger),
            _starknetDestinationDomain,
            defaultMintRecipient,
            merchant
        );
    }
}

// Privacy-pool test flow:
//
// - Shield-in test: mint funds into a merchant anonymizer and call it as the privacy pool.
//   Verify fee split into a merchant note + treasury note, full-balance approval, and
//   stealth note derivation from merchant/treasury pubkeys + a shared ephemeral key.
// - Access-control test: call the same function from a non-pool address.
//   Verify the contract rejects unauthorized execution.
// - Bridge-out test: mint funds into the bridge anonymizer and call it as the privacy pool.
//   Verify balance-based burn, fixed destination config, and correct CCTP payload.
// - Factory test: register a merchant by pubkey.
//   Verify one anonymizer pair is deployed and duplicate registration is rejected.
//
// Core invariant: fixed config is pinned at deploy time.
// Runtime trigger belongs to the privacy pool.
// The ephemeral pubkey is the only runtime input needed for note derivation, and it's
// reused across both the merchant and treasury note hashes — domain separation comes
// from the two distinct static keys, not from the ephemeral.
//
// Mock token and mock messenger are simple fixtures to check real balance, approval, and burn
// behavior.
//

use core::array::ArrayTrait;
use core::traits::TryInto;
use snforge_std::{
    ContractClass, ContractClassTrait, DeclareResultTrait, declare, start_cheat_caller_address,
    stop_cheat_caller_address,
};
use starknet::{ContractAddress, SyscallResultTrait};
use starknet_beanie::bridge_out::{
    IBridgeOutAnonymizerDispatcher, IBridgeOutAnonymizerDispatcherTrait,
};
use starknet_beanie::merchant_factory::{
    IMerchantFactoryDispatcher, IMerchantFactoryDispatcherTrait,
};
use starknet_beanie::shield_in::{IShieldInAnonymizerDispatcher, IShieldInAnonymizerDispatcherTrait};

#[starknet::interface]
pub trait IToken<T> {
    fn mint(ref self: T, recipient: ContractAddress, amount: u256);
    fn balance_of(self: @T, account: ContractAddress) -> u256;
    fn allowance(self: @T, owner: ContractAddress, spender: ContractAddress) -> u256;
    fn approve(ref self: T, spender: ContractAddress, amount: u256) -> bool;
    fn transfer(ref self: T, recipient: ContractAddress, amount: u256) -> bool;
}

#[starknet::contract]
pub mod MockToken {
    use starknet::storage::{Map, StorageMapReadAccess, StorageMapWriteAccess};
    use starknet::{ContractAddress, get_caller_address};
    use super::IToken;

    #[storage]
    struct Storage {
        balances: Map<ContractAddress, u256>,
        allowances: Map<(ContractAddress, ContractAddress), u256>,
    }

    #[constructor]
    fn constructor(ref self: ContractState) {}

    #[abi(embed_v0)]
    pub impl MockTokenImpl of IToken<ContractState> {
        fn mint(ref self: ContractState, recipient: ContractAddress, amount: u256) {
            let current = self.balances.read(recipient);
            self.balances.write(recipient, current + amount);
        }

        fn balance_of(self: @ContractState, account: ContractAddress) -> u256 {
            self.balances.read(account)
        }

        fn allowance(
            self: @ContractState, owner: ContractAddress, spender: ContractAddress,
        ) -> u256 {
            self.allowances.read((owner, spender))
        }

        fn approve(ref self: ContractState, spender: ContractAddress, amount: u256) -> bool {
            let owner = get_caller_address();
            self.allowances.write((owner, spender), amount);
            true
        }

        fn transfer(ref self: ContractState, recipient: ContractAddress, amount: u256) -> bool {
            let sender = get_caller_address();
            let current = self.balances.read(sender);
            assert(current >= amount, 'INSUFFICIENT_BALANCE');
            self.balances.write(sender, current - amount);
            self.balances.write(recipient, self.balances.read(recipient) + amount);
            true
        }
    }
}

#[starknet::interface]
pub trait IMockMessenger<T> {
    fn deposit_for_burn(
        ref self: T,
        amount: u256,
        destination_domain: u32,
        mint_recipient: u256,
        burn_token: ContractAddress,
        destination_caller: u256,
        max_fee: u256,
        min_finality_threshold: u32,
    );
    fn get_last_amount(self: @T) -> u256;
    fn get_last_destination_domain(self: @T) -> u32;
    fn get_last_mint_recipient(self: @T) -> u256;
    fn get_last_burn_token(self: @T) -> ContractAddress;
    fn get_last_max_fee(self: @T) -> u256;
}

#[starknet::contract]
pub mod MockCctpMessenger {
    use starknet::ContractAddress;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use super::IMockMessenger;

    #[storage]
    struct Storage {
        last_amount: u256,
        last_destination_domain: u32,
        last_mint_recipient: u256,
        last_burn_token: ContractAddress,
        last_max_fee: u256,
    }

    #[constructor]
    fn constructor(ref self: ContractState) {}

    #[abi(embed_v0)]
    pub impl MockCctpMessengerImpl of IMockMessenger<ContractState> {
        fn deposit_for_burn(
            ref self: ContractState,
            amount: u256,
            destination_domain: u32,
            mint_recipient: u256,
            burn_token: ContractAddress,
            destination_caller: u256,
            max_fee: u256,
            min_finality_threshold: u32,
        ) {
            self.last_amount.write(amount);
            self.last_destination_domain.write(destination_domain);
            self.last_mint_recipient.write(mint_recipient);
            self.last_burn_token.write(burn_token);
            self.last_max_fee.write(max_fee);
        }

        fn get_last_amount(self: @ContractState) -> u256 {
            self.last_amount.read()
        }

        fn get_last_destination_domain(self: @ContractState) -> u32 {
            self.last_destination_domain.read()
        }

        fn get_last_mint_recipient(self: @ContractState) -> u256 {
            self.last_mint_recipient.read()
        }

        fn get_last_burn_token(self: @ContractState) -> ContractAddress {
            self.last_burn_token.read()
        }

        fn get_last_max_fee(self: @ContractState) -> u256 {
            self.last_max_fee.read()
        }
    }
}

fn deploy_mock_token() -> ContractAddress {
    // Access .contract_class after unwrapping declare()
    let token_class = declare("MockToken").unwrap_syscall().contract_class();
    let (token_address, _) = token_class.deploy(@array![]).unwrap_syscall();
    token_address
}

fn deploy_mock_messenger() -> ContractAddress {
    let messenger_class = declare("MockCctpMessenger").unwrap_syscall().contract_class();
    let (messenger_address, _) = messenger_class.deploy(@array![]).unwrap_syscall();
    messenger_address
}

fn deploy_shield_in(
    privacy_contract: ContractAddress,
    token: ContractAddress,
    merchant_pubkey: felt252,
    treasury_pubkey: felt252,
) -> ContractAddress {
    let shield_class = declare("ShieldInAnonymizer").unwrap_syscall().contract_class();

    let calldata = array![privacy_contract.into(), token.into(), merchant_pubkey, treasury_pubkey];
    let (shield_addr, _) = shield_class.deploy(@calldata).unwrap_syscall();
    shield_addr
}

fn deploy_bridge_out(
    cctp_messenger: ContractAddress,
    privacy_contract: ContractAddress,
    token: ContractAddress,
    destination_domain: u32,
    mint_recipient: u256,
) -> ContractAddress {
    let bridge_class = declare("BridgeOutAnonymizer").unwrap_syscall().contract_class();
    let calldata = array![
        cctp_messenger.into(), privacy_contract.into(), token.into(), destination_domain.into(),
        mint_recipient.low.into(), mint_recipient.high.into(),
    ];
    let (bridge_addr, _) = bridge_class.deploy(@calldata).unwrap_syscall();
    bridge_addr
}

fn deploy_factory(
    privacy_contract: ContractAddress,
    cctp_messenger: ContractAddress,
    token: ContractAddress,
    base_destination_domain: u32,
    solana_destination_domain: u32,
    eth_destination_domain: u32,
    treasury_pubkey: felt252,
    salt: felt252,
    shield_in_class: ContractClass,
    bridge_out_class: ContractClass,
) -> ContractAddress {
    let factory_class = declare("MerchantFactory").unwrap_syscall().contract_class();
    let calldata = array![
        privacy_contract.into(), cctp_messenger.into(), token.into(),
        base_destination_domain.into(), solana_destination_domain.into(),
        eth_destination_domain.into(), treasury_pubkey, salt, shield_in_class.class_hash.into(),
        bridge_out_class.class_hash.into(),
    ];
    let (factory_addr, _) = factory_class.deploy(@calldata).unwrap_syscall();
    factory_addr
}

#[test]
fn shield_in_happy_path_updates_balances_and_returns_note() {
    let token = deploy_mock_token();
    let privacy_pool: ContractAddress = 0x300.try_into().unwrap();
    let merchant_pubkey: felt252 = 0x777;
    let treasury_pubkey: felt252 = 0x778;
    let anonymous_contract = deploy_shield_in(
        privacy_pool, token, merchant_pubkey, treasury_pubkey,
    );

    let amount: u256 = 1_000_u256;
    let token_dispatcher = ITokenDispatcher { contract_address: token };
    token_dispatcher.mint(anonymous_contract, amount);

    let dispatcher = IShieldInAnonymizerDispatcher { contract_address: anonymous_contract };

    start_cheat_caller_address(anonymous_contract, privacy_pool);
    let result = dispatcher.privacy_invoke(0xabc);
    stop_cheat_caller_address(anonymous_contract);

    // Two notes now: the merchant's net amount, and the treasury's fee — both shielded,
    // neither transferred as a plain ERC20 transfer.
    assert!(result.len() == 2, "RESULT_LEN");
    let merchant_note = *result.at(0);
    let treasury_note = *result.at(1);
    assert!(merchant_note.token == token, "MERCHANT_NOTE_TOKEN");
    assert!(treasury_note.token == token, "TREASURY_NOTE_TOKEN");

    // Fee = 1000 * 50 / 10000 = 5; Net = 995
    assert!(merchant_note.amount == 995_u128, "MERCHANT_NOTE_AMOUNT");
    assert!(treasury_note.amount == 5_u128, "TREASURY_NOTE_AMOUNT");

    // Same ephemeral key, different static keys -> distinct note IDs
    assert!(merchant_note.note_id != treasury_note.note_id, "NOTE_ID_COLLISION");

    // Full gross balance approved to the pool in one call, since both notes are pulled
    // from this contract's balance rather than one being transferred out beforehand.
    assert!(
        token_dispatcher.allowance(anonymous_contract, privacy_pool) == 1_000_u256, "ALLOWANCE",
    );

    assert!(dispatcher.get_privacy_contract() == privacy_pool, "STORAGE_PRIVACY");
    assert!(dispatcher.get_token() == token, "STORAGE_TOKEN");
    assert!(dispatcher.get_merchant_pubkey() == merchant_pubkey, "STORAGE_PUBKEY");
    assert!(dispatcher.get_treasury_pubkey() == treasury_pubkey, "STORAGE_TREASURY");
}

#[test]
#[should_panic(expected: ('CALLER_NOT_PRIVACY_POOL',))]
fn shield_in_rejects_non_pool_caller() {
    let token = deploy_mock_token();
    let privacy_pool: ContractAddress = 0x300.try_into().unwrap();
    let bad_caller: ContractAddress = 0x999.try_into().unwrap();
    let anonymous_contract = deploy_shield_in(privacy_pool, token, 0x777, 0x778);

    start_cheat_caller_address(anonymous_contract, bad_caller);
    IShieldInAnonymizerDispatcher { contract_address: anonymous_contract }.privacy_invoke(0xabc);
    stop_cheat_caller_address(anonymous_contract);
}

#[test]
fn bridge_out_happy_path_uses_contract_balance_and_calls_cctp() {
    let token = deploy_mock_token();
    let privacy_pool: ContractAddress = 0x10.try_into().unwrap();
    let messenger: ContractAddress = deploy_mock_messenger();
    let bridge = deploy_bridge_out(messenger, privacy_pool, token, 2_u32, 0x123_u256);
    let token_dispatcher = ITokenDispatcher { contract_address: token };
    let amount: u256 = 10_000_u256;
    token_dispatcher.mint(bridge, amount);

    start_cheat_caller_address(bridge, privacy_pool);
    IBridgeOutAnonymizerDispatcher { contract_address: bridge }.privacy_invoke();
    stop_cheat_caller_address(bridge);

    let messenger_dispatcher = IMockMessengerDispatcher { contract_address: messenger };
    assert(messenger_dispatcher.get_last_amount() == amount, 'LAST_AMOUNT');
    assert(messenger_dispatcher.get_last_destination_domain() == 2_u32, 'LAST_DOMAIN');
    assert(messenger_dispatcher.get_last_mint_recipient() == 0x123_u256, 'LAST_RECIPIENT');
    assert(messenger_dispatcher.get_last_burn_token() == token, 'LAST_TOKEN');
    assert(messenger_dispatcher.get_last_max_fee() == 15_u256, 'LAST_FEE');

    assert(
        IBridgeOutAnonymizerDispatcher { contract_address: bridge }.get_token() == token,
        'BRIDGE_TOKEN',
    );
    assert(
        IBridgeOutAnonymizerDispatcher { contract_address: bridge }
            .get_destination_domain() == 2_u32,
        'BRIDGE_DOMAIN',
    );
    assert(
        IBridgeOutAnonymizerDispatcher { contract_address: bridge }
            .get_mint_recipient() == 0x123_u256,
        'BRIDGE_RECIP',
    );
}

#[test]
#[should_panic(expected: ('CALLER_NOT_PRIVACY',))]
fn bridge_out_rejects_non_pool_caller() {
    let token = deploy_mock_token();
    let messenger: ContractAddress = deploy_mock_messenger();
    let privacy_pool: ContractAddress = 0x10.try_into().unwrap();
    let bad_caller: ContractAddress = 0x99.try_into().unwrap();
    let bridge = deploy_bridge_out(messenger, privacy_pool, token, 2_u32, 0x123_u256);

    start_cheat_caller_address(bridge, bad_caller);
    IBridgeOutAnonymizerDispatcher { contract_address: bridge }.privacy_invoke();
    stop_cheat_caller_address(bridge);
}

#[test]
fn merchant_factory_registers_pair_and_persists_storage() {
    let privacy_pool: ContractAddress = 0x2.try_into().unwrap();
    let cctp_messenger: ContractAddress = 0x3.try_into().unwrap();
    let token: ContractAddress = 0x4.try_into().unwrap();
    let merchant_pubkey: felt252 = 0xabc;
    let treasury_pubkey: felt252 = 0x999;
    let shield_class = declare("ShieldInAnonymizer").unwrap_syscall().contract_class();
    let bridge_class = declare("BridgeOutAnonymizer").unwrap_syscall().contract_class();
    let factory_addr = deploy_factory(
        privacy_pool,
        cctp_messenger,
        token,
        3_u32,
        5_u32,
        0_u32,
        treasury_pubkey,
        0xfeed,
        shield_class.clone(),
        bridge_class.clone(),
    );

    let pair = IMerchantFactoryDispatcher { contract_address: factory_addr }
        .register_merchant(merchant_pubkey, 'BASE', 0x77_u256);

    assert(pair.shield_in != 0, 'SHIELD_DEPLOYED');
    assert(pair.bridge_out != 0, 'BRIDGE_DEPLOYED');

    let factory_dispatcher = IMerchantFactoryDispatcher { contract_address: factory_addr };
    let stored_pair = factory_dispatcher.get_merchant_pair(merchant_pubkey);
    assert(stored_pair.shield_in == pair.shield_in, 'PAIR_SHIELD');
    assert(stored_pair.bridge_out == pair.bridge_out, 'PAIR_BRIDGE');
}

#[test]
#[should_panic(expected: ('ALREADY_REGISTERED',))]
fn merchant_factory_rejects_duplicate_merchant() {
    let privacy_pool: ContractAddress = 0x2.try_into().unwrap();
    let cctp_messenger: ContractAddress = 0x3.try_into().unwrap();
    let token: ContractAddress = 0x4.try_into().unwrap();
    let treasury_pubkey: felt252 = 0x999;
    let shield_class = declare("ShieldInAnonymizer").unwrap_syscall().contract_class();
    let bridge_class = declare("BridgeOutAnonymizer").unwrap_syscall().contract_class();
    let factory_addr = deploy_factory(
        privacy_pool,
        cctp_messenger,
        token,
        3_u32,
        5_u32,
        0_u32,
        treasury_pubkey,
        0xfeed,
        shield_class.clone(),
        bridge_class.clone(),
    );

    IMerchantFactoryDispatcher { contract_address: factory_addr }
        .register_merchant(0xabc, 'BASE', 0x77_u256);
    IMerchantFactoryDispatcher { contract_address: factory_addr }
        .register_merchant(0xabc, 'BASE', 0x77_u256);
}

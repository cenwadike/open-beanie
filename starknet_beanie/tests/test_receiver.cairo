// StarknetReceiver & MerchantFactory test suite
//
// Invariants tested:
// - Initialization safety & single-invocation restriction.
// - Permissionless sweep execution (caller gets 10% of fee, treasury gets 90%).
// - Same-chain settlement direct to merchant when mint_recipient == 0.
// - Cross-chain CCTP deposit_for_burn with 15 bps max fee when mint_recipient != 0.
// - Idempotent zero-balance sweeps.
// - Factory receiver deployment, state tracking, prediction, and cap enforcement.

use core::array::ArrayTrait;
use core::traits::TryInto;
use snforge_std::{
    ContractClass, ContractClassTrait, DeclareResultTrait, EventSpyAssertionsTrait, declare,
    spy_events, start_cheat_caller_address, stop_cheat_caller_address,
};
use starknet::{ContractAddress, SyscallResultTrait};
use starknet_beanie::merchant_factory::MerchantFactory::{
    Event as MerchantFactoryEvent, ReceiverAnnounced,
};
use starknet_beanie::merchant_factory::{
    IMerchantFactoryDispatcher, IMerchantFactoryDispatcherTrait,
};
use starknet_beanie::receiver::{IStarknetReceiverDispatcher, IStarknetReceiverDispatcherTrait};
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
pub trait IMessageTransmitterV2<T> {
    fn send_message(
        ref self: T,
        destination_domain: u32,
        recipient: u256,
        destination_caller: u256,
        min_finality_threshold: u32,
        message_body: ByteArray,
    );
}

#[starknet::interface]
pub trait IMockMessageTransmitter<T> {
    fn get_last_amount(self: @T) -> u256;
    fn get_last_destination_domain(self: @T) -> u32;
    fn get_last_mint_recipient(self: @T) -> u256;
    fn get_last_burn_token(self: @T) -> u256;
    fn get_last_max_fee(self: @T) -> u256;
}

#[starknet::contract]
pub mod MockCctpMessenger {
    use message::BurnMessageV2;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use super::{IMessageTransmitterV2, IMockMessageTransmitter};

    #[storage]
    struct Storage {
        last_amount: u256,
        last_destination_domain: u32,
        last_mint_recipient: u256,
        last_burn_token: u256,
        last_max_fee: u256,
    }

    #[constructor]
    fn constructor(ref self: ContractState) {}

    #[abi(embed_v0)]
    pub impl MessageTransmitterImpl of IMessageTransmitterV2<ContractState> {
        fn send_message(
            ref self: ContractState,
            destination_domain: u32,
            recipient: u256,
            destination_caller: u256,
            min_finality_threshold: u32,
            message_body: ByteArray,
        ) {
            self.last_destination_domain.write(destination_domain);
            self.last_mint_recipient.write(recipient);
            self.last_amount.write(BurnMessageV2::get_amount(@message_body));
            self.last_burn_token.write(BurnMessageV2::get_burn_token(@message_body));
            self.last_max_fee.write(BurnMessageV2::get_max_fee(@message_body));
        }
    }

    #[abi(embed_v0)]
    pub impl MockGettersImpl of IMockMessageTransmitter<ContractState> {
        fn get_last_amount(self: @ContractState) -> u256 {
            self.last_amount.read()
        }
        fn get_last_destination_domain(self: @ContractState) -> u32 {
            self.last_destination_domain.read()
        }
        fn get_last_mint_recipient(self: @ContractState) -> u256 {
            self.last_mint_recipient.read()
        }
        fn get_last_burn_token(self: @ContractState) -> u256 {
            self.last_burn_token.read()
        }
        fn get_last_max_fee(self: @ContractState) -> u256 {
            self.last_max_fee.read()
        }
    }
}
fn deploy_mock_token() -> ContractAddress {
    let token_class = declare("MockToken").unwrap_syscall().contract_class();
    let (token_address, _) = token_class.deploy(@array![]).unwrap_syscall();
    token_address
}

fn deploy_mock_messenger() -> ContractAddress {
    let messenger_class = declare("MockCctpMessenger").unwrap_syscall().contract_class();
    let (messenger_address, _) = messenger_class.deploy(@array![]).unwrap_syscall();
    messenger_address
}

fn deploy_receiver() -> ContractAddress {
    let receiver_class = declare("StarknetReceiver").unwrap_syscall().contract_class();
    let (receiver_address, _) = receiver_class.deploy(@array![]).unwrap_syscall();
    receiver_address
}

fn deploy_factory(
    token: ContractAddress,
    treasury: ContractAddress,
    token_messenger: ContractAddress,
    base_destination_domain: u32,
    solana_destination_domain: u32,
    eth_destination_domain: u32,
    receiver_class: ContractClass,
) -> ContractAddress {
    let factory_class = declare("MerchantFactory").unwrap_syscall().contract_class();
    let calldata = array![
        receiver_class.class_hash.into(), token.into(), treasury.into(), token_messenger.into(),
        base_destination_domain.into(), solana_destination_domain.into(),
        eth_destination_domain.into(),
    ];
    let (factory_addr, _) = factory_class.deploy(@calldata).unwrap_syscall();
    factory_addr
}

#[test]
fn sweep_same_chain_settlement_transfers_net_to_merchant() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();
    let caller: ContractAddress = 0x999.try_into().unwrap();

    let receiver_addr = deploy_receiver();
    let receiver = IStarknetReceiverDispatcher { contract_address: receiver_addr };
    receiver.initialize(token, treasury, messenger, 0, 0, merchant);

    let gross_amount: u256 = 10_000_u256;
    let token_dispatcher = ITokenDispatcher { contract_address: token };
    token_dispatcher.mint(receiver_addr, gross_amount);

    start_cheat_caller_address(receiver_addr, caller);
    let (net, fee_to_caller, fee_to_treasury, fee) = receiver.sweep();
    stop_cheat_caller_address(receiver_addr);

    // Gross = 10,000 | Fee (0.5%) = 50 | Net = 9,950
    // Fee split: Caller (10% of fee) = 5 | Treasury (90% of fee) = 45
    assert(fee == 50, 'FEE_CALC');
    assert(net == 9950, 'NET_CALC');
    assert(fee_to_caller == 5, 'CALLER_FEE_CALC');
    assert(fee_to_treasury == 45, 'TREASURY_FEE_CALC');

    assert(token_dispatcher.balance_of(merchant) == 9950, 'MERCHANT_BALANCE');
    assert(token_dispatcher.balance_of(treasury) == 45, 'TREASURY_BALANCE');
    assert(token_dispatcher.balance_of(caller) == 5, 'CALLER_BALANCE');
    assert(token_dispatcher.balance_of(receiver_addr) == 0, 'RECEIVER_ZERO');
}

#[test]
fn sweep_cross_chain_settlement_burns_via_cctp() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();
    let caller: ContractAddress = 0x999.try_into().unwrap();
    let mint_recipient: u256 = 0xdef_u256;
    let destination_domain: u32 = 6; // Base domain

    let receiver_addr = deploy_receiver();
    let receiver = IStarknetReceiverDispatcher { contract_address: receiver_addr };
    receiver.initialize(token, treasury, messenger, destination_domain, mint_recipient, merchant);

    let gross_amount: u256 = 10_000_u256;
    let token_dispatcher = ITokenDispatcher { contract_address: token };
    token_dispatcher.mint(receiver_addr, gross_amount);

    start_cheat_caller_address(receiver_addr, caller);
    let (net, fee_to_caller, fee_to_treasury, _) = receiver.sweep();
    stop_cheat_caller_address(receiver_addr);

    let messenger_dispatcher = IMockMessageTransmitterDispatcher { contract_address: messenger };
    assert(messenger_dispatcher.get_last_amount() == net, 'CCTP_AMOUNT');
    assert(messenger_dispatcher.get_last_destination_domain() == destination_domain, 'CCTP_DOMAIN');
    assert(messenger_dispatcher.get_last_mint_recipient() == mint_recipient, 'CCTP_RECIPIENT');

    let token_felt: felt252 = token.into();
    let token_u256: u256 = token_felt.into();
    assert(messenger_dispatcher.get_last_burn_token() == token_u256, 'CCTP_TOKEN');

    // CCTP max_fee (15 bps of gross) = 10_000 * 15 / 10_000 = 15
    assert(messenger_dispatcher.get_last_max_fee() == 15, 'CCTP_MAX_FEE');

    assert(token_dispatcher.balance_of(treasury) == fee_to_treasury, 'TREASURY_FEE_BAL');
    assert(token_dispatcher.balance_of(caller) == fee_to_caller, 'CALLER_FEE_BAL');
}

#[test]
fn sweep_is_idempotent_on_zero_balance() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();

    let receiver_addr = deploy_receiver();
    let receiver = IStarknetReceiverDispatcher { contract_address: receiver_addr };
    receiver.initialize(token, treasury, messenger, 0, 0, merchant);

    let (net, fee_to_caller, fee_to_treasury, fee) = receiver.sweep();
    assert(net == 0 && fee_to_caller == 0 && fee_to_treasury == 0 && fee == 0, 'ZERO_SWEEP');
}

#[test]
#[should_panic(expected: ('ALREADY_INITIALIZED',))]
fn receiver_prevents_double_initialization() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();

    let receiver_addr = deploy_receiver();
    let receiver = IStarknetReceiverDispatcher { contract_address: receiver_addr };
    receiver.initialize(token, treasury, messenger, 0, 0, merchant);
    receiver.initialize(token, treasury, messenger, 0, 0, merchant);
}

#[test]
fn merchant_factory_registers_and_predicts_receiver() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();
    let receiver_class = declare("StarknetReceiver").unwrap_syscall().contract_class();

    let factory_addr = deploy_factory(
        token, treasury, messenger, 6_u32, 5_u32, 0_u32, receiver_class.clone(),
    );

    let factory = IMerchantFactoryDispatcher { contract_address: factory_addr };

    let predicted_addr = factory.predict_receiver_address(merchant);
    let registered_addr = factory.register_merchant(merchant, 'BASE', 0x777_u256);

    assert(predicted_addr == registered_addr, 'PREDICTION_MATCH');

    let receivers = factory.get_merchant_receivers(merchant);
    assert(receivers.len() == 1, 'RECEIVER_COUNT_ONE');
    assert(*receivers.at(0) == registered_addr, 'STORED_MATCH');
}

#[test]
#[should_panic(expected: ('MAX_RECEIVERS_EXCEEDED',))]
fn merchant_factory_rejects_exceeding_max_receivers() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();
    let receiver_class = declare("StarknetReceiver").unwrap_syscall().contract_class();

    let factory_addr = deploy_factory(
        token, treasury, messenger, 6_u32, 5_u32, 0_u32, receiver_class.clone(),
    );

    let factory = IMerchantFactoryDispatcher { contract_address: factory_addr };

    let mut i: u32 = 0;
    while i < 33 {
        factory.register_merchant(merchant, 'BASE', 0x777_u256);
        i += 1;
    };
}
#[test]
fn merchant_factory_announces_receiver() {
    let token = deploy_mock_token();
    let treasury: ContractAddress = 0x888.try_into().unwrap();
    let messenger = deploy_mock_messenger();
    let merchant: ContractAddress = 0x777.try_into().unwrap();
    let receiver_class = declare("StarknetReceiver").unwrap_syscall().contract_class();

    let factory_addr = deploy_factory(
        token, treasury, messenger, 6_u32, 5_u32, 0_u32, receiver_class.clone(),
    );

    let factory = IMerchantFactoryDispatcher { contract_address: factory_addr };

    // Predicted address (view call – no event yet)
    let predicted_addr = factory.predict_receiver_address(merchant);

    // Start spying *before* the state-changing call
    let mut spy = spy_events();

    // This should emit ReceiverAnnounced
    factory.announce_receiver(merchant);

    // Assert the event was emitted with the expected data
    // (nonce is 0 on first announce because we never advanced merchant_nonces)
    spy
        .assert_emitted(
            @array![
                (
                    factory_addr,
                    MerchantFactoryEvent::ReceiverAnnounced(
                        ReceiverAnnounced { merchant, receiver: predicted_addr, nonce: 0 },
                    ),
                ),
            ],
        );

    // Register actually deploys – should still match the announced address
    let deployed_receiver = factory.register_merchant(merchant, 'BASE', 0x777_u256);
    assert(predicted_addr == deployed_receiver, 'PREDICTION_MATCH_AFTER_ANNOUNCE');
}

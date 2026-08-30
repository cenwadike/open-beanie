// MerchantFactory — Cairo port aligned with ChainX MerchantFactory.sol
//
// Deploys single StarknetReceiver instances per merchant and initializes them
// with pinned destinations and settlement targets.

use starknet::ContractAddress;

#[starknet::interface]
pub trait IStarknetReceiver<T> {
    fn initialize(
        ref self: T,
        token: ContractAddress,
        treasury: ContractAddress,
        token_messenger: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256,
        merchant: ContractAddress,
    );
}

#[starknet::interface]
pub trait IMerchantFactory<T> {
    fn register_merchant(
        ref self: T, merchant: ContractAddress, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
    ) -> ContractAddress;
    fn predict_receiver_address(self: @T, merchant: ContractAddress) -> ContractAddress;
    fn get_merchant_receivers(self: @T, merchant: ContractAddress) -> Array<ContractAddress>;
    fn get_merchant_receiver_at(self: @T, merchant: ContractAddress, index: u64) -> ContractAddress;
    fn get_receiver_count(self: @T, merchant: ContractAddress) -> u64;
}

#[starknet::contract]
pub mod MerchantFactory {
    use core::num::traits::Zero;
    use core::poseidon::poseidon_hash_span;
    use core::traits::TryInto;
    use openzeppelin::utils::deployments::calculate_contract_address_from_deploy_syscall;
    use starknet::storage::{
        Map, MutableVecTrait, StorageMapReadAccess, StorageMapWriteAccess, StoragePathEntry,
        StoragePointerReadAccess, StoragePointerWriteAccess, Vec, VecTrait,
    };
    use starknet::syscalls::deploy_syscall;
    use starknet::{ClassHash, ContractAddress, get_contract_address};
    use super::{IMerchantFactory, IStarknetReceiverDispatcher, IStarknetReceiverDispatcherTrait};

    const MAX_RECEIVERS_PER_MERCHANT: u64 = 32;

    #[storage]
    struct Storage {
        receiver_class_hash: ClassHash,
        token: ContractAddress,
        treasury: ContractAddress,
        token_messenger: ContractAddress,
        valid_domains: Map<felt252, bool>,
        destination_domains: Map<felt252, u32>,
        merchant_nonces: Map<ContractAddress, felt252>,
        merchant_receivers: Map<ContractAddress, Vec<ContractAddress>>,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        MerchantRegistered: MerchantRegistered,
    }

    #[derive(Drop, starknet::Event)]
    pub struct MerchantRegistered {
        pub merchant: ContractAddress,
        pub receiver: ContractAddress,
    }

    pub mod Errors {
        pub const MAX_RECEIVERS_EXCEEDED: felt252 = 'MAX_RECEIVERS_EXCEEDED';
        pub const DEPLOY_FAILED: felt252 = 'DEPLOY_FAILED';
        pub const INVALID_DOMAIN: felt252 = 'INVALID_DOMAIN';
        pub const INDEX_OUT_OF_BOUNDS: felt252 = 'INDEX_OUT_OF_BOUNDS';
        pub const ZERO_ADDRESS: felt252 = 'ZERO_ADDRESS';
        pub const INVALID_RECIPIENT: felt252 = 'INVALID_RECIPIENT';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        receiver_class_hash: ClassHash,
        token: ContractAddress,
        treasury: ContractAddress,
        token_messenger: ContractAddress,
        base_destination_domain: u32,
        solana_destination_domain: u32,
        eth_destination_domain: u32,
    ) {
        assert(
            token.is_non_zero() && treasury.is_non_zero() && token_messenger.is_non_zero(),
            Errors::ZERO_ADDRESS,
        );

        self.receiver_class_hash.write(receiver_class_hash);
        self.token.write(token);
        self.treasury.write(treasury);
        self.token_messenger.write(token_messenger);

        self.valid_domains.write('BASE', true);
        self.valid_domains.write('SOLANA', true);
        self.valid_domains.write('ETHEREUM', true);

        self.destination_domains.write('BASE', base_destination_domain);
        self.destination_domains.write('SOLANA', solana_destination_domain);
        self.destination_domains.write('ETHEREUM', eth_destination_domain);
    }

    #[abi(embed_v0)]
    pub impl MerchantFactoryImpl of IMerchantFactory<ContractState> {
        fn register_merchant(
            ref self: ContractState,
            merchant: ContractAddress,
            cctp_mint_chain: felt252,
            cctp_mint_recipient: u256,
        ) -> ContractAddress {
            assert(merchant.is_non_zero(), Errors::ZERO_ADDRESS);

            let mut receivers_vec = self.merchant_receivers.entry(merchant);
            assert(
                receivers_vec.len() < MAX_RECEIVERS_PER_MERCHANT, Errors::MAX_RECEIVERS_EXCEEDED,
            );

            let mut destination_domain: u32 = 0;

            if cctp_mint_chain != 0 {
                assert(self.valid_domains.read(cctp_mint_chain), Errors::INVALID_DOMAIN);
                assert(cctp_mint_recipient != 0, Errors::INVALID_RECIPIENT);
                destination_domain = self.destination_domains.read(cctp_mint_chain);
            } else {
                assert(cctp_mint_recipient == 0, Errors::INVALID_RECIPIENT);
            }

            let nonce = self.merchant_nonces.read(merchant);
            let merchant_felt: felt252 = merchant.into();
            let salt = poseidon_hash_span(array![merchant_felt, nonce].span());
            self.merchant_nonces.write(merchant, nonce + 1);

            let empty_calldata = array![];
            let (receiver_address, _) = deploy_syscall(
                self.receiver_class_hash.read(), salt, empty_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            IStarknetReceiverDispatcher { contract_address: receiver_address }
                .initialize(
                    self.token.read(),
                    self.treasury.read(),
                    self.token_messenger.read(),
                    destination_domain,
                    cctp_mint_recipient,
                    merchant,
                );

            receivers_vec.push(receiver_address);

            self
                .emit(
                    Event::MerchantRegistered(
                        MerchantRegistered { merchant, receiver: receiver_address },
                    ),
                );
            receiver_address
        }

        fn predict_receiver_address(
            self: @ContractState, merchant: ContractAddress,
        ) -> ContractAddress {
            let nonce = self.merchant_nonces.read(merchant);
            let merchant_felt: felt252 = merchant.into();
            let salt = poseidon_hash_span(array![merchant_felt, nonce].span());

            let empty_calldata: Array<felt252> = array![];

            // Uses the official Starknet address calculation matching deploy_syscall
            calculate_contract_address_from_deploy_syscall(
                salt,
                self.receiver_class_hash.read(),
                empty_calldata.span(),
                get_contract_address(),
            )
        }

        fn get_merchant_receivers(
            self: @ContractState, merchant: ContractAddress,
        ) -> Array<ContractAddress> {
            let receivers_vec = self.merchant_receivers.entry(merchant);
            let mut result = array![];
            let len = receivers_vec.len();
            let mut i: u64 = 0;
            while i < len {
                result.append(receivers_vec.at(i).read());
                i += 1;
            }
            result
        }

        fn get_merchant_receiver_at(
            self: @ContractState, merchant: ContractAddress, index: u64,
        ) -> ContractAddress {
            let receivers_vec = self.merchant_receivers.entry(merchant);
            assert(index < receivers_vec.len(), Errors::INDEX_OUT_OF_BOUNDS);
            receivers_vec.at(index).read()
        }

        fn get_receiver_count(self: @ContractState, merchant: ContractAddress) -> u64 {
            let receivers_vec = self.merchant_receivers.entry(merchant);
            receivers_vec.len()
        }
    }
}

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
    fn announce_receiver(
        ref self: T, merchant: ContractAddress, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
    );
    fn predict_receiver_address(
        self: @T, merchant: ContractAddress, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
    ) -> ContractAddress;
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
        merchant_receivers: Map<ContractAddress, Vec<ContractAddress>>,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        MerchantRegistered: MerchantRegistered,
        ReceiverAnnounced: ReceiverAnnounced,
    }

    #[derive(Drop, starknet::Event)]
    pub struct ReceiverAnnounced {
        pub merchant: ContractAddress,
        pub receiver: ContractAddress,
        pub cctp_mint_chain: felt252,
        pub cctp_mint_recipient: u256,
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

    // The receiver's address is a pure function of (merchant, route): one receiver per
    // merchant and route, no counter. The route is in the salt, which is what makes
    // registration safe to leave open. Registering a route twice fails at deploy_syscall.
    fn route_salt(
        merchant: ContractAddress, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
    ) -> felt252 {
        let merchant_felt: felt252 = merchant.into();
        poseidon_hash_span(
            array![
                merchant_felt, cctp_mint_chain, cctp_mint_recipient.low.into(),
                cctp_mint_recipient.high.into(),
            ]
                .span(),
        )
    }

    #[generate_trait]
    impl InternalImpl of InternalTrait {
        /// Validates the route and returns the CCTP destination domain (0 for same-chain).
        fn check_route(
            self: @ContractState, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
        ) -> u32 {
            if cctp_mint_chain != 0 {
                assert(self.valid_domains.read(cctp_mint_chain), Errors::INVALID_DOMAIN);
                assert(cctp_mint_recipient != 0, Errors::INVALID_RECIPIENT);
                self.destination_domains.read(cctp_mint_chain)
            } else {
                assert(cctp_mint_recipient == 0, Errors::INVALID_RECIPIENT);
                0
            }
        }
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

            let destination_domain = self.check_route(cctp_mint_chain, cctp_mint_recipient);

            let salt = route_salt(merchant, cctp_mint_chain, cctp_mint_recipient);

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
            self: @ContractState,
            merchant: ContractAddress,
            cctp_mint_chain: felt252,
            cctp_mint_recipient: u256,
        ) -> ContractAddress {
            let salt = route_salt(merchant, cctp_mint_chain, cctp_mint_recipient);

            let empty_calldata: Array<felt252> = array![];

            // Uses the official Starknet address calculation matching deploy_syscall
            calculate_contract_address_from_deploy_syscall(
                salt,
                self.receiver_class_hash.read(),
                empty_calldata.span(),
                get_contract_address(),
            )
        }

        fn announce_receiver(
            ref self: ContractState,
            merchant: ContractAddress,
            cctp_mint_chain: felt252,
            cctp_mint_recipient: u256,
        ) {
            let _ = self.check_route(cctp_mint_chain, cctp_mint_recipient);

            let salt = route_salt(merchant, cctp_mint_chain, cctp_mint_recipient);

            let empty_calldata: Array<felt252> = array![];
            let receiver = calculate_contract_address_from_deploy_syscall(
                salt,
                self.receiver_class_hash.read(),
                empty_calldata.span(),
                get_contract_address(),
            );

            self
                .emit(
                    Event::ReceiverAnnounced(
                        ReceiverAnnounced {
                            merchant, receiver, cctp_mint_chain, cctp_mint_recipient,
                        },
                    ),
                );
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

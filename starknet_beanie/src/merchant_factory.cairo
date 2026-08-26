// MerchantFactory — Deploys ONE anonymizer pair PER MERCHANT.
//
// Registers the merchant's static spending key (`merchant_pubkey`) into storage
// during `ShieldInAnonymizer` deployment.

use starknet::ContractAddress;

#[derive(Copy, Drop, Serde, starknet::Store)]
pub struct MerchantPair {
    pub shield_in: ContractAddress,
    pub bridge_out: ContractAddress,
}

#[starknet::interface]
pub trait IMerchantFactory<T> {
    fn register_merchant(
        ref self: T,
        merchant_id: felt252,
        privacy_contract: ContractAddress,
        cctp_messenger: ContractAddress,
        token: ContractAddress,
        merchant_pubkey: felt252,
        destination_domain: u32,
        mint_recipient: u256,
        wallet_a: ContractAddress,
        wallet_b: ContractAddress,
    ) -> MerchantPair;
    fn get_merchant_pair(self: @T, merchant_id: felt252) -> MerchantPair;
}

#[starknet::contract]
pub mod MerchantFactory {
    use core::num::traits::Zero;
    use starknet::storage::{
        Map, StorageMapReadAccess, StorageMapWriteAccess, StoragePointerReadAccess,
        StoragePointerWriteAccess,
    };
    use starknet::syscalls::deploy_syscall;
    use starknet::{ClassHash, ContractAddress, get_caller_address};
    use super::{IMerchantFactory, MerchantPair};

    #[storage]
    struct Storage {
        governor: ContractAddress,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
        merchant_pairs: Map<felt252, MerchantPair>,
    }

    pub mod Errors {
        pub const NOT_GOVERNOR: felt252 = 'NOT_GOVERNOR';
        pub const ALREADY_REGISTERED: felt252 = 'ALREADY_REGISTERED';
        pub const DEPLOY_FAILED: felt252 = 'DEPLOY_FAILED';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        governor: ContractAddress,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
    ) {
        self.governor.write(governor);
        self.shield_in_class_hash.write(shield_in_class_hash);
        self.bridge_out_class_hash.write(bridge_out_class_hash);
    }

    #[abi(embed_v0)]
    pub impl MerchantFactoryImpl of IMerchantFactory<ContractState> {
        fn register_merchant(
            ref self: ContractState,
            merchant_id: felt252,
            privacy_contract: ContractAddress,
            cctp_messenger: ContractAddress,
            token: ContractAddress,
            merchant_pubkey: felt252,
            destination_domain: u32,
            mint_recipient: u256,
            wallet_a: ContractAddress,
            wallet_b: ContractAddress,
        ) -> MerchantPair {
            assert(get_caller_address() == self.governor.read(), Errors::NOT_GOVERNOR);
            assert(
                self.merchant_pairs.read(merchant_id).shield_in.is_zero(),
                Errors::ALREADY_REGISTERED,
            );

            // Deploy ShieldInAnonymizer with merchant-pinned config.
            let shield_in_calldata = array![
                privacy_contract.into(), token.into(), merchant_pubkey, wallet_a.into(),
                wallet_b.into(),
            ];
            let (shield_in_addr, _) = deploy_syscall(
                self.shield_in_class_hash.read(), merchant_id, shield_in_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            // Deploy BridgeOutAnonymizer with pinned token and egress parameters.
            let bridge_out_calldata = array![
                cctp_messenger.into(), privacy_contract.into(), token.into(),
                destination_domain.into(), mint_recipient.low.into(), mint_recipient.high.into(),
            ];
            let (bridge_out_addr, _) = deploy_syscall(
                self.bridge_out_class_hash.read(), merchant_id, bridge_out_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            let pair = MerchantPair { shield_in: shield_in_addr, bridge_out: bridge_out_addr };
            self.merchant_pairs.write(merchant_id, pair);
            pair
        }

        fn get_merchant_pair(self: @ContractState, merchant_id: felt252) -> MerchantPair {
            self.merchant_pairs.read(merchant_id)
        }
    }
}

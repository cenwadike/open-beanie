// MerchantFactory — Deploys ONE anonymizer pair PER MERCHANT.
//
// Registers the merchant's static spending key (`merchant_pubkey`) into storage
// during `ShieldInAnonymizer` deployment, alongside a single shared `treasury_pubkey`
// that every merchant's fee note shields to.

use starknet::ContractAddress;

#[derive(Copy, Drop, Serde, starknet::Store)]
pub struct MerchantPair {
    pub shield_in: ContractAddress,
    pub bridge_out: ContractAddress,
}

#[starknet::interface]
pub trait IMerchantFactory<T> {
    fn register_merchant(ref self: T, merchant_pubkey: felt252) -> MerchantPair;
    fn get_merchant_pair(self: @T, merchant_pubkey: felt252) -> MerchantPair;
}

#[starknet::contract]
pub mod MerchantFactory {
    use core::num::traits::Zero;
    use core::poseidon::poseidon_hash_span;
    use starknet::storage::{
        Map, StorageMapReadAccess, StorageMapWriteAccess, StoragePointerReadAccess,
        StoragePointerWriteAccess,
    };
    use starknet::syscalls::deploy_syscall;
    use starknet::{ClassHash, ContractAddress};
    use super::{IMerchantFactory, MerchantPair};

    #[storage]
    struct Storage {
        privacy_contract: ContractAddress,
        cctp_messenger: ContractAddress,
        token: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256,
        treasury_pubkey: felt252,
        salt: felt252,
        merchant_nonces: Map<felt252, felt252>,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
        merchant_pairs: Map<felt252, MerchantPair>,
    }

    pub mod Errors {
        pub const ALREADY_REGISTERED: felt252 = 'ALREADY_REGISTERED';
        pub const DEPLOY_FAILED: felt252 = 'DEPLOY_FAILED';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        privacy_contract: ContractAddress,
        cctp_messenger: ContractAddress,
        token: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256,
        treasury_pubkey: felt252,
        salt: felt252,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
    ) {
        self.privacy_contract.write(privacy_contract);
        self.cctp_messenger.write(cctp_messenger);
        self.token.write(token);
        self.destination_domain.write(destination_domain);
        self.mint_recipient.write(mint_recipient);
        self.treasury_pubkey.write(treasury_pubkey);
        self.salt.write(salt);
        self.shield_in_class_hash.write(shield_in_class_hash);
        self.bridge_out_class_hash.write(bridge_out_class_hash);
    }

    #[abi(embed_v0)]
    pub impl MerchantFactoryImpl of IMerchantFactory<ContractState> {
        fn register_merchant(ref self: ContractState, merchant_pubkey: felt252) -> MerchantPair {
            assert(
                self.merchant_pairs.read(merchant_pubkey).shield_in.is_zero(),
                Errors::ALREADY_REGISTERED,
            );

            // Each merchant owns an independent nonce sequence, while the factory salt stays fixed
            // across all deployments from this factory instance.
            let merchant_nonce = self.merchant_nonces.read(merchant_pubkey);
            let salt = poseidon_hash_span(
                array![merchant_pubkey, merchant_nonce, self.salt.read()].span(),
            );
            self.merchant_nonces.write(merchant_pubkey, merchant_nonce + 1);

            let privacy_contract = self.privacy_contract.read();
            let token = self.token.read();
            let treasury_pubkey = self.treasury_pubkey.read();
            let cctp_messenger = self.cctp_messenger.read();
            let destination_domain = self.destination_domain.read();
            let mint_recipient = self.mint_recipient.read();

            let shield_in_calldata = array![
                privacy_contract.into(), token.into(), merchant_pubkey, treasury_pubkey,
            ];
            let (shield_in_addr, _) = deploy_syscall(
                self.shield_in_class_hash.read(), salt, shield_in_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            let bridge_out_calldata = array![
                cctp_messenger.into(), privacy_contract.into(), token.into(),
                destination_domain.into(), mint_recipient.low.into(), mint_recipient.high.into(),
            ];
            let (bridge_out_addr, _) = deploy_syscall(
                self.bridge_out_class_hash.read(), salt, bridge_out_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            let pair = MerchantPair { shield_in: shield_in_addr, bridge_out: bridge_out_addr };
            self.merchant_pairs.write(merchant_pubkey, pair);
            pair
        }

        fn get_merchant_pair(self: @ContractState, merchant_pubkey: felt252) -> MerchantPair {
            self.merchant_pairs.read(merchant_pubkey)
        }
    }
}

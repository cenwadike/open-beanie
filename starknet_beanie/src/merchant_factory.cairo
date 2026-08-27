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
    fn register_merchant(
        ref self: T, merchant_pubkey: felt252, cctp_mint_chain: felt252, cctp_mint_recipient: u256,
    ) -> MerchantPair;
    fn predict_shield_in_address(self: @T, merchant_pubkey: felt252) -> ContractAddress;
    fn get_merchant_pairs(self: @T, merchant_pubkey: felt252) -> Array<MerchantPair>;
    fn get_merchant_pair_at(self: @T, merchant_pubkey: felt252, index: u64) -> MerchantPair;
}

#[starknet::contract]
pub mod MerchantFactory {
    use core::poseidon::poseidon_hash_span;
    use core::traits::TryInto;
    use starknet::storage::{
        Map, MutableVecTrait, StorageMapReadAccess, StorageMapWriteAccess, StoragePathEntry,
        StoragePointerReadAccess, StoragePointerWriteAccess, Vec, VecTrait,
    };
    use starknet::syscalls::deploy_syscall;
    use starknet::{ClassHash, ContractAddress, get_contract_address};
    use super::{IMerchantFactory, MerchantPair};

    const MAX_PAIRS_PER_MERCHANT: u64 = 32;

    #[storage]
    struct Storage {
        privacy_contract: ContractAddress,
        cctp_messenger: ContractAddress,
        token: ContractAddress,
        valid_domains: Map<felt252, felt252>,
        destination_domains: Map<felt252, u32>,
        treasury_pubkey: felt252,
        salt: felt252,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
        merchant_nonces: Map<felt252, felt252>,
        merchant_pairs: Map<felt252, Vec<MerchantPair>>,
    }

    pub mod Errors {
        pub const MAX_PAIRS_EXCEEDED: felt252 = 'MAX_RECEIVERS_EXCEEDED';
        pub const DEPLOY_FAILED: felt252 = 'DEPLOY_FAILED';
        pub const INVALID_DOMAIN: felt252 = 'INVALID_DOMAIN';
        pub const INDEX_OUT_OF_BOUNDS: felt252 = 'INDEX_OUT_OF_BOUNDS';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        privacy_contract: ContractAddress,
        cctp_messenger: ContractAddress,
        token: ContractAddress,
        base_destination_domain: u32,
        solana_destination_domain: u32,
        eth_destination_domain: u32,
        treasury_pubkey: felt252,
        salt: felt252,
        shield_in_class_hash: ClassHash,
        bridge_out_class_hash: ClassHash,
    ) {
        self.privacy_contract.write(privacy_contract);
        self.cctp_messenger.write(cctp_messenger);
        self.token.write(token);

        self.valid_domains.write('BASE', true.into());
        self.valid_domains.write('SOLANA', true.into());
        self.valid_domains.write('ETHEREUM', true.into());

        self.destination_domains.write('BASE', base_destination_domain);
        self.destination_domains.write('SOLANA', solana_destination_domain);
        self.destination_domains.write('ETHEREUM', eth_destination_domain);
        self.treasury_pubkey.write(treasury_pubkey);
        self.salt.write(salt);
        self.shield_in_class_hash.write(shield_in_class_hash);
        self.bridge_out_class_hash.write(bridge_out_class_hash);
    }

    #[abi(embed_v0)]
    pub impl MerchantFactoryImpl of IMerchantFactory<ContractState> {
        fn register_merchant(
            ref self: ContractState,
            merchant_pubkey: felt252,
            cctp_mint_chain: felt252,
            cctp_mint_recipient: u256,
        ) -> MerchantPair {
            // Get mutable reference to the merchant's vector pointer
            let mut pairs_vec = self.merchant_pairs.entry(merchant_pubkey);
            assert(pairs_vec.len() < MAX_PAIRS_PER_MERCHANT, Errors::MAX_PAIRS_EXCEEDED);

            let merchant_nonce = self.merchant_nonces.read(merchant_pubkey);
            let salt = poseidon_hash_span(
                array![merchant_pubkey, merchant_nonce, self.salt.read()].span(),
            );
            self.merchant_nonces.write(merchant_pubkey, merchant_nonce + 1);

            let privacy_contract = self.privacy_contract.read();
            let token = self.token.read();
            let treasury_pubkey = self.treasury_pubkey.read();
            let cctp_messenger = self.cctp_messenger.read();

            assert(self.valid_domains.read(cctp_mint_chain) != 0, Errors::INVALID_DOMAIN);
            let destination_domain = self.destination_domains.read(cctp_mint_chain);

            let shield_in_calldata = array![
                privacy_contract.into(), token.into(), merchant_pubkey, treasury_pubkey,
            ];
            let (shield_in_addr, _) = deploy_syscall(
                self.shield_in_class_hash.read(), salt, shield_in_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            let bridge_out_calldata = array![
                cctp_messenger.into(), privacy_contract.into(), token.into(),
                destination_domain.into(), cctp_mint_recipient.low.into(),
                cctp_mint_recipient.high.into(),
            ];
            let (bridge_out_addr, _) = deploy_syscall(
                self.bridge_out_class_hash.read(), salt, bridge_out_calldata.span(), false,
            )
                .expect(Errors::DEPLOY_FAILED);

            let pair = MerchantPair { shield_in: shield_in_addr, bridge_out: bridge_out_addr };

            // Push directly to storage vector entry
            pairs_vec.push(pair);
            pair
        }

        fn predict_shield_in_address(
            self: @ContractState, merchant_pubkey: felt252,
        ) -> ContractAddress {
            let merchant_nonce = self.merchant_nonces.read(merchant_pubkey);
            let salt = poseidon_hash_span(
                array![merchant_pubkey, merchant_nonce, self.salt.read()].span(),
            );

            let shield_in_calldata = array![
                self.privacy_contract.read().into(), self.token.read().into(), merchant_pubkey,
                self.treasury_pubkey.read(),
            ];
            let calldata_hash = poseidon_hash_span(shield_in_calldata.span());

            let deployer_address: felt252 = get_contract_address().into();
            let class_hash_felt: felt252 = self.shield_in_class_hash.read().into();

            let raw_address = poseidon_hash_span(
                array![
                    'STARKNET_CONTRACT_ADDRESS', deployer_address, salt, class_hash_felt,
                    calldata_hash,
                ]
                    .span(),
            );

            raw_address.try_into().unwrap()
        }

        fn get_merchant_pairs(
            self: @ContractState, merchant_pubkey: felt252,
        ) -> Array<MerchantPair> {
            let pairs_vec = self.merchant_pairs.entry(merchant_pubkey);
            let mut result = array![];
            let len = pairs_vec.len();
            let mut i: u64 = 0;
            while i < len {
                result.append(pairs_vec.at(i).read());
                i += 1;
            }
            result
        }

        fn get_merchant_pair_at(
            self: @ContractState, merchant_pubkey: felt252, index: u64,
        ) -> MerchantPair {
            let pairs_vec = self.merchant_pairs.entry(merchant_pubkey);
            assert(index < pairs_vec.len(), Errors::INDEX_OUT_OF_BOUNDS);
            pairs_vec.at(index).read()
        }
    }
}

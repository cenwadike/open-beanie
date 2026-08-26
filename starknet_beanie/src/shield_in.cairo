// ShieldInAnonymizer — Deployed PER MERCHANT.
//
// Provides strk20 privacy guarantees via
// non-interactive stealth address derivation (ECDH-style).
//
// The merchant's static public key (`merchant_pubkey`) is pinned in storage at deploy.
// During `privacy_invoke`, the caller provides an `ephemeral_pubkey`. The contract
// deterministically hashes these keys together to derive a unique, single-use `note_id`.
// Even if an attacker triggers this with their own ephemeral key, the derived note ID
// is cryptographically bound to the merchant's key—preventing fund redirection.

use privacy::objects::OpenNoteDeposit;

#[starknet::interface]
pub trait IShieldInAnonymizer<T> {
    fn privacy_invoke(ref self: T, ephemeral_pubkey: felt252) -> Span<OpenNoteDeposit>;
}

#[starknet::contract]
pub mod ShieldInAnonymizer {
    use core::poseidon::poseidon_hash_span;
    use openzeppelin::interfaces::token::erc20::{IERC20Dispatcher, IERC20DispatcherTrait};
    use privacy::objects::OpenNoteDeposit;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::{ContractAddress, get_caller_address, get_contract_address};
    use super::IShieldInAnonymizer;

    const FEE_BPS: u256 = 50; // 0.50%
    const BPS_DENOM: u256 = 10000;

    #[storage]
    struct Storage {
        privacy_contract: ContractAddress, // Pool address pinned at deploy
        token: ContractAddress, // Token pinned at deploy
        merchant_pubkey: felt252, // Merchant's static spending key pinned at deploy
        wallet_a: ContractAddress,
        wallet_b: ContractAddress,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        Shielded: Shielded,
    }

    #[derive(Drop, starknet::Event)]
    pub struct Shielded {
        pub gross: u256,
        pub net: u256,
        pub fee: u256,
        pub to_a: u256,
        pub to_b: u256,
        pub stealth_note_id: felt252,
        pub ephemeral_pubkey: felt252,
    }

    pub mod Errors {
        pub const CALLER_NOT_PRIVACY_POOL: felt252 = 'CALLER_NOT_PRIVACY_POOL';
        pub const INVALID_EPHEMERAL_KEY: felt252 = 'INVALID_EPHEMERAL_KEY';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        privacy_contract: ContractAddress,
        token: ContractAddress,
        merchant_pubkey: felt252,
        wallet_a: ContractAddress,
        wallet_b: ContractAddress,
    ) {
        self.privacy_contract.write(privacy_contract);
        self.token.write(token);
        self.merchant_pubkey.write(merchant_pubkey);
        self.wallet_a.write(wallet_a);
        self.wallet_b.write(wallet_b);
    }

    #[abi(embed_v0)]
    pub impl ShieldInAnonymizerImpl of IShieldInAnonymizer<ContractState> {
        fn privacy_invoke(
            ref self: ContractState, ephemeral_pubkey: felt252,
        ) -> Span<OpenNoteDeposit> {
            assert(
                get_caller_address() == self.privacy_contract.read(),
                Errors::CALLER_NOT_PRIVACY_POOL,
            );
            assert(ephemeral_pubkey != 0, Errors::INVALID_EPHEMERAL_KEY);

            let token = self.token.read();

            let erc20 = IERC20Dispatcher { contract_address: token };
            let gross: u256 = erc20.balance_of(get_contract_address());
            if gross == 0 {
                return [].span();
            }

            // Derives a unique stealth note target locked to the merchant's spending key
            let static_key = self.merchant_pubkey.read();
            let stealth_note_id = poseidon_hash_span(array![static_key, ephemeral_pubkey].span());

            let fee = (gross * FEE_BPS) / BPS_DENOM;
            let net = gross - fee;
            let to_a = (fee * 60) / 100;
            let to_b = fee - to_a;

            if to_a > 0 {
                erc20.transfer(self.wallet_a.read(), to_a);
            }
            if to_b > 0 {
                erc20.transfer(self.wallet_b.read(), to_b);
            }

            let amount: u128 = net.try_into().expect('BALANCE_OVERFLOW');
            erc20.approve(get_caller_address(), amount.into());

            self.emit(Shielded { gross, net, fee, to_a, to_b, stealth_note_id, ephemeral_pubkey });

            [OpenNoteDeposit { note_id: stealth_note_id, token, amount }].span()
        }
    }
}

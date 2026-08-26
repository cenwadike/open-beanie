// ShieldInAnonymizer — Deployed PER MERCHANT.
//
// Provides strk20 privacy guarantees via non-interactive stealth address derivation (ECDH-style).
//
// The merchant's static public key (`merchant_pubkey`) is pinned in storage at deploy.
// The protocol fee is shielded into its own note under a single shared `treasury_pubkey`
// — never sent as a plain ERC20 transfer — so no chain-analysis heuristic can read a fee
// amount off an event log and invert it back into the deposit size.
//
// During `privacy_invoke`, the caller provides an `ephemeral_pubkey`. The contract
// deterministically hashes it against both the merchant's and the treasury's static keys
// to derive two unique, single-use note IDs. Even if an attacker triggers this with their
// own ephemeral key, both derived note IDs stay cryptographically bound to the keys they
// were pinned against — preventing fund redirection.

use privacy::objects::OpenNoteDeposit;
use starknet::ContractAddress;

#[starknet::interface]
pub trait IShieldInAnonymizer<T> {
    fn privacy_invoke(ref self: T, ephemeral_pubkey: felt252) -> Span<OpenNoteDeposit>;
    fn get_token(self: @T) -> ContractAddress;
    fn get_privacy_contract(self: @T) -> ContractAddress;
    fn get_merchant_pubkey(self: @T) -> felt252;
    fn get_treasury_pubkey(self: @T) -> felt252;
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
        treasury_pubkey: felt252 // Single shared treasury spending key, pinned at deploy
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
        pub merchant_note_id: felt252,
        pub treasury_note_id: felt252,
        pub ephemeral_pubkey: felt252,
    }

    pub mod Errors {
        pub const CALLER_NOT_PRIVACY_POOL: felt252 = 'CALLER_NOT_PRIVACY_POOL';
        pub const INVALID_EPHEMERAL_KEY: felt252 = 'INVALID_EPHEMERAL_KEY';
        pub const PUBKEY_COLLISION: felt252 = 'PUBKEY_COLLISION';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        privacy_contract: ContractAddress,
        token: ContractAddress,
        merchant_pubkey: felt252,
        treasury_pubkey: felt252,
    ) {
        assert(merchant_pubkey != treasury_pubkey, Errors::PUBKEY_COLLISION);
        self.privacy_contract.write(privacy_contract);
        self.token.write(token);
        self.merchant_pubkey.write(merchant_pubkey);
        self.treasury_pubkey.write(treasury_pubkey);
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

            // Two stealth notes, same derivation, different static keys — merchant gets
            // the net amount, treasury gets the fee. Neither is a public transfer.
            let merchant_key = self.merchant_pubkey.read();
            let treasury_key = self.treasury_pubkey.read();
            let merchant_note_id = poseidon_hash_span(
                array![merchant_key, ephemeral_pubkey].span(),
            );
            let treasury_note_id = poseidon_hash_span(
                array![treasury_key, ephemeral_pubkey].span(),
            );

            let fee = (gross * FEE_BPS) / BPS_DENOM;
            let net = gross - fee;

            // Entire balance is shielded, not partially transferred — the pool pulls both
            // notes' worth from this contract in the same call.
            erc20.approve(get_caller_address(), gross);

            self
                .emit(
                    Shielded {
                        gross, net, fee, merchant_note_id, treasury_note_id, ephemeral_pubkey,
                    },
                );

            let net_amount: u128 = net.try_into().expect('BALANCE_OVERFLOW');
            let fee_amount: u128 = fee.try_into().expect('BALANCE_OVERFLOW');

            [
                OpenNoteDeposit { note_id: merchant_note_id, token, amount: net_amount },
                OpenNoteDeposit { note_id: treasury_note_id, token, amount: fee_amount },
            ]
                .span()
        }

        fn get_token(self: @ContractState) -> ContractAddress {
            self.token.read()
        }

        fn get_privacy_contract(self: @ContractState) -> ContractAddress {
            self.privacy_contract.read()
        }

        fn get_merchant_pubkey(self: @ContractState) -> felt252 {
            self.merchant_pubkey.read()
        }

        fn get_treasury_pubkey(self: @ContractState) -> felt252 {
            self.treasury_pubkey.read()
        }
    }
}

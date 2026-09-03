// StealthAccount — minimal SNIP-6 account, single-purpose: owns a
// stealth pubkey derived off-chain, and execute claims.
//
// The account is a "dual-signer" design: the stealth pubkey is the
// "client" signer, and the paymaster is the "cosigner" signer.

use starknet::ContractAddress;

#[starknet::interface]
pub trait ISRC6<T> {
    fn __execute__(ref self: T, calls: Array<Call>) -> Array<Span<felt252>>;
    fn __validate__(ref self: T, calls: Array<Call>) -> felt252;
    fn is_valid_signature(self: @T, hash: felt252, signature: Array<felt252>) -> felt252;
}

#[starknet::interface]
pub trait ISRC5<T> {
    fn supports_interface(self: @T, interface_id: felt252) -> bool;
}

#[derive(Drop, Serde)]
pub struct Call {
    pub to: ContractAddress,
    pub selector: felt252,
    pub calldata: Span<felt252>,
}

#[starknet::contract(account)]
pub mod StealthAccount {
    use core::ecdsa::check_ecdsa_signature;
    use core::num::traits::Zero;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::syscalls::call_contract_syscall;
    use starknet::{SyscallResultTrait, get_caller_address, get_tx_info};
    use super::{Call, ISRC5, ISRC6};

    const ISRC6_ID: felt252 = 0x2ceccef7f994940b3962a6c67e0ba4fcd37df7d131417c604f91e03caecc1cd;
    const ISRC5_ID: felt252 = 0x3f918d17e5ee77373b56385708f855659a07f75997f365cf87748628532a9;

    #[storage]
    struct Storage {
        client_pubkey: felt252,
        cosigner_pubkey: felt252,
    }

    pub mod Errors {
        pub const INVALID_CALLER: felt252 = 'INVALID_CALLER';
        pub const INVALID_SIGNATURE: felt252 = 'INVALID_SIGNATURE';
        pub const ZERO_PUBKEY: felt252 = 'ZERO_PUBKEY';
    }

    #[constructor]
    fn constructor(ref self: ContractState, client_pubkey: felt252, cosigner_pubkey: felt252) {
        // Enforce both keys must be non-zero — single-key fallback strictly prohibited
        assert(client_pubkey != 0, Errors::ZERO_PUBKEY);
        assert(cosigner_pubkey != 0, Errors::ZERO_PUBKEY);
        self.client_pubkey.write(client_pubkey);
        self.cosigner_pubkey.write(cosigner_pubkey);
    }

    #[abi(embed_v0)]
    pub impl SRC6Impl of ISRC6<ContractState> {
        fn __validate__(ref self: ContractState, calls: Array<Call>) -> felt252 {
            assert(get_caller_address().is_zero(), Errors::INVALID_CALLER);
            let tx_info = get_tx_info().unbox();
            let is_valid = self._is_valid_signature(tx_info.transaction_hash, tx_info.signature);
            assert(is_valid, Errors::INVALID_SIGNATURE);
            starknet::VALIDATED
        }

        fn __execute__(ref self: ContractState, calls: Array<Call>) -> Array<Span<felt252>> {
            assert(get_caller_address().is_zero(), Errors::INVALID_CALLER);

            let mut results = array![];
            let mut i: u32 = 0;
            let len = calls.len();
            while i < len {
                let call = calls.at(i);
                let res = call_contract_syscall(*call.to, *call.selector, *call.calldata)
                    .unwrap_syscall();
                results.append(res);
                i += 1;
            }
            results
        }

        fn is_valid_signature(
            self: @ContractState, hash: felt252, signature: Array<felt252>,
        ) -> felt252 {
            if self._is_valid_signature(hash, signature.span()) {
                starknet::VALIDATED
            } else {
                0
            }
        }
    }

    #[external(v0)]
    fn __validate_deploy__(
        self: @ContractState,
        class_hash: felt252,
        contract_address_salt: felt252,
        client_pubkey: felt252,
        cosigner_pubkey: felt252,
    ) -> felt252 {
        assert(get_caller_address().is_zero(), Errors::INVALID_CALLER);
        let tx_info = get_tx_info().unbox();
        let is_valid = self._is_valid_signature(tx_info.transaction_hash, tx_info.signature);
        assert(is_valid, Errors::INVALID_SIGNATURE);
        starknet::VALIDATED
    }

    #[external(v0)]
    fn __validate_declare__(self: @ContractState, class_hash: felt252) -> felt252 {
        assert(get_caller_address().is_zero(), Errors::INVALID_CALLER);
        let tx_info = get_tx_info().unbox();
        let is_valid = self._is_valid_signature(tx_info.transaction_hash, tx_info.signature);
        assert(is_valid, Errors::INVALID_SIGNATURE);
        starknet::VALIDATED
    }

    #[abi(embed_v0)]
    pub impl SRC5Impl of ISRC5<ContractState> {
        fn supports_interface(self: @ContractState, interface_id: felt252) -> bool {
            interface_id == ISRC6_ID || interface_id == ISRC5_ID
        }
    }

    #[generate_trait]
    impl InternalImpl of InternalTrait {
        fn _is_valid_signature(
            self: @ContractState, hash: felt252, signature: Span<felt252>,
        ) -> bool {
            // Strictly require 4 signature elements [r1, s1, r2, s2]
            if signature.len() != 4 {
                return false;
            }

            let r1 = *signature.at(0);
            let s1 = *signature.at(1);
            let r2 = *signature.at(2);
            let s2 = *signature.at(3);

            // 1. Verify Client Signature (s1)
            let valid_client = check_ecdsa_signature(hash, self.client_pubkey.read(), r1, s1);
            if !valid_client {
                return false;
            }

            // 2. Verify Cosigner/TEE Signature (s2)
            check_ecdsa_signature(hash, self.cosigner_pubkey.read(), r2, s2)
        }
    }
}

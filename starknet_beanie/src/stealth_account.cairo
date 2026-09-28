// StealthAccount — minimal SNIP-6 account, single-purpose: owns a
// stealth pubkey derived off-chain, and executes claims.
//
// Dual-signer design: the stealth pubkey (STARK curve) is the "client"
// signer. The cosigner is verified over secp256k1 via ECDSA public-key
// recovery, compared against a stored Ethereum address — the exact
// pattern used in Starknet's own docs (docs.starknet.io, Starknet by
// Example, "ECDSA Verification"). This avoids constructing/storing a
// raw curve point, which was the source of the last two compile errors.

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
    use starknet::eth_address::EthAddress;
    use starknet::eth_signature::public_key_point_to_eth_address;
    use starknet::secp256_trait::{Signature, recover_public_key, signature_from_vrs};
    use starknet::secp256k1::Secp256k1Point;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::syscalls::call_contract_syscall;
    use starknet::{SyscallResultTrait, get_caller_address, get_tx_info};
    use super::{Call, ISRC5, ISRC6};

    const ISRC6_ID: felt252 = 0x2ceccef7f994940b3962a6c67e0ba4fcd37df7d131417c604f91e03caecc1cd;
    const ISRC5_ID: felt252 = 0x3f918d17e5ee77373b56385708f855659a07f75997f365cf87748628532a9;

    #[storage]
    struct Storage {
        client_pubkey: felt252,
        // Cosigner identified by its Ethereum address (20 bytes), not a
        // raw curve point. Verification recovers the signer's pubkey
        // from the signature and compares its derived address to this.
        cosigner_eth_address: EthAddress,
    }

    pub mod Errors {
        pub const INVALID_CALLER: felt252 = 'INVALID_CALLER';
        pub const INVALID_SIGNATURE: felt252 = 'INVALID_SIGNATURE';
        pub const ZERO_PUBKEY: felt252 = 'ZERO_PUBKEY';
        pub const BAD_SIGNATURE_LEN: felt252 = 'BAD_SIGNATURE_LEN';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState, client_pubkey: felt252, cosigner_eth_address: EthAddress,
    ) {
        assert(client_pubkey != 0, Errors::ZERO_PUBKEY);
        let cosigner_felt: felt252 = cosigner_eth_address.into();
        assert(cosigner_felt != 0, Errors::ZERO_PUBKEY);

        self.client_pubkey.write(client_pubkey);
        self.cosigner_eth_address.write(cosigner_eth_address);
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
        cosigner_eth_address: EthAddress,
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
            // Layout: [r1, s1, r2_low, r2_high, s2_low, s2_high, v2]
            // r1/s1: STARK-curve client signature, unchanged.
            // r2/s2: secp256k1 cosigner signature, each a u256 packed as
            //        two felt252 limbs (low, then high).
            // v2: recovery id / parity (0 or 1, or raw 27/28 — must match
            //     whatever signature_from_vrs expects; confirm against
            //     what your worker actually sends).
            if signature.len() != 7 {
                return false;
            }

            let r1 = *signature.at(0);
            let s1 = *signature.at(1);

            let valid_client = check_ecdsa_signature(hash, self.client_pubkey.read(), r1, s1);
            if !valid_client {
                return false;
            }

            let r2_low: u128 = (*signature.at(2)).try_into().unwrap();
            let r2_high: u128 = (*signature.at(3)).try_into().unwrap();
            let s2_low: u128 = (*signature.at(4)).try_into().unwrap();
            let s2_high: u128 = (*signature.at(5)).try_into().unwrap();
            let v2: u32 = (*signature.at(6)).try_into().unwrap();

            let r2: u256 = u256 { low: r2_low, high: r2_high };
            let s2: u256 = u256 { low: s2_low, high: s2_high };
            let msg_hash: u256 = hash.into();

            let cosigner_sig: Signature = signature_from_vrs(v2, r2, s2);
            if let Option::Some(recovered_point) =
                recover_public_key::<Secp256k1Point>(msg_hash, cosigner_sig) {
                let recovered_address = public_key_point_to_eth_address(recovered_point);
                recovered_address == self.cosigner_eth_address.read()
            } else {
                false
            }
        }
    }
}

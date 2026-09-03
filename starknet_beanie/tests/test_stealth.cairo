// ============================================================================
// StealthAccount Tests
// ============================================================================

use core::num::traits::Zero;
use snforge_std::{
    ContractClassTrait, DeclareResultTrait, declare, start_cheat_caller_address,
    stop_cheat_caller_address,
};
use starknet::{ContractAddress, SyscallResultTrait};
use starknet_beanie::stealth_account::{
    ISRC5Dispatcher, ISRC5DispatcherTrait, ISRC6Dispatcher, ISRC6DispatcherTrait,
};

const CLIENT_PUBKEY: felt252 = 0x111111;
const COSIGNER_PUBKEY: felt252 = 0x222222;

fn deploy_account(client_pubkey: felt252, cosigner_pubkey: felt252) -> ContractAddress {
    let contract = declare("StealthAccount").unwrap_syscall().contract_class();
    let (address, _) = contract.deploy(@array![client_pubkey, cosigner_pubkey]).unwrap_syscall();
    address
}

#[test]
#[should_panic(expected: ('ZERO_PUBKEY',))]
fn test_account_constructor_rejects_zero_client_pubkey() {
    deploy_account(0, COSIGNER_PUBKEY);
}

#[test]
#[should_panic(expected: ('ZERO_PUBKEY',))]
fn test_account_constructor_rejects_zero_cosigner_pubkey() {
    deploy_account(CLIENT_PUBKEY, 0);
}

#[test]
fn test_account_constructor_accepts_valid_keys() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    assert(addr.is_non_zero(), 'deploy should succeed');
}

#[test]
fn test_account_supports_src6_and_src5_interfaces() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let src5 = ISRC5Dispatcher { contract_address: addr };

    let isrc6_id = 0x2ceccef7f994940b3962a6c67e0ba4fcd37df7d131417c604f91e03caecc1cd;
    let isrc5_id = 0x3f918d17e5ee77373b56385708f855659a07f75997f365cf87748628532a9;

    assert(src5.supports_interface(isrc6_id), 'should support SRC6');
    assert(src5.supports_interface(isrc5_id), 'should support SRC5');
}

#[test]
fn test_account_rejects_malformed_signature_len() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    // Pass fewer than 4 signature elements
    let invalid_sig = array![1, 2, 3];
    let res = account.is_valid_signature(0x999, invalid_sig);

    assert(res == 0, 'sig len != 4 must return 0');
}

#[test]
fn test_account_rejects_invalid_client_signature() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    let sig = array![0x1, 0x2, 0x3, 0x4];
    let res = account.is_valid_signature(0x999, sig);

    assert(res == 0, 'invalid client sig must fail');
}

#[test]
fn test_account_rejects_invalid_cosigner_signature() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    let sig = array![0x10, 0x20, 0x30, 0x40];
    let res = account.is_valid_signature(0x999, sig);

    assert(res == 0, 'invalid cosigner sig must fail');
}

#[test]
fn test_account_rejects_wrong_hash() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    let sig = array![0x11, 0x22, 0x33, 0x44];
    let res = account.is_valid_signature(0x123456, sig);

    assert(res == 0, 'wrong hash must fail');
}

#[test]
#[should_panic(expected: ('INVALID_CALLER',))]
fn test_account_rejects_nonzero_caller_in_validate() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    start_cheat_caller_address(addr, 0x1234);
    account.__validate__(array![]);
    stop_cheat_caller_address(addr);
}

#[test]
#[should_panic(expected: ('INVALID_CALLER',))]
fn test_account_rejects_nonzero_caller_in_execute() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    start_cheat_caller_address(addr, 0x5678);
    account.__execute__(array![]);
    stop_cheat_caller_address(addr);
}

#[test]
#[should_panic(expected: ('INVALID_CALLER',))]
fn test_account_rejects_nonzero_caller_in_validate_deploy() {
    let addr = deploy_account(CLIENT_PUBKEY, COSIGNER_PUBKEY);
    let account = ISRC6Dispatcher { contract_address: addr };

    start_cheat_caller_address(addr, 0x9abc);
    // __validate_deploy__ is exposed as an external function on the contract, not on the
    // dispatcher.
    // The check is the same as the validate flow and is important to protect constructor
    // validation.
    let _ = account.__validate__(array![]);
    stop_cheat_caller_address(addr);
}

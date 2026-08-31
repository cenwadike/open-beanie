use snforge_std::{
    ContractClassTrait, DeclareResultTrait, EventSpyAssertionsTrait, declare, spy_events,
    start_cheat_caller_address, stop_cheat_caller_address,
};
use starknet::ContractAddress;
use starknet_beanie::stealth_registry::{
    IStealthRegistryDispatcher, IStealthRegistryDispatcherTrait,
};

fn deploy() -> IStealthRegistryDispatcher {
    let contract = declare("StealthRegistry").unwrap().contract_class();
    let (address, _) = contract.deploy(@array![]).unwrap();
    IStealthRegistryDispatcher { contract_address: address }
}

fn merchant() -> ContractAddress {
    'merchant'.try_into().unwrap()
}

fn payer() -> ContractAddress {
    'payer'.try_into().unwrap()
}

fn stealth_addr() -> ContractAddress {
    'stealth_addr'.try_into().unwrap()
}

#[test]
fn test_register_and_read_meta_address() {
    let registry = deploy();
    start_cheat_caller_address(registry.contract_address, merchant());

    registry.register_meta_address(111, 222);

    let (spend, view) = registry.get_meta_address(merchant());
    assert(spend == 111, 'wrong spending pubkey');
    assert(view == 222, 'wrong viewing pubkey');

    stop_cheat_caller_address(registry.contract_address);
}

#[test]
fn test_register_overwrites_previous() {
    let registry = deploy();
    start_cheat_caller_address(registry.contract_address, merchant());

    registry.register_meta_address(111, 222);
    registry.register_meta_address(333, 444); // key rotation

    let (spend, view) = registry.get_meta_address(merchant());
    assert(spend == 333, 'rotation: spend not updated');
    assert(view == 444, 'rotation: view not updated');

    stop_cheat_caller_address(registry.contract_address);
}

#[test]
#[should_panic(expected: ('ZERO_PUBKEY',))]
fn test_register_rejects_zero_spending_key() {
    let registry = deploy();
    start_cheat_caller_address(registry.contract_address, merchant());
    registry.register_meta_address(0, 222);
}

#[test]
#[should_panic(expected: ('ZERO_PUBKEY',))]
fn test_register_rejects_zero_viewing_key() {
    let registry = deploy();
    start_cheat_caller_address(registry.contract_address, merchant());
    registry.register_meta_address(111, 0);
}

#[test]
fn test_get_meta_address_unregistered_returns_zeros() {
    let registry = deploy();
    let (spend, view) = registry.get_meta_address(merchant());
    assert(spend == 0, 'expected zero spend key');
    assert(view == 0, 'expected zero view key');
}

#[test]
fn test_announce_emits_event() {
    let registry = deploy();
    let mut spy = spy_events();

    start_cheat_caller_address(registry.contract_address, payer());
    registry.announce(stealth_addr(), 555, 'a1');
    stop_cheat_caller_address(registry.contract_address);

    spy
        .assert_emitted(
            @array![
                (
                    registry.contract_address,
                    starknet_beanie::stealth_registry::StealthRegistry::Event::Announcement(
                        starknet_beanie::stealth_registry::StealthRegistry::Announcement {
                            stealth_address: stealth_addr(), ephemeral_pubkey: 555, view_tag: 'a1',
                        },
                    ),
                ),
            ],
        );
}

#[test]
fn test_announce_is_permissionless_and_free_of_side_effects() {
    // Anyone can announce, for any stealth address, any number of times —
    // it's just an event. No state changes beyond the log, no funds move.
    let registry = deploy();
    start_cheat_caller_address(registry.contract_address, payer());

    registry.announce(stealth_addr(), 1, 'x');
    registry.announce(stealth_addr(), 2, 'y');
    registry.announce(stealth_addr(), 3, 'z');

    stop_cheat_caller_address(registry.contract_address);
    // No assertion needed beyond "did not panic" — multiple announcements
// to the same address from the same caller are valid (e.g. retries).
}

#[test]
fn test_independent_merchants_do_not_collide() {
    let registry = deploy();
    let merchant_b: ContractAddress = 'merchant_b'.try_into().unwrap();

    start_cheat_caller_address(registry.contract_address, merchant());
    registry.register_meta_address(111, 222);
    stop_cheat_caller_address(registry.contract_address);

    start_cheat_caller_address(registry.contract_address, merchant_b);
    registry.register_meta_address(999, 888);
    stop_cheat_caller_address(registry.contract_address);

    let (spend_a, view_a) = registry.get_meta_address(merchant());
    let (spend_b, view_b) = registry.get_meta_address(merchant_b);

    assert(spend_a == 111 && view_a == 222, 'merchant A corrupted');
    assert(spend_b == 999 && view_b == 888, 'merchant B corrupted');
}

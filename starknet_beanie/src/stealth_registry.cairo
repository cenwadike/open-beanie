// StealthRegistry — minimal ERC-5564-style stealth address registry,
// adapted for the STARK curve.
//
// This contract holds no funds and has no privileged caller. It does
// exactly two things:
//   1. Lets an owner publish a public "meta-address" (spending pubkey +
//      viewing pubkey) once, and update it later if they rotate keys.
//   2. Lets a sender emit an announcement so a recipient's scanner can
//      discover a payment sent to a one-time stealth address.
//
// Funds move via ordinary ERC20 transfers to the derived stealth address —
// this contract never touches token balances. Beanie's `sweep()` is
// unaffected: a stealth address is just an address, so `merchant` can be
// set to one without any change to StarknetReceiver or MerchantFactory.
//
// Security note: this is a minimal prototype. The STARK-curve point
// arithmetic (add/scalar-mul) used off-chain to derive stealth addresses
// should be reviewed against a maintained ERC-5564-on-STARK reference
// (e.g. the StealthPay hackathon project) before any real funds are
// pointed at addresses derived this way. This contract's job is limited
// to storage + event emission; it does not verify stealth-address
// correctness on-chain.

use starknet::ContractAddress;

#[starknet::interface]
pub trait IStealthRegistry<T> {
    /// Publish or update your own meta-address. Only callable by the
    /// address it's being set for — no admin, no delegate.
    fn register_meta_address(ref self: T, spending_pubkey: felt252, viewing_pubkey: felt252);

    fn get_meta_address(self: @T, owner: ContractAddress) -> (felt252, felt252);

    /// Anyone can announce a payment to any stealth address — this is
    /// how a payer tells the recipient's scanner "check this one."
    /// Announcing is free of side effects beyond the event; it does not
    /// move funds.
    fn announce(
        ref self: T, stealth_address: ContractAddress, ephemeral_pubkey: felt252, view_tag: felt252,
    );
}

#[starknet::contract]
pub mod StealthRegistry {
    use starknet::storage::{
        Map, StoragePathEntry, StoragePointerReadAccess, StoragePointerWriteAccess,
    };
    use starknet::{ContractAddress, get_caller_address};
    use super::IStealthRegistry;

    #[storage]
    struct Storage {
        spending_pubkeys: Map<ContractAddress, felt252>,
        viewing_pubkeys: Map<ContractAddress, felt252>,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        MetaAddressRegistered: MetaAddressRegistered,
        Announcement: Announcement,
    }

    #[derive(Drop, starknet::Event)]
    pub struct MetaAddressRegistered {
        #[key]
        pub owner: ContractAddress,
        pub spending_pubkey: felt252,
        pub viewing_pubkey: felt252,
    }

    #[derive(Drop, starknet::Event)]
    pub struct Announcement {
        #[key]
        pub stealth_address: ContractAddress,
        pub ephemeral_pubkey: felt252,
        pub view_tag: felt252,
    }

    pub mod Errors {
        pub const ZERO_PUBKEY: felt252 = 'ZERO_PUBKEY';
    }

    #[abi(embed_v0)]
    pub impl StealthRegistryImpl of IStealthRegistry<ContractState> {
        fn register_meta_address(
            ref self: ContractState, spending_pubkey: felt252, viewing_pubkey: felt252,
        ) {
            assert(spending_pubkey != 0, Errors::ZERO_PUBKEY);
            assert(viewing_pubkey != 0, Errors::ZERO_PUBKEY);

            let caller = get_caller_address();
            self.spending_pubkeys.entry(caller).write(spending_pubkey);
            self.viewing_pubkeys.entry(caller).write(viewing_pubkey);

            self
                .emit(
                    Event::MetaAddressRegistered(
                        MetaAddressRegistered { owner: caller, spending_pubkey, viewing_pubkey },
                    ),
                );
        }

        fn get_meta_address(self: @ContractState, owner: ContractAddress) -> (felt252, felt252) {
            (self.spending_pubkeys.entry(owner).read(), self.viewing_pubkeys.entry(owner).read())
        }

        fn announce(
            ref self: ContractState,
            stealth_address: ContractAddress,
            ephemeral_pubkey: felt252,
            view_tag: felt252,
        ) {
            self
                .emit(
                    Event::Announcement(
                        Announcement { stealth_address, ephemeral_pubkey, view_tag },
                    ),
                );
        }
    }
}

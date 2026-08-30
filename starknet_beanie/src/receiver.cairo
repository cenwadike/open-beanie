// StarknetReceiver — Cairo port of ChainXReceiver.sol.
//
// No pool coupling. No privacy_invoke. No `caller == privacy_contract` gate.
// This contract doesn't care whether the USDC that lands here arrived via a
// plain public transfer or via a customer unshielding a private pool note
// straight to this address — both look identical once they hit balance_of(),
// same statelessness ChainXReceiver.sol already relies on on the EVM leg.
// Privacy, if the payer wants it, already happened one level up in the
// customer's own wallet, before this contract ever saw the funds.
//
// Same security model as the EVM leg: every destination pinned immutable at
// initialize(), sweep() is permissionless/idempotent/atomic.

use starknet::ContractAddress;

/// CCTP `TokenMessengerMinterV2` burn entrypoint. Identical shape to the one
/// already written in bridge_out.cairo — mirrors circlefin/starknet-cctp.
#[starknet::interface]
pub trait ITokenMessengerMinterV2<T> {
    fn deposit_for_burn(
        ref self: T,
        amount: u256,
        destination_domain: u32,
        mint_recipient: u256,
        burn_token: ContractAddress,
        destination_caller: u256,
        max_fee: u256,
        min_finality_threshold: u32,
    );
}

#[starknet::interface]
pub trait IStarknetReceiver<T> {
    fn initialize(
        ref self: T,
        token: ContractAddress,
        treasury: ContractAddress,
        token_messenger: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256, // 0 = same-chain settlement to `merchant`
        merchant: ContractAddress,
    );
    /// Returns (net, fee_to_caller, fee_to_treasury, fee) — same tuple shape
    /// as ChainXReceiver.sol's sweep().
    fn sweep(ref self: T) -> (u256, u256, u256, u256);
    fn get_token(self: @T) -> ContractAddress;
    fn get_merchant(self: @T) -> ContractAddress;
    fn get_mint_recipient(self: @T) -> u256;
}

#[starknet::contract]
pub mod StarknetReceiver {
    use core::num::traits::Zero;
    use openzeppelin::interfaces::token::erc20::{IERC20Dispatcher, IERC20DispatcherTrait};
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::{ContractAddress, get_caller_address, get_contract_address};
    use super::{
        IStarknetReceiver, ITokenMessengerMinterV2Dispatcher,
        ITokenMessengerMinterV2DispatcherTrait,
    };

    const FEE_BPS: u256 = 50; // 0.50% of gross — matches ChainXReceiver.sol
    const BPS_DENOM: u256 = 10_000;
    const CALLER_SHARE_BPS: u256 = 1_000; // 10% of the fee, not of gross

    // 15bps, not the EVM leg's 2bps — kept from your original bridge_out.cairo:
    // Starknet CCTP FAST reference finality runs ~12bps, this is that + a 3bps
    // buffer. Chain-specific, not a copy-paste of the EVM constant.
    const CCTP_MAX_FEE_BPS: u256 = 15;

    #[storage]
    struct Storage {
        initialized: bool,
        token: ContractAddress,
        treasury: ContractAddress,
        token_messenger: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256, // 0 = same-chain settlement
        merchant: ContractAddress,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        Initialized: Initialized,
        Swept: Swept,
    }

    #[derive(Drop, starknet::Event)]
    pub struct Initialized {
        pub token: ContractAddress,
        pub treasury: ContractAddress,
        pub destination_domain: u32,
        pub mint_recipient: u256,
    }

    #[derive(Drop, starknet::Event)]
    pub struct Swept {
        pub gross: u256,
        pub net: u256,
        pub fee: u256,
        pub fee_to_caller: u256,
        pub fee_to_treasury: u256,
    }

    pub mod Errors {
        pub const ALREADY_INITIALIZED: felt252 = 'ALREADY_INITIALIZED';
        pub const NOT_INITIALIZED: felt252 = 'NOT_INITIALIZED';
        pub const ZERO_ADDRESS: felt252 = 'ZERO_ADDRESS';
        pub const TRANSFER_FAILED: felt252 = 'TRANSFER_FAILED';
    }

    #[constructor]
    fn constructor(ref self: ContractState) {}

    #[abi(embed_v0)]
    pub impl StarknetReceiverImpl of IStarknetReceiver<ContractState> {
        /// Run once after deploy. Pins every destination permanently, same
        /// one-shot guard as ChainXReceiver.sol's initialize().
        fn initialize(
            ref self: ContractState,
            token: ContractAddress,
            treasury: ContractAddress,
            token_messenger: ContractAddress,
            destination_domain: u32,
            mint_recipient: u256,
            merchant: ContractAddress,
        ) {
            assert(!self.initialized.read(), Errors::ALREADY_INITIALIZED);
            assert(
                token.is_non_zero()
                    && treasury.is_non_zero()
                    && token_messenger.is_non_zero()
                    && merchant.is_non_zero(),
                Errors::ZERO_ADDRESS,
            );

            self.initialized.write(true);
            self.token.write(token);
            self.treasury.write(treasury);
            self.token_messenger.write(token_messenger);
            self.destination_domain.write(destination_domain);
            self.mint_recipient.write(mint_recipient);
            self.merchant.write(merchant);

            self
                .emit(
                    Event::Initialized(
                        Initialized { token, treasury, destination_domain, mint_recipient },
                    ),
                );
        }

        /// PERMISSIONLESS, IDEMPOTENT, atomic. Zero balance -> silent no-op —
        /// identical contract to ChainXReceiver.sol's sweep(). Doesn't care
        /// whether the balance arrived via a plain transfer or a customer
        /// unshielding a private pool note straight to this address.
        fn sweep(ref self: ContractState) -> (u256, u256, u256, u256) {
            assert(self.initialized.read(), Errors::NOT_INITIALIZED);

            let token = self.token.read();
            let erc20 = IERC20Dispatcher { contract_address: token };
            let gross: u256 = erc20.balance_of(get_contract_address());
            if gross == 0 {
                return (0, 0, 0, 0); // idempotent, same as the EVM leg
            }

            let fee = (gross * FEE_BPS) / BPS_DENOM;
            let net = gross - fee;
            let fee_to_caller = (fee * CALLER_SHARE_BPS) / BPS_DENOM;
            let fee_to_treasury = fee - fee_to_caller;

            if fee_to_caller > 0 {
                assert(
                    erc20.transfer(get_caller_address(), fee_to_caller), Errors::TRANSFER_FAILED,
                );
            }
            if fee_to_treasury > 0 {
                assert(
                    erc20.transfer(self.treasury.read(), fee_to_treasury), Errors::TRANSFER_FAILED,
                );
            }

            let mint_recipient = self.mint_recipient.read();
            if net > 0 {
                if mint_recipient != 0 {
                    // Cross-chain: burn via CCTP toward the merchant's chosen
                    // destination (e.g. domain 6 = Base).
                    let max_fee = (gross * CCTP_MAX_FEE_BPS) / BPS_DENOM;
                    let messenger = self.token_messenger.read();
                    erc20.approve(messenger, net);
                    ITokenMessengerMinterV2Dispatcher { contract_address: messenger }
                        .deposit_for_burn(
                            net,
                            self.destination_domain.read(),
                            mint_recipient,
                            token,
                            0, // destination_caller: 0 = permissionless mint
                            max_fee,
                            1000 // standard finality threshold
                        );
                } else {
                    // Same-chain settlement direct to the merchant.
                    assert(erc20.transfer(self.merchant.read(), net), Errors::TRANSFER_FAILED);
                }
            }

            self.emit(Event::Swept(Swept { gross, net, fee, fee_to_caller, fee_to_treasury }));
            (net, fee_to_caller, fee_to_treasury, fee)
        }

        fn get_token(self: @ContractState) -> ContractAddress {
            self.token.read()
        }
        fn get_merchant(self: @ContractState) -> ContractAddress {
            self.merchant.read()
        }
        fn get_mint_recipient(self: @ContractState) -> u256 {
            self.mint_recipient.read()
        }
    }
}

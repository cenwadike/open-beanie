// BridgeOutAnonymizer — Deployed PER MERCHANT.
//
// Cross-chain egress parameters (destination_domain, mint_recipient) are
// permanently pinned in contract storage at deployment. Runtime arguments passed
// to privacy_invoke are overridden by storage state to guarantee single-tenant
// parameter isolation.
//
// Access control restricts execution strictly to the privacy pool contract.

use privacy::objects::OpenNoteDeposit;
use starknet::ContractAddress;

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
pub trait IBridgeOutAnonymizer<T> {
    fn privacy_invoke(ref self: T) -> Span<OpenNoteDeposit>;
}

#[starknet::contract]
pub mod BridgeOutAnonymizer {
    use openzeppelin::interfaces::token::erc20::{IERC20Dispatcher, IERC20DispatcherTrait};
    use privacy::objects::OpenNoteDeposit;
    use starknet::storage::{StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::{ContractAddress, get_caller_address, get_contract_address};
    use super::{
        IBridgeOutAnonymizer, ITokenMessengerMinterV2Dispatcher,
        ITokenMessengerMinterV2DispatcherTrait,
    };

    #[storage]
    struct Storage {
        cctp_messenger: ContractAddress,
        privacy_contract: ContractAddress,
        token: ContractAddress, // Pinned per merchant
        destination_domain: u32, // Pinned per merchant
        mint_recipient: u256 // Pinned per merchant
    }

    pub mod Errors {
        pub const CALLER_NOT_PRIVACY: felt252 = 'CALLER_NOT_PRIVACY';
    }

    #[constructor]
    fn constructor(
        ref self: ContractState,
        cctp_messenger: ContractAddress,
        privacy_contract: ContractAddress,
        token: ContractAddress,
        destination_domain: u32,
        mint_recipient: u256,
    ) {
        self.cctp_messenger.write(cctp_messenger);
        self.privacy_contract.write(privacy_contract);
        self.token.write(token);
        self.destination_domain.write(destination_domain);
        self.mint_recipient.write(mint_recipient);
    }

    #[abi(embed_v0)]
    pub impl BridgeOutAnonymizerImpl of IBridgeOutAnonymizer<ContractState> {
        fn privacy_invoke(ref self: ContractState) -> Span<OpenNoteDeposit> {
            assert(
                get_caller_address() == self.privacy_contract.read(), Errors::CALLER_NOT_PRIVACY,
            );

            let token = self.token.read();
            let erc20 = IERC20Dispatcher { contract_address: token };
            let amount_u256: u256 = erc20.balance_of(get_contract_address());

            if amount_u256 == 0 {
                return [].span();
            }

            // Derive the 15 bps FAST CCTP teleport (15 / 10,000 = 0.0015 or 0.15%)
            // 0.03% buffer added over reference Starknet CCTP FAST finality 12bps
            let max_fee: u256 = (amount_u256 * 15) / 10_000;

            let messenger = self.cctp_messenger.read();
            erc20.approve(messenger, amount_u256);

            // Execute Fast Transfer with safety guard
            ITokenMessengerMinterV2Dispatcher { contract_address: messenger }
                .deposit_for_burn(
                    amount_u256,
                    self.destination_domain.read(),
                    self.mint_recipient.read(),
                    token,
                    0_u256,
                    max_fee,
                    1000_u32 // Fast Transfer Threshold
                );

            [].span()
        }
    }
}

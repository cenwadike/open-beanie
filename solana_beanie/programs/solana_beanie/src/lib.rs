/*
//! # Deposit Receiver Factory (receiver-keyed, pre-signed registration)
//!
//! The `receiver` is the identity: the on-curve address handed to exchanges and
//! wallets. Everything else derives from it:
//!   receiver_token_account = ATA(receiver, mint)
//!   receiver_config        = PDA [config,  receiver]   (exists  <=>  registered)
//!   pending_registration   = PDA [pending, receiver]   (pinned blob holder)
//!   staging account        = ATA(receiver_config, mint)
//!
//! Lifecycle
//!   1. announce_merchant
//!        - stores the fully signed registration tx in `pending_registration`
//!        - emits every derived address
//!        - creates NO config and touches NO shared state
//!   2. register_merchant  (the stored tx, broadcast by anyone, once)
//!        - `init`s receiver_config  -> a second registration cannot succeed
//!        - approve(config, u64::MAX) + CloseAccount authority -> config
//!        - appends to MerchantRegistry
//!        - closes pending_registration (rent -> payer)
//!   3. sweep              (permissionless; needs the config to exist)
//!
//! If `[config, receiver]` exists with data, the receiver is registered.
*/

#![allow(unexpected_cfgs)]
#![allow(deprecated)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::{self, Approve, Mint, Token, TokenAccount, Transfer};
use std::str::FromStr;

declare_id!("BsiBBPjkAiLJgQDNmjfajHeJjpFMzJqCNz2FtzZa2Hpg");

// ── Constants ─────────────────────────────────────────────────────────────
pub const FEE_BPS: u64 = 50; // 0.50%
pub const BPS_DENOM: u64 = 10_000;
pub const CALLER_SHARE_BPS: u64 = 1_000; // 10% of the fee
pub const MAX_RECEIVERS_PER_MERCHANT: usize = 32;

// CCTP V2 Fast Transfer ceiling for Solana as source chain: Circle's published
// 1 bps plus a 2 bps buffer. Re-check https://developers.circle.com/cctp before relying on it.
pub const CCTP_MAX_FEE_BPS: u64 = 3;
pub const CCTP_MIN_FINALITY_THRESHOLD: u32 = 1000;

/// Sanity cap on the stored pre-signed tx. The real limit is the 1232-byte
/// packet that has to carry it inside the announce tx.
pub const MAX_REG_TX_BYTES: usize = 1024;
/// bump u8 + Vec length prefix u32 (excluding the 8-byte discriminator and the blob).
pub const PENDING_FIXED_LEN: usize = 1 + 4;

pub const FACTORY_SEED: &[u8] = b"factory";
pub const CONFIG_SEED: &[u8] = b"config";
pub const PENDING_SEED: &[u8] = b"pending";
pub const REGISTRY_SEED: &[u8] = b"registry";

pub const CCTP_TOKEN_MESSENGER_MINTER_V2: &str = "CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe";
pub const CCTP_MESSAGE_TRANSMITTER_V2: &str = "CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC";

pub const SAME_CHAIN_DOMAIN_SENTINEL: u32 = u32::MAX;

fn cctp_token_messenger_minter_id() -> Pubkey {
    Pubkey::from_str(CCTP_TOKEN_MESSENGER_MINTER_V2).expect("valid pubkey literal")
}
fn cctp_message_transmitter_id() -> Pubkey {
    Pubkey::from_str(CCTP_MESSAGE_TRANSMITTER_V2).expect("valid pubkey literal")
}

fn anchor_discriminator(instruction_name: &str) -> [u8; 8] {
    let hash =
        anchor_lang::solana_program::hash::hash(format!("global:{instruction_name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.to_bytes()[..8]);
    out
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct CctpDepositForBurnParams {
    pub amount: u64,
    pub destination_domain: u32,
    pub mint_recipient: Pubkey,
    pub destination_caller: Pubkey,
    pub max_fee: u64,
    pub min_finality_threshold: u32,
}

// ── Program ──────────────────────────────────────────────────────────────
#[program]
pub mod sol {
    use super::*;

    // ── Initialize factory (run once) ──────────────────────────────────────
    #[inline(never)]
    pub fn initialize_factory(
        ctx: Context<InitializeFactory>,
        starknet_domain: u32,
        base_domain: u32,
        ethereum_domain: u32,
        monad_domain: u32,
        arbitrum_domain: u32,
    ) -> Result<()> {
        let domains = [
            starknet_domain,
            base_domain,
            ethereum_domain,
            monad_domain,
            arbitrum_domain,
        ];
        let all_distinct = domains
            .iter()
            .enumerate()
            .all(|(i, a)| domains.iter().skip(i + 1).all(|b| a != b));
        require!(all_distinct, DepositError::DuplicateDomains);

        let cfg = &mut ctx.accounts.factory_config;
        cfg.mint = ctx.accounts.mint.key();
        cfg.treasury_token_account = ctx.accounts.treasury_token_account.key();
        cfg.starknet_domain = starknet_domain;
        cfg.base_domain = base_domain;
        cfg.ethereum_domain = ethereum_domain;
        cfg.monad_domain = monad_domain;
        cfg.arbitrum_domain = arbitrum_domain;
        cfg.bump = ctx.bumps.factory_config;

        emit!(FactoryInitialized {
            mint: cfg.mint,
            treasury_token_account: cfg.treasury_token_account,
            starknet_domain,
            base_domain,
            ethereum_domain,
            monad_domain,
            arbitrum_domain,
        });
        Ok(())
    }

    // ── Announce ───────────────────────────────────────────────────────────
    /// Stores the fully signed registration tx in a small PDA pinned to
    /// `receiver` and emits the derived addresses. No receiver signature: the
    /// client verifies the stored blob and the derivations before disclosing
    /// the address, and a tampered or squatted announce just means the address
    /// is discarded unseen. Creates no config and touches no shared state.
    #[inline(never)]
    pub fn announce_merchant(
        ctx: Context<AnnounceMerchant>,
        merchant: Pubkey,
        cctp_mint_chain: [u8; 32],
        cctp_mint_recipient: [u8; 32],
        receiver: Pubkey,
        reg_tx: Vec<u8>,
    ) -> Result<()> {
        require!(!reg_tx.is_empty(), DepositError::EmptyRegTx);
        require!(
            reg_tx.len() <= MAX_REG_TX_BYTES,
            DepositError::RegTxTooLarge
        );

        // Fail fast on a bad route; register_merchant validates the same rules.
        validate_route(
            &ctx.accounts.factory_config,
            &cctp_mint_chain,
            &cctp_mint_recipient,
        )?;

        let mint = ctx.accounts.factory_config.mint;
        let receiver_token_account = get_associated_token_address(&receiver, &mint);
        let (receiver_config, _) =
            Pubkey::find_program_address(&[CONFIG_SEED, receiver.as_ref()], &crate::ID);
        let cctp_burn_staging_account = get_associated_token_address(&receiver_config, &mint);
        let pending_registration = ctx.accounts.pending_registration.key();

        let pending = &mut ctx.accounts.pending_registration;
        pending.bump = ctx.bumps.pending_registration;
        pending.reg_tx = reg_tx;

        emit!(MerchantAnnounced {
            merchant,
            receiver,
            receiver_token_account,
            receiver_config,
            cctp_burn_staging_account,
            pending_registration,
            cctp_mint_chain,
            cctp_mint_recipient,
        });
        Ok(())
    }

    // ── Register (broadcast from the stored, pre-signed tx) ────────────────
    /// `receiver` and `payer` signed this off-chain. `init` on
    /// `[config, receiver]` makes it succeed exactly once. Delegates spending
    /// to `receiver_config`, hands it CloseAccount authority, never reassigns
    /// AccountOwner, appends to the registry, closes the pending blob.
    #[inline(never)]
    pub fn register_merchant(
        ctx: Context<RegisterMerchant>,
        merchant: Pubkey,
        cctp_mint_chain: [u8; 32],
        cctp_mint_recipient: [u8; 32],
    ) -> Result<()> {
        require!(
            (ctx.accounts.merchant_registry.receiver_count as usize) < MAX_RECEIVERS_PER_MERCHANT,
            DepositError::MaxReceiversExceeded
        );

        let cctp_domain_id = validate_route(
            &ctx.accounts.factory_config,
            &cctp_mint_chain,
            &cctp_mint_recipient,
        )?;

        token::approve(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Approve {
                    to: ctx.accounts.receiver_token_account.to_account_info(),
                    delegate: ctx.accounts.receiver_config.to_account_info(),
                    authority: ctx.accounts.receiver.to_account_info(),
                },
            ),
            u64::MAX,
        )?;

        token::set_authority(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::SetAuthority {
                    account_or_mint: ctx.accounts.receiver_token_account.to_account_info(),
                    current_authority: ctx.accounts.receiver.to_account_info(),
                },
            ),
            anchor_spl::token::spl_token::instruction::AuthorityType::CloseAccount,
            Some(ctx.accounts.receiver_config.key()),
        )?;

        let receiver = ctx.accounts.receiver.key();
        let receiver_config_key = ctx.accounts.receiver_config.key();
        let receiver_token_account = ctx.accounts.receiver_token_account.key();
        let merchant_token_account = ctx.accounts.merchant_token_account.key();
        let mint = ctx.accounts.factory_config.mint;

        let cfg = &mut ctx.accounts.receiver_config;
        cfg.receiver = receiver;
        cfg.merchant = merchant;
        cfg.mint = mint;
        cfg.merchant_token_account = merchant_token_account;
        cfg.cctp_mint_chain = cctp_mint_chain;
        cfg.cctp_mint_recipient = cctp_mint_recipient;
        cfg.cctp_domain_id = cctp_domain_id;
        cfg.bump = ctx.bumps.receiver_config;

        let registry = &mut ctx.accounts.merchant_registry;
        if registry.merchant == Pubkey::default() {
            registry.merchant = merchant;
        }
        let idx = registry.receiver_count as usize;
        registry.receivers[idx] = receiver_config_key;
        registry.receiver_count += 1;

        emit!(MerchantRegistered {
            merchant,
            receiver,
            receiver_config: receiver_config_key,
            receiver_token_account,
            cctp_mint_chain,
            cctp_mint_recipient,
        });
        Ok(())
    }

    // ── Sweep ──────────────────────────────────────────────────────────────
    #[inline(never)]
    pub fn sweep(ctx: Context<Sweep>) -> Result<()> {
        let cross_chain = ctx.accounts.receiver_config.cctp_mint_chain != [0u8; 32];

        ctx.accounts.receiver_token_account.reload()?;
        let balance = ctx.accounts.receiver_token_account.amount;
        if balance == 0 {
            msg!("sweep: balance is zero, nothing to do");
            return Ok(());
        }

        let fee = balance
            .checked_mul(FEE_BPS)
            .ok_or(DepositError::ArithmeticOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(DepositError::ArithmeticOverflow)?;
        let net = balance
            .checked_sub(fee)
            .ok_or(DepositError::ArithmeticOverflow)?;

        let fee_to_caller = fee
            .checked_mul(CALLER_SHARE_BPS)
            .ok_or(DepositError::ArithmeticOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(DepositError::ArithmeticOverflow)?;
        let fee_to_treasury = fee
            .checked_sub(fee_to_caller)
            .ok_or(DepositError::ArithmeticOverflow)?;

        let receiver = ctx.accounts.receiver_config.receiver;
        let recipient = ctx.accounts.receiver_config.cctp_mint_recipient;
        let bump = ctx.accounts.receiver_config.bump;
        let signer_seeds: &[&[u8]] = &[CONFIG_SEED, receiver.as_ref(), &[bump]];
        let signer = &[signer_seeds];

        if fee_to_caller > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: ctx.accounts.caller_token_account.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                fee_to_caller,
            )?;
        }
        if fee_to_treasury > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: ctx.accounts.treasury_token_account.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                fee_to_treasury,
            )?;
        }

        if !cross_chain {
            let merchant_token_account = ctx
                .accounts
                .merchant_token_account
                .as_ref()
                .ok_or(DepositError::MissingMerchantAccount)?;
            require_keys_eq!(
                merchant_token_account.key(),
                ctx.accounts.receiver_config.merchant_token_account,
                DepositError::WrongMerchant
            );

            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: merchant_token_account.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                net,
            )?;

            emit!(Swept {
                receiver_config: ctx.accounts.receiver_config.key(),
                receiver,
                gross_amount: balance,
                net_amount: net,
                fee_amount: fee,
                fee_to_caller,
                fee_to_treasury,
                slot: Clock::get()?.slot,
            });
            return Ok(());
        }

        // ── cross-chain ────────────────────────────────────────────────────
        let cctp_burn_staging_account = ctx
            .accounts
            .cctp_burn_staging_account
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let event_rent_payer = ctx
            .accounts
            .event_rent_payer
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let message_sent_event_data = ctx
            .accounts
            .message_sent_event_data
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let burn_token_mint = ctx
            .accounts
            .burn_token_mint
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_sender_authority_pda = ctx
            .accounts
            .cctp_sender_authority_pda
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_denylist_account = ctx
            .accounts
            .cctp_denylist_account
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_message_transmitter = ctx
            .accounts
            .cctp_message_transmitter
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_token_messenger = ctx
            .accounts
            .cctp_token_messenger
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_remote_token_messenger = ctx
            .accounts
            .cctp_remote_token_messenger
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_token_minter = ctx
            .accounts
            .cctp_token_minter
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_local_token = ctx
            .accounts
            .cctp_local_token
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_event_authority = ctx
            .accounts
            .cctp_event_authority
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_message_transmitter_program = ctx
            .accounts
            .cctp_message_transmitter_program
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;
        let cctp_token_messenger_minter_program = ctx
            .accounts
            .cctp_token_messenger_minter_program
            .as_ref()
            .ok_or(DepositError::MissingCctpAccounts)?;

        require_keys_eq!(
            cctp_token_messenger_minter_program.key(),
            cctp_token_messenger_minter_id(),
            DepositError::WrongCctpProgram
        );
        require_keys_eq!(
            cctp_message_transmitter_program.key(),
            cctp_message_transmitter_id(),
            DepositError::WrongCctpProgram
        );
        let expected_staging = get_associated_token_address(
            &ctx.accounts.receiver_config.key(),
            &ctx.accounts.factory_config.mint,
        );
        require_keys_eq!(
            cctp_burn_staging_account.key(),
            expected_staging,
            DepositError::WrongStagingAccount
        );

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.receiver_token_account.to_account_info(),
                    to: cctp_burn_staging_account.to_account_info(),
                    authority: ctx.accounts.receiver_config.to_account_info(),
                },
                signer,
            ),
            net,
        )?;

        let max_fee = balance
            .checked_mul(CCTP_MAX_FEE_BPS)
            .ok_or(DepositError::ArithmeticOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(DepositError::ArithmeticOverflow)?;

        let params = CctpDepositForBurnParams {
            amount: net,
            destination_domain: ctx.accounts.receiver_config.cctp_domain_id,
            mint_recipient: Pubkey::new_from_array(recipient),
            destination_caller: Pubkey::default(),
            max_fee,
            min_finality_threshold: CCTP_MIN_FINALITY_THRESHOLD,
        };

        let mut data = anchor_discriminator("deposit_for_burn").to_vec();
        data.extend_from_slice(
            &params
                .try_to_vec()
                .map_err(|_| error!(DepositError::ArithmeticOverflow))?,
        );

        let cctp_accounts = vec![
            AccountMeta::new_readonly(ctx.accounts.receiver_config.key(), true), // owner
            AccountMeta::new(event_rent_payer.key(), true),
            AccountMeta::new_readonly(cctp_sender_authority_pda.key(), false),
            AccountMeta::new(cctp_burn_staging_account.key(), false),
            AccountMeta::new_readonly(cctp_denylist_account.key(), false),
            AccountMeta::new(cctp_message_transmitter.key(), false),
            AccountMeta::new_readonly(cctp_token_messenger.key(), false),
            AccountMeta::new_readonly(cctp_remote_token_messenger.key(), false),
            AccountMeta::new_readonly(cctp_token_minter.key(), false),
            AccountMeta::new(cctp_local_token.key(), false),
            AccountMeta::new(burn_token_mint.key(), false),
            AccountMeta::new(message_sent_event_data.key(), true),
            AccountMeta::new_readonly(cctp_message_transmitter_program.key(), false),
            AccountMeta::new_readonly(cctp_token_messenger_minter_program.key(), false),
            AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
            AccountMeta::new_readonly(ctx.accounts.system_program.key(), false),
            AccountMeta::new_readonly(cctp_event_authority.key(), false),
            AccountMeta::new_readonly(cctp_token_messenger_minter_program.key(), false),
        ];

        let ix = Instruction {
            program_id: cctp_token_messenger_minter_program.key(),
            accounts: cctp_accounts,
            data,
        };

        invoke_signed(
            &ix,
            &[
                ctx.accounts.receiver_config.to_account_info(),
                event_rent_payer.to_account_info(),
                cctp_sender_authority_pda.to_account_info(),
                cctp_burn_staging_account.to_account_info(),
                cctp_denylist_account.to_account_info(),
                cctp_message_transmitter.to_account_info(),
                cctp_token_messenger.to_account_info(),
                cctp_remote_token_messenger.to_account_info(),
                cctp_token_minter.to_account_info(),
                cctp_local_token.to_account_info(),
                burn_token_mint.to_account_info(),
                message_sent_event_data.to_account_info(),
                cctp_message_transmitter_program.to_account_info(),
                cctp_token_messenger_minter_program.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                cctp_event_authority.to_account_info(),
                cctp_token_messenger_minter_program.to_account_info(),
            ],
            signer,
        )?;

        emit!(SweptCrossChain {
            receiver_config: ctx.accounts.receiver_config.key(),
            receiver,
            gross_amount: balance,
            net_amount: net,
            fee_amount: fee,
            fee_to_caller,
            fee_to_treasury,
            destination_domain: ctx.accounts.receiver_config.cctp_domain_id,
            max_fee,
        });

        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────
pub fn chain_name_seed(name: &[u8]) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let len = name.len().min(32);
    buf[..len].copy_from_slice(&name[..len]);
    buf
}

fn resolve_domain(factory: &FactoryConfig, chain: &[u8; 32]) -> Option<u32> {
    if *chain == chain_name_seed(b"STARKNET") {
        Some(factory.starknet_domain)
    } else if *chain == chain_name_seed(b"BASE") {
        Some(factory.base_domain)
    } else if *chain == chain_name_seed(b"ETHEREUM") {
        Some(factory.ethereum_domain)
    } else if *chain == chain_name_seed(b"MONAD") {
        Some(factory.monad_domain)
    } else if *chain == chain_name_seed(b"ARBITRUM") {
        Some(factory.arbitrum_domain)
    } else {
        None
    }
}

/// Returns the CCTP domain id (SAME_CHAIN_DOMAIN_SENTINEL for same-chain routes).
fn validate_route(factory: &FactoryConfig, chain: &[u8; 32], recipient: &[u8; 32]) -> Result<u32> {
    if *chain != [0u8; 32] {
        require!(
            *recipient != [0u8; 32],
            DepositError::CrossChainRequiresRecipient
        );
        resolve_domain(factory, chain).ok_or_else(|| error!(DepositError::InvalidDomain))
    } else {
        require!(
            *recipient == [0u8; 32],
            DepositError::SameChainRecipientMustBeZero
        );
        Ok(SAME_CHAIN_DOMAIN_SENTINEL)
    }
}

// ── Account Contexts ────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeFactory<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(constraint = treasury_token_account.mint == mint.key() @ DepositError::WrongMint)]
    pub treasury_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = payer,
        space = 8 + FactoryConfig::INIT_SPACE,
        seeds = [FACTORY_SEED],
        bump,
    )]
    pub factory_config: Account<'info, FactoryConfig>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(merchant: Pubkey, cctp_mint_chain: [u8; 32], cctp_mint_recipient: [u8; 32], receiver: Pubkey, reg_tx: Vec<u8>)]
pub struct AnnounceMerchant<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Account<'info, FactoryConfig>,

    /// Small pinned holder of the signed registration tx, at [pending, receiver].
    #[account(
        init,
        payer = payer,
        space = 8 + PENDING_FIXED_LEN + reg_tx.len(),
        seeds = [PENDING_SEED, receiver.as_ref()],
        bump,
    )]
    pub pending_registration: Account<'info, PendingRegistration>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(merchant: Pubkey, cctp_mint_chain: [u8; 32], cctp_mint_recipient: [u8; 32])]
pub struct RegisterMerchant<'info> {
    /// Fee payer of the pre-signed tx; pays the config/registry rent and
    /// receives the pending account's rent back.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub receiver: Signer<'info>,

    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Box<Account<'info, FactoryConfig>>,

    #[account(
        mut,
        constraint = receiver_token_account.mint == factory_config.mint @ DepositError::WrongMint,
        constraint = receiver_token_account.owner == receiver.key() @ DepositError::WrongReceiver,
        constraint = receiver_token_account.key()
            == get_associated_token_address(&receiver.key(), &factory_config.mint)
            @ DepositError::NonCanonicalReceiverAta,
    )]
    pub receiver_token_account: Box<Account<'info, TokenAccount>>,

    #[account(constraint = merchant_token_account.mint == factory_config.mint @ DepositError::WrongMint)]
    pub merchant_token_account: Box<Account<'info, TokenAccount>>,

    /// `init` here is what makes registration succeed exactly once.
    #[account(
        init,
        payer = payer,
        space = 8 + ReceiverConfig::INIT_SPACE,
        seeds = [CONFIG_SEED, receiver.key().as_ref()],
        bump,
    )]
    pub receiver_config: Account<'info, ReceiverConfig>,

    #[account(
        constraint = cctp_burn_staging_account.mint == factory_config.mint @ DepositError::WrongMint,
        constraint = cctp_burn_staging_account.key()
            == get_associated_token_address(&receiver_config.key(), &factory_config.mint)
            @ DepositError::WrongStagingAccount,
    )]
    pub cctp_burn_staging_account: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + MerchantRegistry::INIT_SPACE,
        seeds = [REGISTRY_SEED, merchant.as_ref()],
        bump,
    )]
    pub merchant_registry: Box<Account<'info, MerchantRegistry>>,

    #[account(
        mut,
        close = payer,
        seeds = [PENDING_SEED, receiver.key().as_ref()],
        bump = pending_registration.bump,
    )]
    pub pending_registration: Account<'info, PendingRegistration>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Sweep<'info> {
    pub caller: Signer<'info>,

    #[account(
        mut,
        constraint = caller_token_account.owner == caller.key() @ DepositError::WrongCaller,
        constraint = caller_token_account.mint == factory_config.mint @ DepositError::WrongMint,
    )]
    pub caller_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = receiver_token_account.key()
            == get_associated_token_address(&receiver_config.receiver, &factory_config.mint)
            @ DepositError::WrongReceiver,
    )]
    pub receiver_token_account: Box<Account<'info, TokenAccount>>,

    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Account<'info, FactoryConfig>,

    #[account(
        mut,
        constraint = treasury_token_account.key() == factory_config.treasury_token_account @ DepositError::WrongTreasury,
    )]
    pub treasury_token_account: Box<Account<'info, TokenAccount>>,

    /// Exists <=> registered. An unregistered receiver has no account here.
    #[account(
        seeds = [CONFIG_SEED, receiver_config.receiver.as_ref()],
        bump = receiver_config.bump,
    )]
    pub receiver_config: Box<Account<'info, ReceiverConfig>>,

    // ── Same-chain only ─────────────────────────────────────────────────
    #[account(mut)]
    pub merchant_token_account: Option<Box<Account<'info, TokenAccount>>>,

    // ── Cross-chain only. Client passes `crate::ID` for "None". ─────────
    #[account(mut)]
    pub cctp_burn_staging_account: Option<Box<Account<'info, TokenAccount>>>,
    #[account(mut)]
    pub event_rent_payer: Option<Signer<'info>>,
    #[account(mut)]
    pub message_sent_event_data: Option<Signer<'info>>,
    #[account(mut)]
    pub burn_token_mint: Option<Box<Account<'info, Mint>>>,
    pub cctp_sender_authority_pda: Option<UncheckedAccount<'info>>,
    pub cctp_denylist_account: Option<UncheckedAccount<'info>>,
    #[account(mut)]
    pub cctp_message_transmitter: Option<UncheckedAccount<'info>>,
    pub cctp_token_messenger: Option<UncheckedAccount<'info>>,
    pub cctp_remote_token_messenger: Option<UncheckedAccount<'info>>,
    pub cctp_token_minter: Option<UncheckedAccount<'info>>,
    #[account(mut)]
    pub cctp_local_token: Option<UncheckedAccount<'info>>,
    pub cctp_event_authority: Option<UncheckedAccount<'info>>,
    pub cctp_message_transmitter_program: Option<UncheckedAccount<'info>>,
    pub cctp_token_messenger_minter_program: Option<UncheckedAccount<'info>>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

// ── State ───────────────────────────────────────────────────────────────────

#[account]
#[derive(InitSpace)]
pub struct FactoryConfig {
    pub mint: Pubkey,
    pub treasury_token_account: Pubkey,
    pub starknet_domain: u32,
    pub base_domain: u32,
    pub ethereum_domain: u32,
    pub monad_domain: u32,
    pub arbitrum_domain: u32,
    pub bump: u8,
}

/// PDA `[config, receiver]`. Created only by a successful registration, so
/// its existence is the proof. `receiver_token_account` and the staging
/// account are derivable from `receiver` + `mint`, so they are not stored.
#[account]
#[derive(InitSpace)]
pub struct ReceiverConfig {
    pub receiver: Pubkey,
    pub merchant: Pubkey,
    pub mint: Pubkey,
    pub merchant_token_account: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
    pub cctp_domain_id: u32,
    pub bump: u8,
}

/// PDA `[pending, receiver]`. Holds the fully signed registration tx until it
/// is broadcast; closed by register_merchant. Size = 8 + PENDING_FIXED_LEN + reg_tx.len().
#[account]
pub struct PendingRegistration {
    pub bump: u8,
    pub reg_tx: Vec<u8>,
}

/// One per merchant. Appended at registration, in registration order.
#[account]
#[derive(InitSpace)]
pub struct MerchantRegistry {
    pub merchant: Pubkey,
    pub receiver_count: u16,
    pub receivers: [Pubkey; MAX_RECEIVERS_PER_MERCHANT],
}

// ── Events ──────────────────────────────────────────────────────────────────

#[event]
pub struct FactoryInitialized {
    pub mint: Pubkey,
    pub treasury_token_account: Pubkey,
    pub starknet_domain: u32,
    pub base_domain: u32,
    pub ethereum_domain: u32,
    pub monad_domain: u32,
    pub arbitrum_domain: u32,
}

#[event]
pub struct MerchantAnnounced {
    pub merchant: Pubkey,
    pub receiver: Pubkey,
    pub receiver_token_account: Pubkey,
    pub receiver_config: Pubkey,
    pub cctp_burn_staging_account: Pubkey,
    pub pending_registration: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
}

#[event]
pub struct MerchantRegistered {
    pub merchant: Pubkey,
    pub receiver: Pubkey,
    pub receiver_config: Pubkey,
    pub receiver_token_account: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
}

#[event]
pub struct Swept {
    pub receiver_config: Pubkey,
    pub receiver: Pubkey,
    pub gross_amount: u64,
    pub net_amount: u64,
    pub fee_amount: u64,
    pub fee_to_caller: u64,
    pub fee_to_treasury: u64,
    pub slot: u64,
}

#[event]
pub struct SweptCrossChain {
    pub receiver_config: Pubkey,
    pub receiver: Pubkey,
    pub gross_amount: u64,
    pub net_amount: u64,
    pub fee_amount: u64,
    pub fee_to_caller: u64,
    pub fee_to_treasury: u64,
    pub destination_domain: u32,
    pub max_fee: u64,
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[error_code]
pub enum DepositError {
    #[msg("Arithmetic overflow in fee calculation")]
    ArithmeticOverflow,
    #[msg("receiver_token_account does not match config / receiver")]
    WrongReceiver,
    #[msg("merchant_token_account does not match config")]
    WrongMerchant,
    #[msg("treasury token account does not match factory config")]
    WrongTreasury,
    #[msg("Token account mint does not match configured mint")]
    WrongMint,
    #[msg("Unknown or unsupported destination chain")]
    InvalidDomain,
    #[msg("Cross-chain routes require a non-zero CCTP mint recipient")]
    CrossChainRequiresRecipient,
    #[msg("Same-chain routes must leave the CCTP mint recipient zeroed")]
    SameChainRecipientMustBeZero,
    #[msg("Destination chain domain ids must be distinct")]
    DuplicateDomains,
    #[msg("This merchant already has the maximum number of registered receivers")]
    MaxReceiversExceeded,
    #[msg("CCTP program account does not match the expected id")]
    WrongCctpProgram,
    #[msg("caller_token_account is not owned by the calling signer")]
    WrongCaller,
    #[msg("This route is same-chain but merchant_token_account was not supplied")]
    MissingMerchantAccount,
    #[msg("This route is cross-chain but one or more CCTP accounts were not supplied")]
    MissingCctpAccounts,
    #[msg("cctp_burn_staging_account is not ATA(receiver_config, mint)")]
    WrongStagingAccount,
    #[msg("receiver_token_account is not ATA(receiver, mint)")]
    NonCanonicalReceiverAta,
    #[msg("reg_tx must not be empty")]
    EmptyRegTx,
    #[msg("reg_tx exceeds MAX_REG_TX_BYTES")]
    RegTxTooLarge,
}

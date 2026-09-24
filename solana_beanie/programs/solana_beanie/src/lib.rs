/*
//! # Deposit Receiver Factory
//!
//! Solana doesn't need separate factory and receier implementation contracts
//! One program instance can serve unlimited merchants by keying every
//! account off PDA seeds. So the "factory" on this side isn't a deployer —
//! it's a small set of PDA-seeded accounts (FactoryConfig, ReceiverConfig,
//! MerchantRegistry) that play the same role:
//!
//! ## Still deliberately NOT in scope
//! - No per-sweep caller incentive (ChainXReceiver's 10%-to-tx.origin cut)
//!   — same as the original single-tenant program, unchanged.
*/
#![allow(unexpected_cfgs)]
#![allow(deprecated)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::{self, Approve, Mint, Token, TokenAccount, Transfer};
use std::str::FromStr;

declare_id!("3M36pB7qKJhmictctHYYfzV5sy6PWAbUNU6rbPuTjSpc");

// ── Constants ─────────────────────────────────────────────────────────────
pub const FEE_BPS: u64 = 50; // 0.50% fee on each sweep — same rate as the EVM/Starknet legs
pub const BPS_DENOM: u64 = 10_000;
pub const CALLER_SHARE_BPS: u64 = 1_000; // 10% of the fee, not of gross — matches ChainXReceiver.sol / StarknetReceiver
pub const WALLET_A_SHARE_PCT: u64 = 60; // of the *remaining* 90% of fee, after the caller's cut
pub const MAX_RECEIVERS_PER_MERCHANT: usize = 32; // matches MerchantFactory.sol / ReceiverFactory (Cairo)

pub const FACTORY_SEED: &[u8] = b"factory";
pub const CONFIG_SEED: &[u8] = b"config";
pub const REGISTRY_SEED: &[u8] = b"registry";

// ── CCTP V2 (Solana) ─────────────────────────────────────────────────────
pub const CCTP_TOKEN_MESSENGER_MINTER_V2: &str = "CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe";
pub const CCTP_MESSAGE_TRANSMITTER_V2: &str = "CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC";

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

    #[inline(never)]
    pub fn initialize_factory(
        ctx: Context<InitializeFactory>,
        starknet_domain: u32,
        base_domain: u32,
        ethereum_domain: u32,
    ) -> Result<()> {
        require!(
            starknet_domain != base_domain
                && starknet_domain != ethereum_domain
                && base_domain != ethereum_domain,
            DepositError::DuplicateDomains
        );

        let cfg = &mut ctx.accounts.factory_config;
        cfg.mint = ctx.accounts.mint.key();
        cfg.wallet_a_token_account = ctx.accounts.wallet_a_token_account.key();
        cfg.wallet_b_token_account = ctx.accounts.wallet_b_token_account.key();
        cfg.starknet_domain = starknet_domain;
        cfg.base_domain = base_domain;
        cfg.ethereum_domain = ethereum_domain;
        cfg.bump = ctx.bumps.factory_config;

        emit!(FactoryInitialized {
            mint: cfg.mint,
            wallet_a_token_account: cfg.wallet_a_token_account,
            wallet_b_token_account: cfg.wallet_b_token_account,
            starknet_domain,
            base_domain,
            ethereum_domain,
        });
        Ok(())
    }

    // ── Announce — mirrors announceReceiver()/announce_receiver(). ─────────
    /// Touches no state, creates nothing. `receiver_config`'s PDA is
    /// derivable off-chain from (merchant, chain, recipient) alone; the
    /// customer-facing `receiver_token_account` address is NOT derivable
    /// this way (it depends on whatever `ephemeral_owner` keypair gets
    /// generated for this route — see register_merchant), so this event
    /// only announces `receiver_config`, same information density as
    /// `predictReceiverAddress` gives on the EVM leg for a not-yet-deployed
    /// clone.
    #[inline(never)]
    pub fn announce_merchant(
        ctx: Context<AnnounceMerchant>,
        merchant: Pubkey,
        cctp_mint_chain: [u8; 32],
        cctp_mint_recipient: [u8; 32],
    ) -> Result<()> {
        if cctp_mint_chain != [0u8; 32] {
            require!(
                cctp_mint_recipient != [0u8; 32],
                DepositError::CrossChainRequiresRecipient
            );
            resolve_domain(&ctx.accounts.factory_config, &cctp_mint_chain)
                .ok_or(DepositError::InvalidDomain)?;
        } else {
            require!(
                cctp_mint_recipient == [0u8; 32],
                DepositError::SameChainRecipientMustBeZero
            );
        }

        let (receiver_config, _) = Pubkey::find_program_address(
            &[
                CONFIG_SEED,
                merchant.as_ref(),
                cctp_mint_chain.as_ref(),
                cctp_mint_recipient.as_ref(),
            ],
            &crate::ID,
        );
        // cctp_burn_staging_account IS derivable in advance (owner =
        // receiver_config PDA), so we can still announce it — just not
        // receiver_token_account, which depends on ephemeral_owner.
        let cctp_burn_staging_account =
            get_associated_token_address(&receiver_config, &ctx.accounts.factory_config.mint);
        emit!(MerchantAnnounced {
            merchant,
            receiver_config,
            cctp_burn_staging_account,
            cctp_mint_chain,
            cctp_mint_recipient,
        });
        Ok(())
    }

    // ── Register merchant ────────────────────────────────────────────────
    /// `ephemeral_owner` is a real, on-curve keypair — required so
    /// `receiver_token_account` passes ordinary CEX withdrawal
    /// destination checks (an off-curve PDA-owned account has no private
    /// key and fails those checks). This instruction delegates spending
    /// authority to `receiver_config` and hands it CloseAccount authority,
    /// but deliberately never reassigns AccountOwner — the caller is
    /// expected to discard `ephemeral_owner`'s private key after this
    /// confirms, same trust assumption as the single-tenant version.
    ///
    /// Neither `receiver_token_account` nor `cctp_burn_staging_account` is
    /// created here — both must already exist. `receiver_token_account`'s
    /// address depends on `ephemeral_owner` (generated off-chain, the same
    /// way the single-tenant deploy script always worked);
    /// `cctp_burn_staging_account`'s address is the ATA of
    /// (receiver_config PDA, mint), fully derivable off-chain before
    /// `receiver_config` itself is initialized. Whoever provisions a
    /// merchant creates both up front (e.g. with the associated-token
    /// program's own idempotent `create` instruction) and this program
    /// only ever checks them, the same way it already checks
    /// `receiver_token_account` today.
    #[inline(never)]
    pub fn register_merchant(
        ctx: Context<RegisterMerchant>,
        merchant: Pubkey,
        cctp_mint_chain: [u8; 32],
        cctp_mint_recipient: [u8; 32],
    ) -> Result<()> {
        {
            let registry = &ctx.accounts.merchant_registry;
            require!(
                (registry.receiver_count as usize) < MAX_RECEIVERS_PER_MERCHANT,
                DepositError::MaxReceiversExceeded
            );
        }

        let cctp_domain_id: u32 = if cctp_mint_chain != [0u8; 32] {
            require!(
                cctp_mint_recipient != [0u8; 32],
                DepositError::CrossChainRequiresRecipient
            );
            resolve_domain(&ctx.accounts.factory_config, &cctp_mint_chain)
                .ok_or(DepositError::InvalidDomain)?
        } else {
            require!(
                cctp_mint_recipient == [0u8; 32],
                DepositError::SameChainRecipientMustBeZero
            );
            0
        };

        // ── Delegate + CloseAccount handoff — receiver_token_account stays
        // on-curve, owned by ephemeral_owner, the whole time. ──────────────
        token::approve(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Approve {
                    to: ctx.accounts.receiver_token_account.to_account_info(),
                    delegate: ctx.accounts.receiver_config.to_account_info(),
                    authority: ctx.accounts.ephemeral_owner.to_account_info(),
                },
            ),
            u64::MAX,
        )?;

        token::set_authority(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::SetAuthority {
                    account_or_mint: ctx.accounts.receiver_token_account.to_account_info(),
                    current_authority: ctx.accounts.ephemeral_owner.to_account_info(),
                },
            ),
            anchor_spl::token::spl_token::instruction::AuthorityType::CloseAccount,
            Some(ctx.accounts.receiver_config.key()),
        )?;

        let cfg = &mut ctx.accounts.receiver_config;
        cfg.merchant = merchant;
        cfg.mint = ctx.accounts.factory_config.mint;
        cfg.receiver_token_account = ctx.accounts.receiver_token_account.key();
        cfg.merchant_vault_token_account = ctx.accounts.merchant_vault_token_account.key();
        cfg.cctp_mint_chain = cctp_mint_chain;
        cfg.cctp_mint_recipient = cctp_mint_recipient;
        cfg.cctp_domain_id = cctp_domain_id;
        cfg.cctp_burn_staging_account = ctx.accounts.cctp_burn_staging_account.key();
        cfg.bump = ctx.bumps.receiver_config;

        let registry = &mut ctx.accounts.merchant_registry;
        if registry.merchant == Pubkey::default() {
            registry.merchant = merchant;
        }
        let idx = registry.receiver_count as usize;
        registry.receivers[idx] = ctx.accounts.receiver_config.key();
        registry.receiver_count += 1;

        emit!(MerchantRegistered {
            merchant,
            receiver_config: ctx.accounts.receiver_config.key(),
            receiver_token_account: ctx.accounts.receiver_token_account.key(),
            cctp_mint_chain,
            cctp_mint_recipient,
        });

        msg!(
            "register_merchant: merchant={} receiver_config={} receiver_token_account={} cross_chain={}",
            merchant,
            ctx.accounts.receiver_config.key(),
            ctx.accounts.receiver_token_account.key(),
            cctp_mint_chain != [0u8; 32],
        );
        Ok(())
    }

    // ── Sweep — one instruction, one Accounts struct ───────────────────────
    /// Fee math and the caller/protocol payouts happen exactly once,
    /// regardless of route. Only the `net` destination branches: local
    /// transfer to `merchant_vault_token_account`, or a same-account
    /// transfer to `cctp_burn_staging_account` (via the delegate approved
    /// in register_merchant) followed by the CCTP burn from there — CCTP's
    /// `has_one = owner` needs a literal owner, which `receiver_config` is
    /// only for the staging account, never for `receiver_token_account`
    /// itself. The 15 CCTP accounts are `Option<...>`, absent (client
    /// passes `crate::ID`) on a same-chain sweep.
    #[inline(never)]
    pub fn sweep(
        ctx: Context<Sweep>,
        max_fee_bps: Option<u64>,
        min_finality_threshold: Option<u32>,
    ) -> Result<()> {
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
        let protocol_fee = fee
            .checked_sub(fee_to_caller)
            .ok_or(DepositError::ArithmeticOverflow)?;
        let to_a = protocol_fee
            .checked_mul(WALLET_A_SHARE_PCT)
            .ok_or(DepositError::ArithmeticOverflow)?
            .checked_div(100)
            .ok_or(DepositError::ArithmeticOverflow)?;
        let to_b = protocol_fee
            .checked_sub(to_a)
            .ok_or(DepositError::ArithmeticOverflow)?;

        let merchant = ctx.accounts.receiver_config.merchant;
        let chain = ctx.accounts.receiver_config.cctp_mint_chain;
        let recipient = ctx.accounts.receiver_config.cctp_mint_recipient;
        let bump = ctx.accounts.receiver_config.bump;
        let signer_seeds: &[&[u8]] = &[
            CONFIG_SEED,
            merchant.as_ref(),
            chain.as_ref(),
            recipient.as_ref(),
            &[bump],
        ];
        let signer = &[signer_seeds];

        // ── shared payouts: caller cut + both protocol wallets. Authorized
        // by receiver_config acting as the *delegate* on
        // receiver_token_account (never the owner) — same as before. ──────
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
        if to_a > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: ctx.accounts.wallet_a_token_account.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                to_a,
            )?;
        }
        if to_b > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: ctx.accounts.wallet_b_token_account.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                to_b,
            )?;
        }

        // ── branch: local settlement vs. CCTP burn ─────────────────────────
        if !cross_chain {
            let vault = ctx
                .accounts
                .merchant_vault_token_account
                .as_ref()
                .ok_or(DepositError::MissingVaultAccount)?;
            require_keys_eq!(
                vault.key(),
                ctx.accounts.receiver_config.merchant_vault_token_account,
                DepositError::WrongVault
            );

            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.receiver_token_account.to_account_info(),
                        to: vault.to_account_info(),
                        authority: ctx.accounts.receiver_config.to_account_info(),
                    },
                    signer,
                ),
                net,
            )?;

            emit!(Swept {
                receiver_config: ctx.accounts.receiver_config.key(),
                gross_amount: balance,
                net_amount: net,
                fee_amount: fee,
                fee_to_caller,
                fee_to_wallet_a: to_a,
                fee_to_wallet_b: to_b,
                slot: Clock::get()?.slot,
            });
            return Ok(());
        }

        // ── cross-chain: unwrap the optional accounts/params ────────────────
        let max_fee_bps = max_fee_bps.ok_or(DepositError::MissingCctpParams)?;
        let min_finality_threshold =
            min_finality_threshold.ok_or(DepositError::MissingCctpParams)?;
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
        require_keys_eq!(
            cctp_burn_staging_account.key(),
            ctx.accounts.receiver_config.cctp_burn_staging_account,
            DepositError::WrongStagingAccount
        );

        // ── net → staging account, via the same delegate every other
        // transfer here uses. receiver_token_account's AccountOwner is
        // untouched. ─────────────────────────────────────────────────────
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

        let max_fee = net
            .checked_mul(max_fee_bps)
            .ok_or(DepositError::ArithmeticOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(DepositError::ArithmeticOverflow)?;

        // ── staging account → CCTP depositForBurn. receiver_config is this
        // account's genuine AccountOwner (set at creation, off-chain), so
        // it satisfies CCTP's has_one = owner constraint directly. ─────────
        let params = CctpDepositForBurnParams {
            amount: net,
            destination_domain: ctx.accounts.receiver_config.cctp_domain_id,
            mint_recipient: Pubkey::new_from_array(recipient),
            destination_caller: Pubkey::default(),
            max_fee,
            min_finality_threshold,
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
            AccountMeta::new(cctp_burn_staging_account.key(), false), // burn_token_account
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
            AccountMeta::new_readonly(cctp_event_authority.key(), false), // event_cpi
            AccountMeta::new_readonly(cctp_token_messenger_minter_program.key(), false), // event_cpi
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
            gross_amount: balance,
            net_amount: net,
            fee_amount: fee,
            fee_to_caller,
            fee_to_wallet_a: to_a,
            fee_to_wallet_b: to_b,
            destination_domain: ctx.accounts.receiver_config.cctp_domain_id,
            max_fee,
        });

        Ok(())
    }
}

// ── Route/domain resolution ────────────────────────────────────────────────
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
    } else {
        None
    }
}

// ── Account Contexts ────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeFactory<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(constraint = wallet_a_token_account.mint == mint.key() @ DepositError::WrongMint)]
    pub wallet_a_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        constraint = wallet_b_token_account.mint == mint.key() @ DepositError::WrongMint,
        constraint = wallet_b_token_account.owner != wallet_a_token_account.owner @ DepositError::WalletsSame,
    )]
    pub wallet_b_token_account: Box<Account<'info, TokenAccount>>,
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
pub struct AnnounceMerchant<'info> {
    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Account<'info, FactoryConfig>,
}

#[derive(Accounts)]
#[instruction(merchant: Pubkey, cctp_mint_chain: [u8; 32], cctp_mint_recipient: [u8; 32])]
pub struct RegisterMerchant<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// Real, on-curve keypair currently owning `receiver_token_account`.
    /// Discard this key after the transaction confirms.
    pub ephemeral_owner: Signer<'info>,

    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Account<'info, FactoryConfig>,

    #[account(
        mut,
        constraint = receiver_token_account.mint == factory_config.mint @ DepositError::WrongMint,
        constraint = receiver_token_account.owner == ephemeral_owner.key() @ DepositError::WrongReceiver,
    )]
    pub receiver_token_account: Box<Account<'info, TokenAccount>>,

    #[account(constraint = merchant_vault_token_account.mint == factory_config.mint @ DepositError::WrongMint)]
    pub merchant_vault_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = payer,
        space = 8 + ReceiverConfig::INIT_SPACE,
        seeds = [CONFIG_SEED, merchant.as_ref(), cctp_mint_chain.as_ref(), cctp_mint_recipient.as_ref()],
        bump,
    )]
    pub receiver_config: Account<'info, ReceiverConfig>,

    /// Must already exist as the ATA of (receiver_config, factory_config.mint)
    /// — created off-chain (e.g. via the associated-token program's own
    /// idempotent `create` instruction) before this call, not by this
    /// program. receiver_config is its real, native owner from the moment
    /// it was created — no delegate, no SetAuthority, ever, for this
    /// account, which is what lets sweep()'s cross-chain branch burn
    /// straight out of it.
    #[account(
        mut,
        constraint = cctp_burn_staging_account.mint == factory_config.mint @ DepositError::WrongMint,
        constraint = cctp_burn_staging_account.owner == receiver_config.key() @ DepositError::WrongStagingAccount,
    )]
    pub cctp_burn_staging_account: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + MerchantRegistry::INIT_SPACE,
        seeds = [REGISTRY_SEED, merchant.as_ref()],
        bump,
    )]
    pub merchant_registry: Account<'info, MerchantRegistry>,

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
        constraint = receiver_token_account.key() == receiver_config.receiver_token_account @ DepositError::WrongReceiver,
    )]
    pub receiver_token_account: Box<Account<'info, TokenAccount>>,

    #[account(seeds = [FACTORY_SEED], bump = factory_config.bump)]
    pub factory_config: Account<'info, FactoryConfig>,

    #[account(
        mut,
        constraint = wallet_a_token_account.key() == factory_config.wallet_a_token_account @ DepositError::WrongWalletA,
    )]
    pub wallet_a_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = wallet_b_token_account.key() == factory_config.wallet_b_token_account @ DepositError::WrongWalletB,
    )]
    pub wallet_b_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        seeds = [
            CONFIG_SEED,
            receiver_config.merchant.as_ref(),
            receiver_config.cctp_mint_chain.as_ref(),
            receiver_config.cctp_mint_recipient.as_ref(),
        ],
        bump = receiver_config.bump,
    )]
    pub receiver_config: Box<Account<'info, ReceiverConfig>>,

    // ── Same-chain only ─────────────────────────────────────────────────
    pub merchant_vault_token_account: Option<Box<Account<'info, TokenAccount>>>,

    // ── Cross-chain only. Client passes `crate::ID` in place of any of
    // these on a same-chain sweep to signal "None". ───────────────────────
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
    pub wallet_a_token_account: Pubkey,
    pub wallet_b_token_account: Pubkey,
    pub starknet_domain: u32,
    pub base_domain: u32,
    pub ethereum_domain: u32,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct ReceiverConfig {
    pub merchant: Pubkey,
    pub mint: Pubkey,
    pub receiver_token_account: Pubkey,
    pub merchant_vault_token_account: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
    pub cctp_domain_id: u32,
    pub cctp_burn_staging_account: Pubkey,
    pub bump: u8,
}

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
    pub wallet_a_token_account: Pubkey,
    pub wallet_b_token_account: Pubkey,
    pub starknet_domain: u32,
    pub base_domain: u32,
    pub ethereum_domain: u32,
}

#[event]
pub struct MerchantAnnounced {
    pub merchant: Pubkey,
    pub receiver_config: Pubkey,
    pub cctp_burn_staging_account: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
}

#[event]
pub struct MerchantRegistered {
    pub merchant: Pubkey,
    pub receiver_config: Pubkey,
    pub receiver_token_account: Pubkey,
    pub cctp_mint_chain: [u8; 32],
    pub cctp_mint_recipient: [u8; 32],
}

#[event]
pub struct Swept {
    pub receiver_config: Pubkey,
    pub gross_amount: u64,
    pub net_amount: u64,
    pub fee_amount: u64,
    pub fee_to_caller: u64,
    pub fee_to_wallet_a: u64,
    pub fee_to_wallet_b: u64,
    pub slot: u64,
}

#[event]
pub struct SweptCrossChain {
    pub receiver_config: Pubkey,
    pub gross_amount: u64,
    pub net_amount: u64,
    pub fee_amount: u64,
    pub fee_to_caller: u64,
    pub fee_to_wallet_a: u64,
    pub fee_to_wallet_b: u64,
    pub destination_domain: u32,
    pub max_fee: u64,
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[error_code]
pub enum DepositError {
    #[msg("Arithmetic overflow in fee calculation")]
    ArithmeticOverflow,
    #[msg("receiver_token_account does not match config")]
    WrongReceiver,
    #[msg("merchant_vault_token_account does not match config")]
    WrongVault,
    #[msg("wallet_a token account does not match factory config")]
    WrongWalletA,
    #[msg("wallet_b token account does not match factory config")]
    WrongWalletB,
    #[msg("Token account mint does not match configured mint")]
    WrongMint,
    #[msg("wallet_a and wallet_b token accounts must have different owners")]
    WalletsSame,
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
    #[msg("CCTP program account does not match the expected TokenMessengerMinterV2 / MessageTransmitterV2 id")]
    WrongCctpProgram,
    #[msg("caller_token_account is not owned by the calling signer")]
    WrongCaller,
    #[msg("This route is same-chain but merchant_vault_token_account was not supplied")]
    MissingVaultAccount,
    #[msg("This route is cross-chain but one or more CCTP accounts were not supplied")]
    MissingCctpAccounts,
    #[msg("This route is cross-chain but max_fee_bps / min_finality_threshold were not supplied")]
    MissingCctpParams,
    #[msg("cctp_burn_staging_account does not match the expected ATA for (mint, receiver_config)")]
    WrongStagingAccount,
}

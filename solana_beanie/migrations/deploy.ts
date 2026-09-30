import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import {
  getAssociatedTokenAddress,
  createAssociatedTokenAccountInstruction,
  getAccount
} from "@solana/spl-token";

// ── Configuration Constants ──────────────────────────────────────────────────

// Update this if using a different token mint (e.g., Devnet USDC vs Mainnet USDC)
const MINT_PUBKEY = new PublicKey("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");

// Circle CCTP V2 Domain IDs
const CCTP_DOMAINS = {
  starknet: 25,
  base: 6,
  ethereum: 0,
  monad: 15,
  arbitrum: 3,
};

const FACTORY_SEED = Buffer.from("factory");

module.exports = async function (provider: anchor.AnchorProvider) {
  anchor.setProvider(provider);

  // 1. Load Program from the Anchor workspace context
  const program = anchor.workspace.Sol as Program;
  if (!program) {
    throw new Error(
      "Program 'Sol' not found in anchor.workspace. Ensure your Anchor.toml matches the program name."
    );
  }

  const payer = provider.wallet.publicKey;
  console.log(`Payer / Wallet: ${payer.toBase58()}`);
  console.log(`Program ID:     ${program.programId.toBase58()}`);

  // 2. Derive Factory PDA
  const [factoryConfigPda] = PublicKey.findProgramAddressSync(
    [FACTORY_SEED],
    program.programId
  );
  console.log(`Factory PDA:    ${factoryConfigPda.toBase58()}`);

  // Idempotency Check: Don't re-initialize if already deployed & initialized
  const factoryAccount = await provider.connection.getAccountInfo(factoryConfigPda);
  if (factoryAccount !== null) {
    console.log("⚠️️  Factory Config PDA is already initialized! Skipping setup.");
    return;
  }

  // 3. Resolve Treasury Token Account (ATA)
  // Defaults to creating/using an ATA owned by the deployment wallet
  const treasuryTokenAccount = await getAssociatedTokenAddress(
    MINT_PUBKEY,
    payer
  );
  console.log(`Treasury ATA:   ${treasuryTokenAccount.toBase58()}`);

  const tx = new anchor.web3.Transaction();

  // Create Treasury ATA if it does not exist yet
  try {
    await getAccount(provider.connection, treasuryTokenAccount);
    console.log(" Treasury ATA exists.");
  } catch (_err) {
    console.log(" Treasury ATA missing. Adding creation instruction...");
    tx.add(
      createAssociatedTokenAccountInstruction(
        payer,                  // payer
        treasuryTokenAccount,   // ata address
        payer,                  // owner
        MINT_PUBKEY             // mint
      )
    );
  }

  // 4. Construct `initialize_factory` instruction
  const initIx = await program.methods
    .initializeFactory(
      CCTP_DOMAINS.starknet,
      CCTP_DOMAINS.base,
      CCTP_DOMAINS.ethereum,
      CCTP_DOMAINS.monad,
      CCTP_DOMAINS.arbitrum
    )
    .accountsStrict({
      payer: payer,
      mint: MINT_PUBKEY,
      treasuryTokenAccount: treasuryTokenAccount,
      factoryConfig: factoryConfigPda,
      systemProgram: SystemProgram.programId,
    })
    .instruction();

  tx.add(initIx);

  // 5. Submit Transaction
  console.log("Sending initialize_factory transaction...");
  const sig = await provider.sendAndConfirm(tx, [], {
    commitment: "confirmed",
  });

  console.log(`✅ Factory Initialized successfully!`);
  console.log(`Tx Signature: ${sig}`);
};
// tests/solana_beanie.ts
// Anchor/Mocha suite for the receiver-keyed `sol` factory.
//
// Run:  anchor test        (mocha timeout must be generous: -t 1000000 in Anchor.toml)
//
// Flow under test
//   prepare():  grind stand-in keypair -> tx1 (payer-only: durable nonce with
//               authority=PAYER, ATA(receiver), staging ATA) -> sign ONE tx
//               [AdvanceNonce(payer), register_merchant(receiver)] -> key goes
//               out of scope. Nothing after prepare() can use the key
//               (except tests that opt into keepKey).
//   announce(): payer-only tx that stores the signed reg tx in the pinned
//               [pending, receiver] account. No config exists yet.
//   keeper:     read the blob from chain and broadcast it unchanged; that is
//               the only place [config, receiver] is created.
//   close:      the nonce can't be closed inside the reg tx (a full withdraw fails
//               in the block it was advanced), so the payer, who is the nonce
//               authority, withdraws it back to itself in a follow-up tx.
//
// NOTE: JS cannot truly zeroize memory; "discarded" here means unreachable.
// The cross-chain happy path (real CCTP CPI) stays it.skip — see the flags there.

import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  LAMPORTS_PER_SOL,
  Transaction,
  SystemProgram,
  NonceAccount,
  NONCE_ACCOUNT_LENGTH,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import {
  createMint,
  createAccount,
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountInstruction,
  mintTo,
  getAccount,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { assert } from "chai";

import { Sol } from "../target/types/sol";


// ── Constants (mirror on-chain values) ───────────────────────────────────────
const FEE_BPS_BI = BigInt(50);
const BPS_DENOM_BI = BigInt(10_000);
const CALLER_SHARE_BPS_BI = BigInt(1_000);
const CCTP_MAX_FEE_BPS_BI = BigInt(3);
const U64_MAX_BI = (BigInt(1) << BigInt(64)) - BigInt(1);
const MAX_RECEIVERS_PER_MERCHANT = 32;
const MAX_TX_BYTES = 1232;
const PENDING_FIXED_LEN = 1 + 4;

const FACTORY_SEED = Buffer.from("factory");
const CONFIG_SEED = Buffer.from("config");
const PENDING_SEED = Buffer.from("pending");
const REGISTRY_SEED = Buffer.from("registry");

const CCTP_TOKEN_MESSENGER_MINTER_V2 = new PublicKey("CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe");
const CCTP_MESSAGE_TRANSMITTER_V2 = new PublicKey("CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC");

const calcFee = (n: bigint): bigint => (n * FEE_BPS_BI) / BPS_DENOM_BI;
const calcNet = (n: bigint): bigint => n - calcFee(n);
const calcFeeToCaller = (fee: bigint): bigint => (fee * CALLER_SHARE_BPS_BI) / BPS_DENOM_BI;
const calcFeeToTreasury = (fee: bigint): bigint => fee - calcFeeToCaller(fee);
const calcMaxFee = (gross: bigint): bigint => (gross * CCTP_MAX_FEE_BPS_BI) / BPS_DENOM_BI;

function chainNameSeed(name: string): Buffer {
  const buf = Buffer.alloc(32);
  const nameBuf = Buffer.from(name, "utf-8");
  nameBuf.copy(buf, 0, 0, Math.min(nameBuf.length, 32));
  return buf;
}
const ZERO_32 = Buffer.alloc(32);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ── Types ────────────────────────────────────────────────────────────────────
interface Route {
  merchant: PublicKey;
  chain: Buffer;
  recipient: Buffer;
  receiver: PublicKey;
  receiverTA: PublicKey;
  stagingTA: PublicKey;
  merchantTA: PublicKey;
  nonce: PublicKey;
  configPda: PublicKey;
  pendingPda: PublicKey;
  registryPda: PublicKey;
  regTx: Buffer;        // the single fully signed tx (what announce stores)
  receiverKp?: Keypair; // only with keepKey
}

interface PrepareOpts {
  keepKey?: boolean;
  /** Override accounts of the SIGNED register instruction (announce stays valid). */
  regAccounts?:
  | Record<string, PublicKey>
  | ((ctx: { receiver: PublicKey; configPda: PublicKey }) => Promise<Record<string, PublicKey>>);
  /** Sign a register instruction whose route differs from what announce was given. */
  regRoute?: { chain: Buffer; recipient: Buffer };
}

// ── Suite ────────────────────────────────────────────────────────────────────
describe("Solana Beanie (receiver-keyed factory, pinned pre-signed registration)", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.sol as Program<Sol>;
  const connection = provider.connection;
  const programId = program.programId;

  const payerKp = Keypair.generate();
  const callerKp = Keypair.generate();
  const treasuryOwnerKp = Keypair.generate();
  const badActorKp = Keypair.generate();

  const STARKNET_DOMAIN = 21;
  const BASE_DOMAIN = 6;
  const ETHEREUM_DOMAIN = 0;
  const MONAD_DOMAIN = 15;
  const ARBITRUM_DOMAIN = 3;
  // Mirrors Rust's SAME_CHAIN_DOMAIN_SENTINEL: same-chain routes store this,
  // NOT 0, because 0 is Ethereum's real CCTP domain id.
  const SAME_CHAIN_DOMAIN_SENTINEL = 0xffffffff;

  let usdtMint: PublicKey;
  let wrongMint: PublicKey;
  let treasuryTA: PublicKey;
  let callerTA: PublicKey;
  let factoryConfigPda: PublicKey;
  let factoryBump: number;

  // ── generic helpers ──────────────────────────────────────────────────────
  async function airdrop(pubkey: PublicKey, sol: number) {
    const sig = await connection.requestAirdrop(pubkey, sol * LAMPORTS_PER_SOL);
    await connection.confirmTransaction(sig, "confirmed");
  }

  /** Always a fresh NON-ATA token account (spl createAccount without a keypair makes an ATA). */
  async function newTokenAccount(mint: PublicKey, owner: PublicKey): Promise<PublicKey> {
    return createAccount(connection, payerKp, mint, owner, Keypair.generate());
  }

  async function tokenBalance(account: PublicKey): Promise<bigint> {
    return (await getAccount(connection, account)).amount;
  }

  async function waitSlots(n: number) {
    const start = await connection.getSlot("confirmed");
    while ((await connection.getSlot("confirmed")) < start + n) await sleep(200);
  }

  async function sendRaw(raw: Buffer): Promise<string> {
    const sig = await connection.sendRawTransaction(raw, { preflightCommitment: "confirmed" });
    const res = await connection.confirmTransaction(sig, "confirmed");
    if (res.value.err) throw new Error(JSON.stringify(res.value.err));
    return sig;
  }

  async function eventsOf(sig: string): Promise<{ name: string; data: any }[]> {
    let tx: any = null;
    for (let i = 0; i < 20 && !tx; i++) {
      tx = await connection.getTransaction(sig, { commitment: "confirmed", maxSupportedTransactionVersion: 0 });
      if (!tx) await sleep(250);
    }
    assert.isNotNull(tx, "tx not found");
    const parser = new anchor.EventParser(programId, new anchor.BorshCoder(program.idl as any));
    return [...parser.parseLogs(tx.meta.logMessages)] as any;
  }
  const findEvent = (evs: { name: string; data: any }[], name: string) =>
    evs.find((e) => e.name.toLowerCase() === name.toLowerCase());

  /** Asserts the promise fails (optionally with a named program error) and returns the failure text. */
  async function expectFail(p: Promise<unknown>, errName?: string, label = ""): Promise<string> {
    let failure: any = null;
    try { await p; } catch (e) { failure = e; }
    assert.isNotNull(failure, `${label} expected a failure${errName ? ` (${errName})` : ""}`);
    let logs: string[] = [];
    try {
      if (typeof failure.getLogs === "function") logs = await failure.getLogs(connection);
      else if (failure.logs) logs = failure.logs;
    } catch { /* ignore */ }
    const hay = `${failure}\n${(logs || []).join("\n")}\n${JSON.stringify(failure.transactionLogs ?? [])}`;
    if (errName) {
      const code = (program.idl as any).errors?.find((e: any) => e.name === errName)?.code;
      const hit =
        hay.includes(errName) ||
        (code !== undefined && (hay.includes(`0x${code.toString(16)}`) || hay.includes(`"Custom":${code}`)));
      assert.isTrue(hit, `${label} expected ${errName}, got: ${hay.slice(0, 700)}`);
    }
    return hay;
  }

  const walletPayer = (provider.wallet as any).payer as Keypair;

  /** Payer (nonce authority) withdraws the whole nonce balance back to itself. Returns the lamports returned. */
  async function closeNonce(nonce: PublicKey, authority: Keypair = payerKp): Promise<number> {
    const lamports = await connection.getBalance(nonce, "confirmed");
    const tx = new Transaction().add(
      SystemProgram.nonceWithdraw({
        noncePubkey: nonce,
        authorizedPubkey: authority.publicKey,
        toPubkey: payerKp.publicKey,
        lamports,
      }),
    );
    tx.feePayer = walletPayer.publicKey; // keep the payer's balance delta exact
    await sendAndConfirmTransaction(connection, tx, [walletPayer, authority], { commitment: "confirmed" });
    return lamports;
  }

  // ── derivations ──────────────────────────────────────────────────────────
  const deriveConfig = (receiver: PublicKey) =>
    PublicKey.findProgramAddressSync([CONFIG_SEED, receiver.toBuffer()], programId);
  const derivePending = (receiver: PublicKey) =>
    PublicKey.findProgramAddressSync([PENDING_SEED, receiver.toBuffer()], programId);
  const deriveRegistry = (merchant: PublicKey) =>
    PublicKey.findProgramAddressSync([REGISTRY_SEED, merchant.toBuffer()], programId);

  const factoryCfg = () => program.account.factoryConfig.fetch(factoryConfigPda);
  const configOf = (pda: PublicKey) => program.account.receiverConfig.fetch(pda);
  const pendingOf = (pda: PublicKey) => program.account.pendingRegistration.fetch(pda);
  const registryOf = (merchant: PublicKey) => program.account.merchantRegistry.fetch(deriveRegistry(merchant)[0]);

  async function assertFactoryStillUninitialized() {
    assert.isNull(await connection.getAccountInfo(factoryConfigPda), "factory_config must NOT exist yet");
  }

  // ── route lifecycle ──────────────────────────────────────────────────────
  /** Signs the ONE registration tx [AdvanceNonce, register_merchant] against the nonce's current value. */
  async function signRegTx(
    f: Omit<Route, "regTx" | "receiverKp">,
    receiverKp: Keypair,
    opts: PrepareOpts = {},
  ): Promise<Buffer> {
    const nonceInfo = await connection.getAccountInfo(f.nonce, "confirmed");
    const nonceValue = NonceAccount.fromAccountData(nonceInfo!.data).nonce;
    const overrides =
      typeof opts.regAccounts === "function"
        ? await opts.regAccounts({ receiver: f.receiver, configPda: f.configPda })
        : opts.regAccounts ?? {};
    const route = opts.regRoute ?? { chain: f.chain, recipient: f.recipient };

    const registerIx = await program.methods
      .registerMerchant(f.merchant, Array.from(route.chain), Array.from(route.recipient))
      .accounts({
        payer: payerKp.publicKey,
        receiver: f.receiver,
        factoryConfig: factoryConfigPda,
        receiverTokenAccount: f.receiverTA,
        merchantTokenAccount: f.merchantTA,
        receiverConfig: f.configPda,
        cctpBurnStagingAccount: f.stagingTA,
        merchantRegistry: f.registryPda,
        pendingRegistration: f.pendingPda,
        ...overrides,
      } as any)
      .instruction();

    const tx = new Transaction();
    tx.feePayer = payerKp.publicKey;
    tx.recentBlockhash = nonceValue;
    tx.add(
      SystemProgram.nonceAdvance({ noncePubkey: f.nonce, authorizedPubkey: payerKp.publicKey }),
      registerIx,
    );
    tx.sign(payerKp, receiverKp);
    return tx.serialize();
  }

  /**
   * Everything that needs the receiver secret key happens in here: one
   * signature over one tx. The key is unreachable after return (unless keepKey).
   */
  async function prepare(merchant: PublicKey, chain: Buffer, recipient: Buffer, opts: PrepareOpts = {}): Promise<Route> {
    const receiverKp = Keypair.generate(); // stands in for the vanity-grind result
    const receiver = receiverKp.publicKey;
    const [configPda] = deriveConfig(receiver);
    const [pendingPda] = derivePending(receiver);
    const [registryPda] = deriveRegistry(merchant);
    const receiverTA = getAssociatedTokenAddressSync(usdtMint, receiver, false, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID);
    const stagingTA = getAssociatedTokenAddressSync(usdtMint, configPda, true, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID);
    const nonceKp = Keypair.generate(); // throwaway; only signs the account creation
    const merchantTA = await newTokenAccount(usdtMint, Keypair.generate().publicKey);

    // tx1 — payer signs only; the receiver key is not involved.
    const nonceRent = await connection.getMinimumBalanceForRentExemption(NONCE_ACCOUNT_LENGTH);
    const tx1 = new Transaction().add(
      SystemProgram.createAccount({
        fromPubkey: payerKp.publicKey,
        newAccountPubkey: nonceKp.publicKey,
        lamports: nonceRent,
        space: NONCE_ACCOUNT_LENGTH,
        programId: SystemProgram.programId,
      }),
      SystemProgram.nonceInitialize({ noncePubkey: nonceKp.publicKey, authorizedPubkey: payerKp.publicKey }),
      createAssociatedTokenAccountInstruction(payerKp.publicKey, receiverTA, receiver, usdtMint, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID),
      createAssociatedTokenAccountInstruction(payerKp.publicKey, stagingTA, configPda, usdtMint, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID),
    );
    await sendAndConfirmTransaction(connection, tx1, [payerKp, nonceKp], { commitment: "confirmed" });
    await waitSlots(2); // a nonce can't be advanced in the slot it was initialized

    const base = {
      merchant, chain, recipient, receiver, receiverTA, stagingTA, merchantTA,
      nonce: nonceKp.publicKey, configPda, pendingPda, registryPda,
    };
    const regTx = await signRegTx(base, receiverKp, opts);
    return { ...base, regTx, receiverKp: opts.keepKey ? receiverKp : undefined };
  }

  /** Payer-only announce tx, built with a fresh blockhash. `blob` and `signer` can be overridden to model a squatter. */
  async function announceTx(r: Route, blob: Buffer = r.regTx, signer: Keypair = payerKp): Promise<Buffer> {
    const ix = await program.methods
      .announceMerchant(r.merchant, Array.from(r.chain), Array.from(r.recipient), r.receiver, blob)
      .accounts({
        payer: signer.publicKey,
        factoryConfig: factoryConfigPda,
        pendingRegistration: r.pendingPda,
      } as any)
      .instruction();
    const tx = new Transaction();
    tx.feePayer = signer.publicKey;
    tx.recentBlockhash = (await connection.getLatestBlockhash("confirmed")).blockhash;
    tx.add(ix);
    tx.sign(signer);
    return tx.serialize();
  }
  const announce = async (r: Route, blob?: Buffer, signer?: Keypair) => sendRaw(await announceTx(r, blob, signer));

  /** Keeper: read the stored blob from chain and broadcast it unchanged. */
  async function broadcastStored(pendingPda: PublicKey): Promise<string> {
    const p = await pendingOf(pendingPda);
    return sendRaw(Buffer.from(p.regTx as any));
  }

  async function onboard(merchant: PublicKey, chain: Buffer, recipient: Buffer): Promise<Route> {
    const r = await prepare(merchant, chain, recipient);
    await announce(r);
    await waitSlots(2);
    await broadcastStored(r.pendingPda);
    return r;
  }

  // ── setup ────────────────────────────────────────────────────────────────
  before(async () => {
    await airdrop(payerKp.publicKey, 100);
    await Promise.all([callerKp, treasuryOwnerKp, badActorKp].map((kp) => airdrop(kp.publicKey, 2)));

    usdtMint = await createMint(connection, payerKp, payerKp.publicKey, null, 6);
    wrongMint = await createMint(connection, payerKp, payerKp.publicKey, null, 6);
    treasuryTA = await newTokenAccount(usdtMint, treasuryOwnerKp.publicKey);
    callerTA = await newTokenAccount(usdtMint, callerKp.publicKey);

    [factoryConfigPda, factoryBump] = PublicKey.findProgramAddressSync([FACTORY_SEED], programId);
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  INITIALIZE FACTORY
  // ═════════════════════════════════════════════════════════════════════════
  describe("Initialize Factory", () => {
    it("IF-SAD-1: duplicate domains reverts DuplicateDomains", async () => {
      await assertFactoryStillUninitialized();
      await expectFail(
        program.methods.initializeFactory(BASE_DOMAIN, BASE_DOMAIN, ETHEREUM_DOMAIN, MONAD_DOMAIN, ARBITRUM_DOMAIN)
          .accounts({ payer: payerKp.publicKey, mint: usdtMint, treasuryTokenAccount: treasuryTA })
          .signers([payerKp]).rpc(),
        "DuplicateDomains",
      );
      await assertFactoryStillUninitialized();
    });

    it("IF-SAD-1b: a duplicate among the newer domains also reverts DuplicateDomains", async () => {
      await assertFactoryStillUninitialized();
      await expectFail(
        program.methods.initializeFactory(STARKNET_DOMAIN, BASE_DOMAIN, ETHEREUM_DOMAIN, ARBITRUM_DOMAIN, ARBITRUM_DOMAIN)
          .accounts({ payer: payerKp.publicKey, mint: usdtMint, treasuryTokenAccount: treasuryTA })
          .signers([payerKp]).rpc(),
        "DuplicateDomains",
      );
      await assertFactoryStillUninitialized();
    });

    it("IF-SAD-2: wrong-mint treasury reverts WrongMint", async () => {
      const bad = await newTokenAccount(wrongMint, treasuryOwnerKp.publicKey);
      await expectFail(
        program.methods.initializeFactory(STARKNET_DOMAIN, BASE_DOMAIN, ETHEREUM_DOMAIN, MONAD_DOMAIN, ARBITRUM_DOMAIN)
          .accounts({ payer: payerKp.publicKey, mint: usdtMint, treasuryTokenAccount: bad })
          .signers([payerKp]).rpc(),
        "WrongMint",
      );
      await assertFactoryStillUninitialized();
    });

    it("IF-HAPPY-1: initialize_factory stores config and emits FactoryInitialized", async () => {
      const sig = await program.methods.initializeFactory(STARKNET_DOMAIN, BASE_DOMAIN, ETHEREUM_DOMAIN, MONAD_DOMAIN, ARBITRUM_DOMAIN)
        .accounts({ payer: payerKp.publicKey, mint: usdtMint, treasuryTokenAccount: treasuryTA })
        .signers([payerKp]).rpc();
      const ev = findEvent(await eventsOf(sig), "FactoryInitialized")!.data;
      assert.equal(ev.mint.toBase58(), usdtMint.toBase58());
      assert.equal(ev.monadDomain, MONAD_DOMAIN);
      assert.equal(ev.arbitrumDomain, ARBITRUM_DOMAIN);
      const cfg = await factoryCfg();
      assert.equal(cfg.mint.toBase58(), usdtMint.toBase58());
      assert.equal(cfg.treasuryTokenAccount.toBase58(), treasuryTA.toBase58());
      assert.equal(cfg.starknetDomain, STARKNET_DOMAIN);
      assert.equal(cfg.baseDomain, BASE_DOMAIN);
      assert.equal(cfg.ethereumDomain, ETHEREUM_DOMAIN);
      assert.equal(cfg.monadDomain, MONAD_DOMAIN);
      assert.equal(cfg.arbitrumDomain, ARBITRUM_DOMAIN);
      assert.equal(cfg.bump, factoryBump);
    });

    it("IF-HAPPY-2: re-running initialize_factory reverts", async () => {
      await expectFail(
        program.methods.initializeFactory(STARKNET_DOMAIN, BASE_DOMAIN, ETHEREUM_DOMAIN, MONAD_DOMAIN, ARBITRUM_DOMAIN)
          .accounts({ payer: payerKp.publicKey, mint: usdtMint, treasuryTokenAccount: treasuryTA })
          .signers([payerKp]).rpc(),
      );
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  ANNOUNCE — pins the signed blob, creates no config
  // ═════════════════════════════════════════════════════════════════════════
  describe("Announce", () => {
    it("AN-HAPPY-1: stores the signed blob in the pinned account, creates NO config, emits addresses", async () => {
      const merchant = Keypair.generate().publicKey;
      const r = await prepare(merchant, ZERO_32, ZERO_32);
      const raw = await announceTx(r);
      assert.isAtMost(raw.length, MAX_TX_BYTES, "announce tx must fit the 1232-byte limit");

      const sig = await sendRaw(raw);

      const ev = findEvent(await eventsOf(sig), "MerchantAnnounced")!.data;
      assert.equal(ev.merchant.toBase58(), merchant.toBase58());
      assert.equal(ev.receiver.toBase58(), r.receiver.toBase58());
      assert.equal(ev.receiverTokenAccount.toBase58(), r.receiverTA.toBase58());
      assert.equal(ev.receiverConfig.toBase58(), r.configPda.toBase58());
      assert.equal(ev.cctpBurnStagingAccount.toBase58(), r.stagingTA.toBase58());
      assert.equal(ev.pendingRegistration.toBase58(), r.pendingPda.toBase58());

      const pending = await pendingOf(r.pendingPda);
      assert.isTrue(Buffer.from(pending.regTx as any).equals(r.regTx), "stored blob == the tx signed off-chain");
      const info = await connection.getAccountInfo(r.pendingPda);
      assert.equal(info!.data.length, 8 + PENDING_FIXED_LEN + r.regTx.length, "pinned account is exactly blob-sized");

      assert.isNull(await connection.getAccountInfo(r.configPda), "no config at announce");
      assert.isNull(await connection.getAccountInfo(r.registryPda), "no registry write at announce");
    });

    it("AN-HAPPY-2: cross-chain route is accepted", async () => {
      const r = await prepare(Keypair.generate().publicKey, chainNameSeed("STARKNET"), Keypair.generate().publicKey.toBuffer());
      await announce(r);
      assert.isNotNull(await connection.getAccountInfo(r.pendingPda));
    });

    it("AN-HAPPY-4: Monad route is accepted", async () => {
      const r = await prepare(Keypair.generate().publicKey, chainNameSeed("MONAD"), Keypair.generate().publicKey.toBuffer());
      await announce(r);
      assert.isNotNull(await connection.getAccountInfo(r.pendingPda));
    });

    it("AN-HAPPY-5: Arbitrum route is accepted", async () => {
      const r = await prepare(Keypair.generate().publicKey, chainNameSeed("ARBITRUM"), Keypair.generate().publicKey.toBuffer());
      await announce(r);
      assert.isNotNull(await connection.getAccountInfo(r.pendingPda));
    });

    it("AN-SAD-1: cross-chain with zero recipient reverts CrossChainRequiresRecipient", async () => {
      const r = await prepare(Keypair.generate().publicKey, chainNameSeed("BASE"), ZERO_32);
      await expectFail(announce(r), "CrossChainRequiresRecipient");
      assert.isNull(await connection.getAccountInfo(r.pendingPda));
    });

    it("AN-SAD-2: same-chain with non-zero recipient reverts SameChainRecipientMustBeZero", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, Keypair.generate().publicKey.toBuffer());
      await expectFail(announce(r), "SameChainRecipientMustBeZero");
    });

    it("AN-SAD-3: unknown chain reverts InvalidDomain", async () => {
      const r = await prepare(Keypair.generate().publicKey, chainNameSeed("POLYGON"), Keypair.generate().publicKey.toBuffer());
      await expectFail(announce(r), "InvalidDomain");
    });

    it("AN-SAD-4: empty blob reverts EmptyRegTx", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await expectFail(announce(r, Buffer.alloc(0)), "EmptyRegTx");
    });

    it("AN-SAD-5: a second announce for the same receiver reverts and leaves the first blob intact", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await announce(r);
      await expectFail(announce(r, Buffer.from([1, 2, 3, 4])));
      const pending = await pendingOf(r.pendingPda);
      assert.isTrue(Buffer.from(pending.regTx as any).equals(r.regTx));
    });

    it("AN-HAPPY-3: a squatted pinned account (garbage blob) cannot block registration", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      // Attacker announces garbage at [pending, receiver] first; nothing is signed by the receiver.
      await announce(r, Buffer.from([9, 9, 9, 9]), badActorKp);
      await expectFail(announce(r)); // legit announce is refused: address must NOT be disclosed
      const stored = Buffer.from((await pendingOf(r.pendingPda)).regTx as any);
      assert.isFalse(stored.equals(r.regTx), "read-back mismatch is how the client detects the squat");

      // The offline copy of the signed tx still registers: the pinned account is only a holder.
      await waitSlots(2);
      await sendRaw(r.regTx);
      assert.isNotNull(await connection.getAccountInfo(r.configPda));
      assert.isNull(await connection.getAccountInfo(r.pendingPda), "closed by register");
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  REGISTER — the only place config is created; succeeds once
  // ═════════════════════════════════════════════════════════════════════════
  describe("Register (pre-signed, stored blob)", () => {
    it("RM-HAPPY-1: the blob signed before announce lands later and creates config, delegate, registry entry", async () => {
      const merchant = Keypair.generate().publicKey;
      const r = await prepare(merchant, ZERO_32, ZERO_32); // key unreachable after this
      await announce(r);
      assert.isNull(await connection.getAccountInfo(r.configPda), "still unregistered");
      await waitSlots(2);

      const sig = await broadcastStored(r.pendingPda);

      const ev = findEvent(await eventsOf(sig), "MerchantRegistered")!.data;
      assert.equal(ev.receiver.toBase58(), r.receiver.toBase58());
      assert.equal(ev.receiverConfig.toBase58(), r.configPda.toBase58());

      const cfg = await configOf(r.configPda);
      assert.equal(cfg.receiver.toBase58(), r.receiver.toBase58());
      assert.equal(cfg.merchant.toBase58(), merchant.toBase58());
      assert.equal(cfg.mint.toBase58(), usdtMint.toBase58());
      assert.equal(cfg.merchantTokenAccount.toBase58(), r.merchantTA.toBase58());
      assert.equal(cfg.cctpDomainId, SAME_CHAIN_DOMAIN_SENTINEL, "same-chain must NOT store 0 (that's Ethereum's real domain)");

      const acc = await getAccount(connection, r.receiverTA);
      assert.equal(acc.owner.toBase58(), r.receiver.toBase58(), "AccountOwner stays the receiver");
      assert.equal(acc.delegate?.toBase58(), r.configPda.toBase58());
      assert.equal(acc.closeAuthority?.toBase58(), r.configPda.toBase58());
      assert.equal(acc.delegatedAmount, U64_MAX_BI);

      assert.isNull(await connection.getAccountInfo(r.pendingPda), "pinned blob closed, rent refunded to payer");

      const reg = await registryOf(merchant);
      assert.equal(reg.receiverCount, 1);
      assert.equal(reg.receivers[0].toBase58(), r.configPda.toBase58());
    });

    it("RM-HAPPY-1b: registration resolves Monad and Arbitrum to the configured domain ids", async () => {
      const monadRoute = await onboard(Keypair.generate().publicKey, chainNameSeed("MONAD"), Keypair.generate().publicKey.toBuffer());
      const arbitrumRoute = await onboard(Keypair.generate().publicKey, chainNameSeed("ARBITRUM"), Keypair.generate().publicKey.toBuffer());

      const monadCfg = await configOf(monadRoute.configPda);
      assert.equal(monadCfg.cctpDomainId, MONAD_DOMAIN);
      assert.isTrue(Buffer.from(monadCfg.cctpMintChain as any).equals(chainNameSeed("MONAD")));

      const arbitrumCfg = await configOf(arbitrumRoute.configPda);
      assert.equal(arbitrumCfg.cctpDomainId, ARBITRUM_DOMAIN);
      assert.isTrue(Buffer.from(arbitrumCfg.cctpMintChain as any).equals(chainNameSeed("ARBITRUM")));
    });

    it("RM-HAPPY-1c: a real cross-chain route to Ethereum stores domain 0, distinct from the same-chain sentinel", async () => {
      const sameChain = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      const ethereumRoute = await onboard(Keypair.generate().publicKey, chainNameSeed("ETHEREUM"), Keypair.generate().publicKey.toBuffer());

      assert.equal((await configOf(sameChain.configPda)).cctpDomainId, SAME_CHAIN_DOMAIN_SENTINEL);
      assert.equal((await configOf(ethereumRoute.configPda)).cctpDomainId, ETHEREUM_DOMAIN);
      assert.equal(ETHEREUM_DOMAIN, 0, "sanity: this test only proves anything if Ethereum's real domain is 0");
    });

    it("RM-HAPPY-2: many receivers under one merchant; registry follows registration order", async () => {
      const merchant = Keypair.generate().publicKey;
      const routes: Route[] = [];
      for (let i = 0; i < 3; i++) {
        const r = await prepare(merchant, ZERO_32, ZERO_32);
        await announce(r);
        routes.push(r);
      }
      await waitSlots(2);
      const order = [routes[2], routes[0], routes[1]];
      for (const r of order) await broadcastStored(r.pendingPda);

      const reg = await registryOf(merchant);
      assert.equal(reg.receiverCount, 3);
      order.forEach((r, i) => assert.equal(reg.receivers[i].toBase58(), r.configPda.toBase58()));
    });

    it("RM-HAPPY-3: two merchants get isolated registries", async () => {
      const a = Keypair.generate().publicKey;
      const b = Keypair.generate().publicKey;
      const ra = await onboard(a, ZERO_32, ZERO_32);
      const rb = await onboard(b, ZERO_32, ZERO_32);
      assert.notEqual(ra.registryPda.toBase58(), rb.registryPda.toBase58());
      assert.equal((await registryOf(a)).receiverCount, 1);
      assert.equal((await registryOf(b)).receiverCount, 1);
    });

    it("RM-SAD-1: replaying the same signed tx after it landed is rejected", async () => {
      const r = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await expectFail(sendRaw(r.regTx));
    });

    it("RM-SAD-2: idempotent — a SECOND validly signed registration for the same receiver cannot succeed or repeat effects", async () => {
      const merchant = Keypair.generate().publicKey;
      const r = await prepare(merchant, ZERO_32, ZERO_32, { keepKey: true }); // key kept ONLY for this test
      await announce(r);
      await waitSlots(2);
      await broadcastStored(r.pendingPda);

      const acc0 = await getAccount(connection, r.receiverTA);
      const cfg0 = await configOf(r.configPda);

      // Fresh signature against the nonce's new value, re-pinned, then broadcast.
      const second = await signRegTx(r, r.receiverKp!);
      await announce(r, second);
      await waitSlots(2);
      await expectFail(sendRaw(second)); // config already exists -> init fails

      assert.equal((await registryOf(merchant)).receiverCount, 1, "no duplicate registry entry");
      const acc1 = await getAccount(connection, r.receiverTA);
      assert.equal(acc1.delegate?.toBase58(), acc0.delegate?.toBase58(), "delegate not replaced");
      assert.equal(acc1.closeAuthority?.toBase58(), acc0.closeAuthority?.toBase58());
      const cfg1 = await configOf(r.configPda);
      assert.equal(cfg1.merchant.toBase58(), cfg0.merchant.toBase58());
    });

    it("RM-SAD-3: only the nonce authority (payer) can advance the nonce; neither a stranger nor the receiver key can pre-empt the stored tx", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32, { keepKey: true });
      await announce(r);
      await waitSlots(2);
      for (const attacker of [badActorKp, r.receiverKp!]) {
        const attack = new Transaction().add(
          SystemProgram.nonceAdvance({ noncePubkey: r.nonce, authorizedPubkey: attacker.publicKey }),
        );
        await expectFail(sendAndConfirmTransaction(connection, attack, [payerKp, attacker], { commitment: "confirmed" }));
      }
      await broadcastStored(r.pendingPda);
      assert.isNotNull(await connection.getAccountInfo(r.configPda));
    });

    it("RM-SAD-4: non-canonical receiver token account reverts NonCanonicalReceiverAta", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32, {
        regAccounts: async ({ receiver }) => ({ receiverTokenAccount: await newTokenAccount(usdtMint, receiver) }),
      });
      await announce(r);
      await waitSlots(2);
      await expectFail(broadcastStored(r.pendingPda), "NonCanonicalReceiverAta");
      assert.isNull(await connection.getAccountInfo(r.configPda));
    });

    it("RM-SAD-5: wrong-mint merchant reverts WrongMint", async () => {
      const bad = await newTokenAccount(wrongMint, Keypair.generate().publicKey);
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32, {
        regAccounts: { merchantTokenAccount: bad },
      });
      await announce(r);
      await waitSlots(2);
      await expectFail(broadcastStored(r.pendingPda), "WrongMint");
    });

    it("RM-SAD-6: register re-validates the route (signed args differ from announced) — CrossChainRequiresRecipient", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32, {
        regRoute: { chain: chainNameSeed("BASE"), recipient: ZERO_32 },
      });
      await announce(r);
      await waitSlots(2);
      await expectFail(broadcastStored(r.pendingPda), "CrossChainRequiresRecipient");
    });

    it("RM-SAD-7: staging account that is not ATA(config, mint) reverts WrongStagingAccount", async () => {
      const foreign = await newTokenAccount(usdtMint, badActorKp.publicKey);
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32, {
        regAccounts: { cctpBurnStagingAccount: foreign },
      });
      await announce(r);
      await waitSlots(2);
      await expectFail(broadcastStored(r.pendingPda), "WrongStagingAccount");
    });

    // Documents the known residual: the cap is only enforced at registration, so a registry
    // that fills between announce and broadcast fails the tx (and on-chain that burns the nonce).
    it("RM-SAD-8: registry full — the 33rd registration reverts MaxReceiversExceeded (known residual)", async () => {
      const merchant = Keypair.generate().publicKey;
      for (let i = 0; i < MAX_RECEIVERS_PER_MERCHANT; i++) await onboard(merchant, ZERO_32, ZERO_32);
      assert.equal((await registryOf(merchant)).receiverCount, MAX_RECEIVERS_PER_MERCHANT);

      const overflow = await prepare(merchant, ZERO_32, ZERO_32);
      await announce(overflow);
      await waitSlots(2);
      await expectFail(broadcastStored(overflow.pendingPda), "MaxReceiversExceeded");
      assert.isNull(await connection.getAccountInfo(overflow.configPda));
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  NONCE CLOSE — rent returns to the payer after registration
  // ═════════════════════════════════════════════════════════════════════════
  describe("Nonce close", () => {
    it("NC-HAPPY-1: after registration the payer withdraws the whole nonce back to itself", async () => {
      const r = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await waitSlots(2); // full withdraw fails in the block the nonce was advanced
      const lamportsInNonce = await connection.getBalance(r.nonce, "confirmed");
      assert.isAbove(lamportsInNonce, 0);
      const before = await connection.getBalance(payerKp.publicKey, "confirmed");

      const returned = await closeNonce(r.nonce);

      assert.equal(returned, lamportsInNonce);
      assert.isNull(await connection.getAccountInfo(r.nonce), "nonce account closed");
      assert.equal((await connection.getBalance(payerKp.publicKey, "confirmed")) - before, lamportsInNonce);
    });

    it("NC-HAPPY-2: an abandoned (announced, never registered) receiver's nonce is recoverable, and the dead blob can no longer register", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await announce(r);
      await waitSlots(2);
      await closeNonce(r.nonce);
      assert.isNull(await connection.getAccountInfo(r.nonce));
      await expectFail(broadcastStored(r.pendingPda));
      assert.isNull(await connection.getAccountInfo(r.configPda));
    });

    it("NC-SAD-1: a full withdraw in the SAME tx as AdvanceNonce fails (why the close is a follow-up tx)", async () => {
      // Rule: Agave durable-nonce docs + system program (full withdraw needs stored nonce != current).
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      const info = await connection.getAccountInfo(r.nonce, "confirmed");
      const nonceValue = NonceAccount.fromAccountData(info!.data).nonce;
      const tx = new Transaction();
      tx.feePayer = payerKp.publicKey;
      tx.recentBlockhash = nonceValue;
      tx.add(
        SystemProgram.nonceAdvance({ noncePubkey: r.nonce, authorizedPubkey: payerKp.publicKey }),
        SystemProgram.nonceWithdraw({
          noncePubkey: r.nonce,
          authorizedPubkey: payerKp.publicKey,
          toPubkey: payerKp.publicKey,
          lamports: info!.lamports,
        }),
      );
      tx.sign(payerKp);
      const hay = await expectFail(sendRaw(tx.serialize()));
      assert.match(hay, /0x7|"Custom":7|NonceBlockhashNotExpired/, `unexpected failure: ${hay.slice(0, 400)}`);
    });

    it("NC-SAD-2: only the nonce authority can withdraw", async () => {
      const r = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await waitSlots(2);
      await expectFail(closeNonce(r.nonce, badActorKp));
      assert.isNotNull(await connection.getAccountInfo(r.nonce), "nonce untouched");
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  SWEEP — SAME-CHAIN
  // ═════════════════════════════════════════════════════════════════════════
  describe("Sweep — same-chain", () => {
    let route: Route;

    const deposit = (amount: bigint) => mintTo(connection, payerKp, usdtMint, route.receiverTA, payerKp, amount);

    function sweepAccounts(f: Route, overrides: Record<string, PublicKey> = {}) {
      return {
        caller: callerKp.publicKey,
        callerTokenAccount: callerTA,
        receiverTokenAccount: f.receiverTA,
        factoryConfig: factoryConfigPda,
        treasuryTokenAccount: treasuryTA,
        receiverConfig: f.configPda,
        merchantTokenAccount: f.merchantTA,
        cctpBurnStagingAccount: programId,
        eventRentPayer: programId,
        messageSentEventData: programId,
        burnTokenMint: programId,
        cctpSenderAuthorityPda: programId,
        cctpDenylistAccount: programId,
        cctpMessageTransmitter: programId,
        cctpTokenMessenger: programId,
        cctpRemoteTokenMessenger: programId,
        cctpTokenMinter: programId,
        cctpLocalToken: programId,
        cctpEventAuthority: programId,
        cctpMessageTransmitterProgram: programId,
        cctpTokenMessengerMinterProgram: programId,
        ...overrides,
      };
    }

    function sweep(signer: Keypair = callerKp, accts: Record<string, PublicKey> = {}) {
      return program.methods
        .sweep()
        .accounts(sweepAccounts(route, { caller: signer.publicKey, ...accts }) as any)
        .signers([signer])
        .rpc();
    }

    async function drainReceiver() {
      if ((await tokenBalance(route.receiverTA)) > BigInt(0)) await sweep();
    }

    before(async () => {
      route = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
    });
    beforeEach(drainReceiver);

    it("SW-HP-1: caller/treasury split and merchant gets net", async () => {
      const amt = BigInt(1_000_000_000);
      const v0 = await tokenBalance(route.merchantTA);
      const c0 = await tokenBalance(callerTA);
      const t0 = await tokenBalance(treasuryTA);
      await deposit(amt);
      await sweep();
      const fee = calcFee(amt);
      assert.equal((await tokenBalance(route.merchantTA)) - v0, calcNet(amt));
      assert.equal((await tokenBalance(callerTA)) - c0, calcFeeToCaller(fee));
      assert.equal((await tokenBalance(treasuryTA)) - t0, calcFeeToTreasury(fee));
      assert.equal(await tokenBalance(route.receiverTA), BigInt(0));
    });

    it("SW-HP-2: multiple deposits then one sweep covers the total", async () => {
      const amounts = [BigInt(400_000_000), BigInt(300_000_000), BigInt(800_000_000)];
      for (const a of amounts) await deposit(a);
      const v0 = await tokenBalance(route.merchantTA);
      await sweep();
      assert.equal((await tokenBalance(route.merchantTA)) - v0, calcNet(amounts.reduce((a, b) => a + b, BigInt(0))));
    });

    it("SW-HP-3: sweep on an empty receiver is a no-op", async () => {
      const v0 = await tokenBalance(route.merchantTA);
      await sweep();
      assert.equal(await tokenBalance(route.merchantTA), v0);
    });

    it("SW-HP-4: sweep is permissionless", async () => {
      await deposit(BigInt(500_000_000));
      const v0 = await tokenBalance(route.merchantTA);
      const badTA = await newTokenAccount(usdtMint, badActorKp.publicKey);
      await sweep(badActorKp, { callerTokenAccount: badTA });
      assert.isTrue((await tokenBalance(route.merchantTA)) > v0);
    });

    it("SW-HP-5: repeated sweeps never double-charge", async () => {
      await deposit(BigInt(1_000_000_000));
      await sweep();
      const snap = await tokenBalance(route.merchantTA);
      for (let i = 0; i < 3; i++) await sweep();
      assert.equal(await tokenBalance(route.merchantTA), snap);
    });

    it("SW-HP-6: multi-tenant isolation", async () => {
      const b = await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await mintTo(connection, payerKp, usdtMint, b.receiverTA, payerKp, BigInt(250_000_000));
      const bMerchant = await tokenBalance(b.merchantTA);
      await deposit(BigInt(1_000_000_000));
      await sweep();
      assert.equal(await tokenBalance(b.merchantTA), bMerchant);
      assert.equal(await tokenBalance(b.receiverTA), BigInt(250_000_000));
    });

    it("SW-HP-7: funds deposited BEFORE registration are swept after it", async () => {
      const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      await announce(r);
      const amt = BigInt(600_000_000);
      await mintTo(connection, payerKp, usdtMint, r.receiverTA, payerKp, amt);
      await waitSlots(2);
      await broadcastStored(r.pendingPda);
      await program.methods.sweep().accounts(sweepAccounts(r) as any).signers([callerKp]).rpc();
      assert.equal(await tokenBalance(r.merchantTA), calcNet(amt));
    });

    describe("same-chain sad paths", () => {
      beforeEach(async () => {
        await drainReceiver();
        await deposit(BigInt(200_000_000));
      });

      it("SW-SAD-1: wrong receiver_token_account reverts WrongReceiver", async () => {
        const foreign = await newTokenAccount(usdtMint, badActorKp.publicKey);
        await expectFail(sweep(callerKp, { receiverTokenAccount: foreign }), "WrongReceiver");
      });

      it("SW-SAD-2: caller_token_account not owned by signer reverts WrongCaller", async () => {
        const other = await newTokenAccount(usdtMint, badActorKp.publicKey);
        await expectFail(sweep(callerKp, { callerTokenAccount: other }), "WrongCaller");
      });

      it("SW-SAD-3: wrong treasury reverts WrongTreasury", async () => {
        const fake = await newTokenAccount(usdtMint, badActorKp.publicKey);
        await expectFail(sweep(callerKp, { treasuryTokenAccount: fake }), "WrongTreasury");
      });

      it("SW-SAD-4: omitted merchant reverts MissingAccount", async () => {
        await expectFail(sweep(callerKp, { merchantTokenAccount: programId }), "MissingMerchantAccount");
      });

      it("SW-SAD-5: wrong merchant reverts WrongMerchant", async () => {
        const fake = await newTokenAccount(usdtMint, badActorKp.publicKey);
        await expectFail(sweep(callerKp, { merchantTokenAccount: fake }), "WrongMerchant");
      });

      it("SW-SAD-6: sweeping an announced-but-unregistered receiver fails: no config exists", async () => {
        const r = await prepare(Keypair.generate().publicKey, ZERO_32, ZERO_32);
        await announce(r);
        await expectFail(
          program.methods.sweep().accounts(sweepAccounts(r) as any).signers([callerKp]).rpc(),
          "AccountNotInitialized",
        );
      });
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  SWEEP — CROSS-CHAIN (validation only; real CPI needs cloned CCTP programs)
  // ═════════════════════════════════════════════════════════════════════════
  describe("Sweep — cross-chain", () => {
    let route: Route;

    before(async () => {
      route = await onboard(Keypair.generate().publicKey, chainNameSeed("STARKNET"), Keypair.generate().publicKey.toBuffer());
      await mintTo(connection, payerKp, usdtMint, route.receiverTA, payerKp, BigInt(1_000_000_000));
    });

    function ccAccounts(overrides: Record<string, PublicKey> = {}) {
      const eventKp = Keypair.generate();
      const accounts = {
        caller: callerKp.publicKey,
        callerTokenAccount: callerTA,
        receiverTokenAccount: route.receiverTA,
        factoryConfig: factoryConfigPda,
        treasuryTokenAccount: treasuryTA,
        receiverConfig: route.configPda,
        merchantTokenAccount: programId, // None sentinel: same-chain only field
        cctpBurnStagingAccount: route.stagingTA,
        eventRentPayer: payerKp.publicKey,
        messageSentEventData: eventKp.publicKey,
        burnTokenMint: usdtMint,
        // Present-but-arbitrary placeholders. `programId` is Anchor's "None"
        // sentinel for Option<Account>, so anything meant to resolve to
        // Some(...) here must be a distinct real pubkey, or SAD tests below
        // will trip MissingCctpAccounts on one of THESE instead of reaching
        // the check they're actually targeting.
        cctpSenderAuthorityPda: Keypair.generate().publicKey,
        cctpDenylistAccount: Keypair.generate().publicKey,
        cctpMessageTransmitter: Keypair.generate().publicKey,
        cctpTokenMessenger: Keypair.generate().publicKey,
        cctpRemoteTokenMessenger: Keypair.generate().publicKey,
        cctpTokenMinter: Keypair.generate().publicKey,
        cctpLocalToken: Keypair.generate().publicKey,
        cctpEventAuthority: Keypair.generate().publicKey,
        cctpMessageTransmitterProgram: CCTP_MESSAGE_TRANSMITTER_V2,
        cctpTokenMessengerMinterProgram: CCTP_TOKEN_MESSENGER_MINTER_V2,
        ...overrides,
      };
      return { accounts, extraSigners: [payerKp, eventKp] };
    }

    it("SW-CC-SAD-1: omitted CCTP accounts revert MissingCctpAccounts", async () => {
      const { accounts, extraSigners } = ccAccounts({ cctpBurnStagingAccount: programId });
      await expectFail(
        program.methods.sweep().accounts(accounts as any).signers([callerKp, ...extraSigners]).rpc(),
        "MissingCctpAccounts",
      );
    });

    it("SW-CC-SAD-2: wrong CCTP program id reverts WrongCctpProgram", async () => {
      // Must be a real, present pubkey that's simply wrong -- NOT `programId`,
      // which Anchor reads as "this optional account is None".
      const { accounts, extraSigners } = ccAccounts({ cctpTokenMessengerMinterProgram: SystemProgram.programId });
      await expectFail(
        program.methods.sweep().accounts(accounts as any).signers([callerKp, ...extraSigners]).rpc(),
        "WrongCctpProgram",
      );
    });

    it("SW-CC-SAD-3: staging account that is not ATA(config, mint) reverts WrongStagingAccount", async () => {
      const foreign = await newTokenAccount(usdtMint, badActorKp.publicKey);
      const { accounts, extraSigners } = ccAccounts({ cctpBurnStagingAccount: foreign });
      await expectFail(
        program.methods.sweep().accounts(accounts as any).signers([callerKp, ...extraSigners]).rpc(),
        "WrongStagingAccount",
      );
    });

    // Needs the real CCTP programs on the validator:
    //   solana-test-validator \
    //     --clone CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe \
    //     --clone CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC \
    //     --url mainnet-beta
    // plus the CCTP state accounts, and real derived addresses instead of programId placeholders.
    it.skip("SW-CC-HAPPY-1: cross-chain sweep burns net via CCTP and emits SweptCrossChain", async () => {
      const gross = await tokenBalance(route.receiverTA);
      const sig = await program.methods.sweep().accounts(ccAccounts() as any).signers([callerKp]).rpc();
      const ev = findEvent(await eventsOf(sig), "SweptCrossChain")!.data;
      assert.equal(ev.destinationDomain, STARKNET_DOMAIN);
      assert.equal(BigInt(ev.maxFee.toString()), calcMaxFee(gross));
    });
  });

  // ═════════════════════════════════════════════════════════════════════════
  //  INVARIANTS
  // ═════════════════════════════════════════════════════════════════════════
  describe("Invariants", () => {
    it("INV-1: fee + net = gross", () => {
      for (const a of [BigInt(1), BigInt(400), BigInt(1000), BigInt(999999), BigInt(1_000_000_000)])
        assert.equal(calcFee(a) + calcNet(a), a);
    });

    it("INV-2: caller + treasury = fee (no dust)", () => {
      for (const f of [BigInt(1), BigInt(2), BigInt(3), BigInt(10), BigInt(999), BigInt(1_000_001)])
        assert.equal(calcFeeToCaller(f) + calcFeeToTreasury(f), f);
    });

    it("INV-3: factory_config is immutable across announces/registrations", async () => {
      const before = await factoryCfg();
      await onboard(Keypair.generate().publicKey, ZERO_32, ZERO_32);
      const after = await factoryCfg();
      assert.equal(after.mint.toBase58(), before.mint.toBase58());
      assert.equal(after.treasuryTokenAccount.toBase58(), before.treasuryTokenAccount.toBase58());
      assert.equal(after.starknetDomain, before.starknetDomain);
      assert.equal(after.baseDomain, before.baseDomain);
      assert.equal(after.ethereumDomain, before.ethereumDomain);
      assert.equal(after.bump, before.bump);
    });

    it("INV-4: registry never exceeds the cap and every entry is a derivable config PDA", async () => {
      const merchant = Keypair.generate().publicKey;
      const r1 = await onboard(merchant, ZERO_32, ZERO_32);
      const r2 = await onboard(merchant, ZERO_32, ZERO_32);
      const reg = await registryOf(merchant);
      assert.isAtMost(reg.receiverCount, MAX_RECEIVERS_PER_MERCHANT);
      assert.equal(reg.receivers[0].toBase58(), deriveConfig(r1.receiver)[0].toBase58());
      assert.equal(reg.receivers[1].toBase58(), deriveConfig(r2.receiver)[0].toBase58());
    });

    it("INV-5: CCTP max_fee is computed off gross at 3 bps", () => {
      for (const b of [BigInt(10_000), BigInt(1_000_000), BigInt(999_999_999)])
        assert.equal(calcMaxFee(b), (b * BigInt(3)) / BPS_DENOM_BI);
    });
  });
});
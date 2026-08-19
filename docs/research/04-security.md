# Security, Malware & Failure Modes (research, Aug 2026)

## ⚠️ Direct warning about the tools this project might otherwise have copied

`Cetipoo/solana-onchain-arbitrage-bot` (263★) is the open-source repo of the **same
operator** behind SolanaMevBot — the commercial bot benchmarked in `00-key-numbers.md`.
Independent review flags it as **read-only reference, do not run**:

- config takes a **base58 private key in plaintext TOML**
- no on-chain program source — it CPIs a **third-party program you cannot audit**
- a binary `lib/` directory committed
- `solana-sdk 1.17` (ancient), `.DS_Store` committed
- README funnels to a "full featured production bot" + Discord

That is the classic lead-gen shape. It does not prove the commercial product is
malicious, but "paste your private key into a TOML so it can CPI a closed program" is
disqualifying as a pattern to imitate — **and it is the pattern this project must not
reproduce.**

## Repos worth reading

| Repo | Lang | ★ | Verdict |
|---|---|---|---|
| `buffalojoec/arb-program` | Rust | 182 | **Best profit-or-revert teaching reference** (Anza engineer). Old deps; copy the on-chain pattern, not the versions. |
| `jito-labs/searcher-examples` | Rust | 431 | Authoritative block-engine auth + bundle submission. |
| `jito-labs/mev-bot` | TS | 1198 | Canonical backrun architecture. Read, don't run. |
| `rpcpool/yellowstone-grpc` | Rust | 988 | Essential. Actively maintained. ⚠️ **AGPL-3.0** — check licence implications. |
| `Shyft-to/solana-defi` | Rust | 352 | Best practical gRPC consumer/decoder examples. No licence file. |
| `0xNineteen/solana-arbitrage-bot` | Rust | 807 | Abandoned (2023, Serum-era) but instructive routing/pool math. |
| `lamports-dev/richat` | Rust | 146 | Lower-latency geyser stream/fanout alternative. |
| `0xfnzero/*` (solana-streamer, sol-parser-sdk, sol-trade-sdk) | Rust | 177/41/333 | Active, MIT, useful — but **anonymous maintainer**; vendor and diff-audit before signing anything with them. |

**Assume hostile** (Telegram-only contact, referral links, "proof" wallet screenshots,
star counts with no contributor history): `WSOL12/Solana-Arbitrage-Bot`,
`katlogic/solana-arbitrage-bot`, `adams322111233221/solana-mev-bot`,
`SaoXuan/rust-mev-bot-shared`.

## Documented supply-chain attacks against exactly this use case

- **`@solana/web3.js` compromise (Dec 2024).** Versions **1.95.6 / 1.95.7**, published
  2 Dec 2024 after a maintainer's npm token was phished. Injected `addToQueue()`
  exfiltrated private keys to `sol-rpc.xyz` behind fake CloudFront headers. ~$190k
  stolen. Fixed in **1.95.8**. **Impact was concentrated in bots/backends holding raw
  keys** — precisely this project's profile.
- **`solana-pumpfun-bot`** (SlowMist, Jul 2025). Fake stars/forks; `package-lock.json`
  rewritten to fetch `crypto-layout-utils` from an **attacker-controlled GitHub release
  URL**, bypassing npm registry scanning. Scans disk for wallet files, POSTs keys out.
- **npm typosquats** (Socket, Jan 2025): `@async-mutex/mutex`, `dexscreener`,
  `solana-transaction-toolkit`, `solana-stable-web-huks` — exfiltrate via
  **smtp.gmail.com** to evade egress filtering; two auto-drain ~98% of balance.
- **crates.io** (Sept 2025): `faster_log@1.7.8`, `async_println@1.0.1` cloned
  `fast_log`'s README, worked as real loggers, scanned source for Solana/ETH key
  patterns. 8,424 downloads before removal.
- **JFrog "Solana FakeFix"**: 25 npm/PyPI packages posing as patched builds, riding the
  panic after the web3.js incident.

## 10-point audit checklist — run before executing any third-party repo

1. **Commit archaeology** — `git log --format='%ad %an %s'`. Uniform timestamps, single
   author, no issues/PRs, stars ≫ contributors ⇒ fabricated.
2. **Lockfile provenance** — grep lockfiles for `resolved` URLs that are *not*
   `registry.npmjs.org` / `crates.io` (GitHub release tarballs, raw URLs, bare IPs).
3. **Dependency name diff** — check for one-character / word-order squats
   (`fast_log` vs `faster_log`) and scoped lookalikes.
4. **Grep the tree** for `Keypair.fromSecretKey`, `secretKey`, `id.json`, `nodemailer`,
   `smtp`, network calls to non-RPC hosts, `child_process`, `eval`,
   `Buffer.from(…,'base64')`, `atob`, long hex/base64 blobs, `process.env` dumps.
5. **Install hooks** — `preinstall`/`postinstall`/`prepare`, `build.rs`, `setup.py`.
6. **No binaries** — reject committed `.so`/`.exe`/`.node`/minified-only `lib/`. If the
   on-chain program isn't in the repo as source, you cannot verify it.
7. **Verify the on-chain program** — take the CPI'd program ID, check a verified-build
   explorer, read what it does with `profit_receiver`/authority accounts.
8. **Where do keys go?** Plaintext base58 key in TOML/.env is disqualifying on its own.
9. **Social smell** — Telegram/Discord as primary support, referral links, "sample profit
   tx", "DM for the full version", missing licence.
10. **Detonate first** — throwaway VM/container, **no wallet files present**, egress
    logged, keypair holding 0 SOL. Watch for any outbound connection that isn't your RPC.

## Key management

**Never put a Phantom seed phrase in a bot.** A BIP-39 seed derives every account on
every chain, forever, and cannot be rotated. A bot needs exactly one Ed25519 keypair.
Anything requesting 12/24 words is malicious or written by someone not worth copying.

Design rules adopted for this project:

- **Burner hot wallet only.** Fresh keypair used solely by the bot; never touched by a
  browser wallet; never given authority over a program, ATA, or multisig.
- **Capped balance.** Fund only the configured working capital. That number is the
  maximum acceptable loss.
- **Hardcode the profit destination inside the on-chain program**, pointing at a cold
  address. Then a stolen hot key costs the gas float, not the profits. This is the single
  highest-leverage security decision available and it is nearly free to implement.
- **Windows storage: DPAPI.** Encrypt the 64-byte secret with
  `CryptProtectData`/`ProtectedData.Protect(CurrentUser)` — ciphertext is bound to the
  Windows account, so a copied file is useless elsewhere. Rust: `keyring` crate (Windows
  Credential Manager, DPAPI-backed) or the `windows` crate directly. Alternative:
  `age`/`sops` keyfile with passphrase entered at process start.
- **In process:** hold in `secrecy::Secret` / `zeroize::Zeroizing`, never log, never
  `Debug`-print the keypair, disable crash dumps, keep BitLocker on.
- **Separate keys per concern:** fee payer, profit receiver (cold), RPC credentials.

## Token-level traps

| Trap | Breaks arb how | Detection |
|---|---|---|
| **Transfer fee (T22)** | received ≠ `amount_out`; off-chain profit check passes, tx reverts or eats fee twice | mint owner == Token-2022 ⇒ unpack `TransferFeeConfig`, use `calculate_epoch_fee` |
| **Transfer hook (T22)** | CPI to arbitrary program per transfer: missing accounts ⇒ failed tx; CU blowup; hook can deny non-whitelisted senders (a working honeypot) | `TransferHook` extension ⇒ **skip token** unless hook program allowlisted |
| **Permanent delegate (T22)** | issuer can transfer/burn from your ATA — clawback after you buy | `PermanentDelegate` ⇒ hard blacklist |
| **Non-transferable / default-frozen** | buy but cannot sell; ATA arrives frozen | `NonTransferable`, `DefaultAccountState=Frozen` |
| **Freeze authority live** | issuer freezes your ATA mid-arb; funds locked. Most common Solana honeypot | classic mint layout `freeze_authority != None` ⇒ blacklist |
| **Mint authority live** | infinite mint ⇒ pool drained, your quote is fiction | `mint_authority != None` |
| **Spoofed symbol** | route through fake "USDC" | **match by mint address only, never symbol**; cross-check Jupiter `verified` |
| **Thin/fake liquidity** | quote fills at size, real pool doesn't | reserve floors, holder concentration |

**Detection stack:** parse the mint yourself (`StateWithExtensions::<Mint>::unpack` +
`get_extension_types`) against an **allowlist of extension types — deny unknown by
default**, since new extensions ship regularly. Enrich with Helius DAS `getAsset` and
Jupiter Tokens API V2 (`lite-api.jup.ag/tokens/v2`, `verified` tag).

**Enforce twice:** off-chain to filter candidates, and **on-chain in our own program** as
a profit-or-revert assertion on *actual post-transfer balances*. The on-chain check is
the only one a transfer hook or fee cannot lie to.

## Testing stack

| Tool | Use |
|---|---|
| **LiteSVM** (629★, active) | **Primary.** In-process SVM; inject real mainnet pool account bytes; unit-test swap math and multi-instruction arb flows. Default in Anchor 1.0. |
| **Mollusk** (Anza, 299★) | Single-instruction isolation + **CU benchmarking** — catches compute regressions that cause landing failures. |
| **surfpool** (584★, active) | **Mainnet-fork integration testing** — real pool state at a real slot. Replaces `solana-test-validator`. |
| `solana-bankrun` | superseded by LiteSVM TS bindings (same author) |

**Pipeline:** Mollusk/LiteSVM for math + CU → surfpool fork for realistic state →
mainnet `simulateTransaction` (`replaceRecentBlockhash: true`, `sigVerify: false`) as the
final pre-send gate → on-chain profit-or-revert as the actual safety net.

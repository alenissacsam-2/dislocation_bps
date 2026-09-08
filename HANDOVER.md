# Handover

For whoever picks this up next — a fork, a fresh session, or me in a week.

Read this before touching anything. The most useful thing in it is not the
architecture; it is the list of ways this instrument has already lied, and the one
way it is probably lying right now.

---

## 1. What this is, and what it has concluded

A **measurement instrument** for Solana AMM arbitrage, running against live mainnet. It
is not a money-maker and the measurements say it cannot become one at this capital.

**As of 2026-08-30 it can also trade.** Live execution was built on request: swap
encoders for Orca Whirlpool and Raydium CLMM, transaction assembly, and a risk-gated
submission path. Nothing about the measurement changed — the edge is still negative
against the 2 bps cost floor, and §4's fee-tier question is still open. What changed is
that the instrument is now capable of acting on a number it says not to act on. Invariant
1 in §8 is the thing to read before touching any of it.

The answer it has produced, in three numbers:

| | |
|---|---|
| Cheapest round trip available | **2 bps** (was 50 before multi-venue) |
| Median opportunity, at the size that maximises it, at **any** capital | **$0.0013** |
| Max lifetime of every opportunity worth more than $0.10 | **0 slots** |

That last row is the finding the whole project rests on, and it was measured, not
argued. Opportunity size and opportunity lifetime run in **opposite** directions:

```
whole pie          episodes   avg life   longest   capital needed
under $0.001         17,312       0.2s      8.8s              $1
$0.001 - $0.01          574       0.4s      7.6s             $20
$0.01 - $0.10           107       0.1s      3.6s             $59
$0.10 - $1                8       0.0s      0.0s            $930
over $1                   8       0.0s      0.0s          $2,274
```

Sub-cent gaps loiter for up to 22 slots. Every one of the sixteen worth more than
$0.10 was gone before the next slot began — maximum, not average, with no exceptions.
There is no size at which an opportunity is both worth taking and still there when you
arrive. That answers "just add capital" and "just go faster" with data.

*(Table is 6.6 h from `cryptobot-pre-cyclekey.db`. Those rows predate
`Cycle::canonical_key`, so the **episode counts** are roughly doubled by the mirror bug
in §4 — one loop logged under both its entry points. Lifetimes, capital figures and the
ordering are unaffected, since they are per-episode rather than counts. The shape has
held on the run since.)*

Corollaries already established, so you do not have to re-derive them:

- **Flash loans do not help.** Profit is unimodal in trade size; past the optimum,
  borrowing more earns less. Leverage moves capture from ~13% of a $0.0013 pie to
  ~100% of a $0.0013 pie.
- **Cross-chain does not work**, structurally. Same-chain arbitrage is atomic — a bad
  fill reverts. Cross-chain cannot be, and the fastest USDC bridge is 8–20s, during
  which SOL moves ~4 bps (1σ) against 2.6 bps of available dislocation.
- **Foundry/Solidity are irrelevant.** Nothing on the viable path touches an EVM chain.
- **Latency was never the constraint.** Sweep time is ~7 ms against a 400 ms block.

Full write-up: `docs/research/06-multi-venue-measurement.md`. Earlier research in the
same directory, numbered in order.

---

## 2. Running it

**This builds and runs natively on Windows as of 2026-08-23.** The old instruction here
said "build and run from WSL, not Windows" — that was true of *this machine*, which had
no MSVC linker, and was never true of the code. With Visual Studio Build Tools
installed the whole tree compiles for `x86_64-pc-windows-msvc`, `rusqlite` with bundled
SQLite included. WSL is no longer a dependency of this project, and with it go the VM's
CPU overhead and the failure mode where the VM restarts and silently takes the run with
it.

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\cryptobot-win-target"
cargo build --release -p cb-bot -p cb-desk
```

**Keep the target directory out of the repo.** `./target` is where a bare `cargo build`
writes, and anything looking elsewhere for the binary will then run a stale one without
saying so. The WSL arrangement had the same hazard and solved it in `scripts/env.sh`;
on Windows, set `CARGO_TARGET_DIR` as above.

The normal way to run it is **`cryptobot-desk.exe`** — the application starts, stops,
configures and observes the bot, and reads the ledger whether or not anything is
running. See §9.

Headless, or for the reports:

```powershell
cb-bot                          # run it directly
cb-bot --report                 # read the ledger without stopping the run
cb-bot --verify                 # audit decoders against an independent router
```

> **Run `cb-bot` from the repository root and nowhere else.** It creates
> `cryptobot.db` in the working directory. Started from the wrong directory it makes a
> second, empty ledger and cheerfully records into that instead — which reads exactly
> like a run that found nothing. This has already happened once, from a shell left in
> `crates/desk`.

The API is on `http://127.0.0.1:8787` while running: `/api/health`, `/api/stream`,
`/api/equity`. It no longer serves a UI.

`config.toml` — `mode` and `dry_run`. **The measurements do not justify moving either.**

`mode = "live"` arms execution: it loads a key, builds routes, signs, and simulates
against live state. `dry_run = false` is what lets the last step happen. They are
separate on purpose, and both default to safe. Arming live also needs
`CRYPTOBOT_ALLOW_LIVE=1` in the environment and the wallet passphrase fed to `cb-bot`'s
stdin, which `cryptobot-desk` does — a `cb-bot` started by hand in live mode blocks
waiting for one.

```powershell
cb-verify-encode                      # check the encoders against mainnet; no key, no funds
cb-verify-encode --as <address>       # and the account order, using a public address
```

Run the first before trusting anything under §8 invariant 1b, and the second before
setting `dry_run = false`.

### The installer, and the two shapes of install — added 2026-08-24

`scripts\installer.ps1` produces a per-user NSIS installer. Three things about it are
load-bearing and none are obvious:

**The root is resolved, not assumed.** `Paths::discover` takes a saved choice, then a
checkout found by walking up from the executable and the working directory, then
`%LOCALAPPDATA%\cryptobot`. *Both* markers are required to call something a checkout —
`config.toml` **and** `crates/` — because `config.toml` alone also describes the data
directory. Installed, the app seeds a paper-mode config into that data directory on
first launch (`Paths::ensure_ready`, from a `include_str!` of `config.example.toml`).
The Parameters tab prints whichever root it settled on, because a ledger whose location
is a guess is a ledger nobody can go and check.

**The data directory is not beside the executable, deliberately.** An installer puts the
binaries somewhere the running account cannot write. A ledger that cannot be opened for
writing is a run that records nothing while every panel still looks healthy.

**`externalBin` is merged in only at bundle time.** `cb-bot.exe` ships as a Tauri
sidecar so `Paths::bot_exe` finds it beside the app. Declaring that in `tauri.conf.json`
makes a staged, triple-suffixed binary a precondition of *every* `cargo build` and
`cargo test` in the workspace — CI included, which is how it was first discovered. The
declaration therefore lives in `crates/desk/tauri.installer.conf.json` and is passed as
`cargo tauri build --config tauri.installer.conf.json`.

### CI does not gate on `cargo fmt` — and that is on purpose

The tree has never been fmt-clean. At rustfmt's default width about 290 files differ; at
`max_width = 100` about 70 still do, and those disagree with *each other* — some lines
want joining that a narrower setting split, others want splitting that a wider one
joined. It was hand-laid at inconsistent widths over time. `rustfmt.toml` records the
closest width so new code does not drift further, but gating CI on it would fail every
pull request on unrelated files. Running `cargo fmt --all` once is reasonable; it just
deserves its own commit rather than riding inside someone else's.

---

## 3. Layout

```
crates/core       money math. amm.rs (constant product, ppm fees), clmm.rs
                  (concentrated-liquidity identity + tick bounds), path.rs (Leg,
                  optimal_input, marginal_edge_bps), types.rs (PoolState, PoolMath)
crates/dex        one module per venue, pure decode functions, no network
crates/scanner    PoolStore, Snapshot, multi.rs (cycle enumeration + Cycle::canonical_key)
crates/feed       websocket account subscriptions
crates/ledger     SQLite. sweeps (every sample), paper_fills (every clearing cycle),
                  episodes()/survival() collapse detections into opportunities
crates/bot        live.rs (venue dispatch, sweep, USD index, reconcile), main.rs
                  (event loop, --report, --verify), registry.rs (embedded pools.json)
crates/server     event bus + three API endpoints. No UI since 2026-08-23.
crates/desk       the Windows application. runner.rs (process control behind one
                  trait), config.rs (toml_edit, so the file's prose survives an edit),
                  archive.rs, history.rs (cb-ledger read-only), paths.rs, ui/
scripts           build.ps1 (Windows build), installer.ps1 (the NSIS bundle),
                  build_registry.py (regenerates the pool registry).
                  env.sh/build.sh/run-forever.sh are the WSL-era scripts and are kept
                  only because build_registry.py is still driven from a shell.
.github/workflows ci.yml (clippy + tests on windows-latest), release.yml (builds the
                  installer and attaches it to a tag)
```

**The central mathematical fact**, in `crates/core/src/clmm.rs`: a concentrated-liquidity
pool inside its current tick is *exactly* a constant-product pool over virtual reserves
`x = L/√P`, `y = L·√P`. That is why five venues share one quote engine and there is no
second implementation to disagree with the first. Anything new should be expressed
through that identity if it possibly can be.

### Venues currently decoded

| Venue | Program | Notes |
|---|---|---|
| Orca Whirlpool | `whirLbMi…` | self-contained; adaptive-fee pools rejected |
| Raydium CLMM | `CAMMCzo5…` | fee lives in a shared `AmmConfig` account |
| Raydium CP-Swap | `CPMMoo8L…` | three fee buckets to subtract — see §4 |
| Raydium AMM v4 | `675kPX9M…` | reserves in two vaults, 3 subscriptions each |
| Meteora DAMM v2 | `cpamdpZC…` | concentrated **without ticks**; `liquidity` is L·2⁶⁴ |

`crates/dex/src/pumpswap.rs` is a **complete, tested decoder that is deliberately not
live.** No registry pool uses it, and `live.rs` rejects a PumpSwap entry loudly rather
than quoting one. Wiring it needs three things and none are done: registry pools, vault
subscriptions in the feed, and a `--verify` pass against an outside router before a
single number from it is believed (invariant #3). It is kept rather than deleted because
the decode work is finished and correct; what is missing is the trust, not the code.

Registry is generated by `scripts/build_registry.py` and embedded at compile time via
`include_str!`, so a measurement's exact pool universe is traceable to a commit.
Currently 90 pools / 104 subscriptions / 29 mints.

---

## 4. How this instrument has lied before

Read this section twice. Every one of these passed all its own tests.

**The phantom $400 (CP-Swap creator fees).** Raydium added `creator_fees_token_0/1` to
CP-Swap in bytes that used to be padding. Not subtracting them left 6.888 SOL counted
as tradable reserve, and the scanner reported the difference as a 68 bps arbitrage
standing open for four hours. Mints were clean, fresh RPC reads reproduced it exactly,
every internal check passed — because the arithmetic was right and the input was wrong.
Caught only by quoting the same swap through Jupiter, where it was worse in *both*
directions, which is impossible for a real dislocation.

> **A decoder that errs in the profitable direction produces numbers that look like the
> project working.** Every other class of bug announces itself. This one flatters you.
> Errors that flatter need an adversary, not more of your own tests.

**One gap counted 65,000 times.** The sweep re-detects a standing gap 5×/second, and
summing every detection called one gap "$105/hour". It was $0.43/hour. Fixed by
collapsing detections into episodes valued at their single best moment.

**One loop counted twice.** `SOL → USDC → SOL` and `USDC → SOL → USDC` over the same
two pools are one closed loop entered at two points; taking it at either entry removes
it from both. Keying episodes on the printed route double-counted 14,408 slots' worth.
Fixed by `Cycle::canonical_key()` — the sequence of (pool, input mint) rotated to its
smallest form, so entry point falls out while direction, which genuinely matters, does
not.

**SOL at $32.** The registry priced concentrated pools from their balance ratio. A
ranged pool holds its two tokens in a ratio set by where spot sits *inside its range*,
not by the price. Price comes from `sqrt_price` now.

**A layout off by 2⁶⁴.** DAMM v2 stores `liquidity` as L·2⁶⁴ where Orca and Raydium
store L. Caught because the account carries `token_a_amount`/`token_b_amount`
independently, so the reconstruction could be checked against something the decoder had
not used.

**A rate with nothing behind it, reported as an edge.** The board ranked cycles by
marginal rate — profit per unit at infinitesimally small size — while depth was
measured on the entry pool only. A cycle whose *downstream* leg sat at the end of its
tick therefore showed an enormous rate over almost no capacity, and led the
leaderboard for hours at 1156 bps without ever producing a fill. Nothing was
mis-decoded and no arithmetic was wrong; the instrument was answering a different
question from the one its label claimed. Caught by asking why the biggest number on
the screen never appeared in `paper_fills`, and confirmed by the giveaway that depth
*fell* as the reported edge rose. Fixed in §5.1.

> **Two searches that answer different questions must not share one label.** The
> arithmetic being right does not make the number mean what the heading says.

**Edge that may grow with the fee you pay to reach it — open, evidence conflicting.**

On the 17.85 h run archived as `cryptobot-20260823-193303.db`, the *edge* — profit after
fees, the number that decides everything — rose sharply with the route's fee tier:

```
route fee     fills    mean edge
under 5 bps  16,067      2.16 bps
5-20          7,136      2.44
20-50         1,145      3.66
over 50       1,429     11.95
```

93% of that run's claimed value sat above 5 bps of fees, while the cheap, liquid,
genuinely-arbitraged tier produced $3.12 of the $44.55 a $100 book was told it could
reach. That ordering is backwards for real arbitrage, and it is the reason the headline
profit from that run should not be believed.

**It has not reproduced.** The run started 2026-08-23 14:03 UTC, on the same code plus
`slot_spread` and with `max_hops` raised 3 → 4, reads flat: 1.75, 1.84, 1.83 bps across
the first three tiers over ~8,000 fills. Only `over 50` is elevated (7.54 bps) and it has
138 fills, six of them simultaneous — which is not evidence of anything.

So there are two runs disagreeing, and the difference is unexplained. **This is the open
question.** Do not close it on either run alone.

**What was ruled out along the way.**

*A tautology in the reporting.* `dislocation_bps` is **defined** as `edge_bps + fee_bps`
(`main.rs:514`). "Dislocation rises with fee" is therefore arithmetic and evidence of
nothing. Anyone re-deriving this must use `edge_bps`; the tables above do. An earlier
pass through this data did not, and reported the tautology as a finding.

*Time skew as the whole story.* 82% of loops price their two legs from **different
slots** — `MAX_STALE_LAG_SLOTS` admits a pool 1800 slots (~6 min) behind the head, and
`Cycle::slot()` reports only the stalest leg without anything rejecting the loop. So part
of a reported edge can be the market moving between two observations rather than two
venues disagreeing, and that part cannot be traded. `Cycle::slot_spread` measures it per
loop and `--report` prints two sections built on it.

The measured cost of requiring simultaneity, per fee tier, is small so far: −9%, −6%,
+11%. Real but not the explanation for a 5× gradient. **Note the trap:** comparing
*spread bands* directly looks flat and appears to exonerate timing, because each band
mixes fee tiers. The fee tier has to be held fixed. An earlier pass made that mistake in
both directions within an hour.

> **`--verify` structurally cannot settle any of this.** It checks one pool against a
> router at one instant, in one direction. It returned **118 checked, 0 faults** while the
> archived run's gradient sat in the same data. A clean decoder audit is evidence about
> decoders, not about the arithmetic built on top of them.

Until it is explained, **value concentrated in high-fee routes is unproven** — and on the
archived run that is most of the value the instrument reported. What settles it is a long
run on the current build, then `--report`'s two simultaneity sections read together.

**The 3-strike breaker halts on ordinary market movement, not just on defects — found
2026-08-31.** At `slippage_bps = 1` — already the tightest floor the maths allows for a
~2 bp measured edge over two hops — a live run hit `simulation rejected:
{"InstructionError":[6,{"Custom":6018}]}` three times and halted. Instruction index 6 is
the second swap; 6018 is Raydium CLMM's `TooLittleOutputReceived`. The cycle was
profitable when detected. By the time the built, signed transaction reached simulation
— one sweep of latency later — the second leg's real price had moved past a floor with
essentially no slack, and the simulation correctly refused rather than filling badly.

The breaker's own justification, stated in `risk.rs`, is that `Failed` covers outcomes
that "mean something is wrong that retrying will not fix." A slippage-floor miss at a
1 bp tolerance does not meet that bar — retrying can straightforwardly succeed the next
time the market sits still for one round trip, which is not evidence of a defect in
what was built. Treating it as `Failed` anyway means the breaker fires on the ordinary
cost of running with zero slack, which is most of the time at this edge size, and a run
can spend its whole session halted having built nothing that was actually wrong.

No code changed to fix this — reclassifying by matching a bare, log-free error code was
considered and rejected: `Custom(6018)` is confirmed here by cross-referencing this
session's own earlier `cb-verify-encode` run, but a wrong guess at a different Anchor
user-error number would silently turn a real encoder defect into a shrugged-off `Missed`
for years before anyone caught it, and this codebase already carries a working
`classify()` for exactly this ambiguity in `cb_executor::verify` — one built against
logs the live path does not have, and not yet worth wiring in for one confirmed number.
`max_consecutive_failures` was raised from 3 to 6 instead: enough tolerance for a
transient miss or two before assuming something structural, not so much that a real
defect goes uncaught. This is a config choice, not a fix, and revisit it if the reason
in the log ever stops being 6018.

**`Custom(6036)` — and the error-table mistake that named it wrong, corrected
2026-09-08.** This section previously said 6036 was Raydium CLMM's `Insufficient
liquidity for this direction`, claiming it was verified rather than guessed. It was
neither: the code was matched against the wrong program's table. Once the simulation's
own logs were carried through (see below), the chain named it directly:

```
{"InstructionError":[5,{"Custom":6036}]} — AnchorError occurred.
Error Code: AmountOutBelowMinimum. Error Number: 6036.
Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc failed
```

`whirLb…` is **Orca Whirlpool**, not Raydium, and 6036 there is `AmountOutBelowMinimum`.
So 6018 and 6036 are not two different failures at all — they are the same failure at
two venues: the output floor written into the instruction was not met. The lesson is the
one this file already gave two paragraphs up and then did not follow: an Anchor user
error number means nothing without the program that raised it, because every program
numbers its own from 6000. Reading it off the wrong table produced a plausible,
confident, wrong story about liquidity that survived a week.

**A halt had no reachable resume path at all, and sat idle for 6+ hours — found
2026-09-01.** `RiskGate::resume()` (`crates/executor/src/risk.rs`) existed but nothing
in the whole app ever called it outside a unit test: no API route, no desk UI control,
no CLI flag. The comment above `halt()` even said the lack of auto-recovery was
deliberate ("whatever caused it is still true until someone has looked") — true in
principle, except there was no *way* for someone to say they'd looked, short of killing
and restarting the whole `cb-bot` process. That accidental restart-as-resume is why
every restart this session appeared to "fix" a halt: it wasn't fixing anything, it was
building a fresh `RiskGate` with `halted: None`. Confirmed live: a burst of five
`Custom(6018)`s tripped the (already-raised) 6-strike breaker at `16:02:39 UTC` on
2026-08-31, and the bot was still halted, having done nothing but log reconcile
heartbeats, when this was found at `22:23:49 UTC` — over six hours later.

Fixed by adding `RiskGate::tick_auto_resume()` and a new `halt_cooldown_secs` limit
(config default 600s = 10 minutes; `0` disables it and restores the exact old
behavior). Called from three places: once per sweep unconditionally (so the UI's
notion of halted state clears on schedule even on a sweep that finds no candidate),
and at the top of `Trader::attempt()` before any RPC work (so a halt, while it stands,
costs nothing — previously every sweep still paid for a full account fetch, tick-array
resolution and a blockhash only to be refused at the very last step). `resume()` itself
is untouched and still the deliberate manual override; the cooldown is not a claim that
whatever tripped the breaker is fixed, only a bound on how long the instrument sits
idle before it is allowed to find out by trying again. Given how thin this book's
opportunities are (see `--report`: 246 distinct opportunities in 2.2h, 0 taken), expect
it to sometimes halt, cool down, and halt again on the same underlying condition — that
repeated cycling is the correct behavior for a structural mismatch, not a bug in the
cooldown.

**The refusal cooldown was skipping most of what it should have taken — found
2026-09-01.** The 20s per-cycle cooldown added the day before (to stop a wrap-shortfall
refusal re-deriving itself once a second, forever) was applied to *every* refusal
including simulation rejections. Those are the opposite case: `Custom(6018)` means the
price moved between the quote and the simulation, which describes one instant and says
nothing about the next slot. Twenty seconds is ~50 slots, and the opportunities being
waited out live 0.1-5s.

Measured over one 3.9h live run, on the subset that was genuinely takeable — encodable
venues, `slot_spread <= 1` so not a stale-price artifact, net-positive after the modeled
tip — **1,586 of 2,064 detections (77%) were skipped by a cooldown earned by a price
that had already moved.** Fixed by classifying the refusal (`refusal_cooldown_for`):
price-moved reasons get 1.5s, structural ones (a size that cannot fit the wallet) keep
20s, and a halt gets none at all — it is one fact about the gate, and holding every
cycle seen during it left them all still held for the window *after* the halt lifted,
exactly when they should have been retried. Log throttling was split out into its own
map on the long window: how often a thing is *said* and how often it is *retried* were
never the same question. Retrying is free to be wrong here — `Plan::execute` simulates
before it submits, so a rejected retry costs a round trip and nothing on chain.

**Where the money actually is, at $10 — measured 2026-09-01.** Same run, deduplicated to
distinct opportunities, `slot_spread <= 1`, net of the modeled tip:

| | count | total net |
|---|---|---|
| encodable + simultaneous (takeable today) | 11 | $0.0585 |
| **blocked by a missing encoder** | 9 | **$0.2353** |

Behind each missing encoder: **RAY-V4 $0.1794**, RAY-CP $0.0878, MET-D2 $0.0102. The
single best opportunity of the run was `RAY-V4 25bp · ORCA 16bp`, net $0.1070 at
`slot_spread 0`, and it cannot be built. So the largest available lever at this book
size is not capital and not latency — it is a **Raydium AMM v4 swap encoder**, which
would roughly 4x the takeable money on this sample. Note the concentration caveat before
believing the ranking: the biggest blocked names are memecoin pairs (Fartcoin, USELESS),
and one route was 79% of the sample.

**Pools that hold liquidity and cannot be traded — found 2026-08-30.** Building the swap
encoders required, for the first time, asking the chain for the accounts a *trade* needs
rather than the ones a *quote* needs. That found 21 of the 48 Raydium CLMM pools in the
registry with **no tick arrays at any of the 24 addresses swept in both directions**,
while reporting non-zero liquidity in the pool account. A concentrated-liquidity pool
cannot be swapped through without a tick array. These pools are not tradeable by anyone,
and the registry has been feeding them to the scanner as though they were.

Twenty of the twenty-one are harmless: they are `STA/ST` and `ST/STB` pairs whose mints
never appear in a pool with a base mint, so no cycle from SOL, USDC or USDT can reach
them and none ever entered a measurement. **The twenty-first is not.** `WSOL/ALNOOR` on
Raydium CLMM has no tick arrays and *does* pair with a `WSOL/ALNOOR` CP-Swap pool, which
makes a two-hop cycle the scanner can enumerate, price, and record as an opportunity —
through a leg no transaction could have executed.

> **The pool account says `liquidity: 35720662117822` and the pool cannot be swapped.**
> Every decoder check passes, because the decoder is reading the field correctly. The
> field simply does not mean what "there is liquidity here" means. Whether a venue can
> be *quoted* and whether it can be *traded* are separate questions, and this codebase
> had only ever asked the first.

This does not overturn the headline — one pool out of ninety, on an exotic pair. It is
here because it is the same shape as the other seven and because the fix is not a code
change: the registry needs a tradeability filter, and nothing currently applies one.

**The fee-tier gradient was stale prices — resolved 2026-08-31.** §5's open question is
closed, and the answer is the unflattering one. Measured over a 1,241,443-opportunity
archive (`cryptobot-20260831-132830.db`), grouping by `slot_spread` — how many slots
apart the two legs of a cycle were observed:

| slot spread | opportunities | avg edge | total value |
|---|---|---|---|
| 0 (simultaneous) | 185,052 | 1.96 bp | $141.12 |
| 1–4 | 349,738 | 1.80 bp | $226.58 |
| 5–19 | 331,308 | 1.86 bp | $103.02 |
| 20–99 | 245,998 | 2.21 bp | $114.98 |
| **100+** | **109,374** | **5.76 bp** | **$521.55** |

**8.8% of opportunities produced 46% of the value, and their defining feature is that
their legs were minutes apart.** Of $1,128.99 reported, **$141.12 — 12.5% — came from
legs observed in the same slot.** The other 87.5% is a price from now compared against a
price from before.

The single most valuable venue pair in the archive, `RAY-CL 25bp · RAY-CL 5bp` at
$122.96, is one cycle repeated: `SOL → RAY → SOL`, edge 33.19 bp, **slot spread 502** —
about 3.3 minutes between observing the two legs. Of the thousand most valuable
opportunities, 102 had simultaneous legs.

And the fee gradient itself, restricted to simultaneous legs, largely collapses: 1.51,
1.98, 3.66, 2.83 bp across the four fee bands, against 2.16 → 11.95 unrestricted. The
mechanism is now obvious in hindsight: **an expensive pool is a thinly traded pool, a
thinly traded pool updates rarely, and a stale cached price compared against a fresh one
shows a gap that nobody can trade.** Fee was never the cause; it was a proxy for
staleness.

> **The earlier investigation measured this and dismissed it.** §5 recorded clock skew
> as "real but far too small — −9%, −6%, +11% by tier". That measured the *average* cost
> of requiring simultaneity across all opportunities, where the 91% with fresh legs swamp
> the 9% without. Conditioning on the spread instead of averaging over it reverses the
> conclusion. The number was right and the question it answered was the wrong one.

**Consequences.** The instrument's own headline is now known to be ~87% artifact, and the
honest figure is the simultaneous-leg subset: about 2 bp of average edge against a 2 bp
floor — break-even before tips. This also settles the venue-expansion question in the
negative: adding thinly traded venues adds stale pools, and stale pools are what
manufactured the phantom edge in the first place.

### 4.10 — The floor was measured in a coarser unit than the market — **fixed 2026-09-08**

Fifteen hours of live trading, on the build that finally re-prices against fresh state
before building anything. 2,494 detections were simultaneous, encodable, on cheap-fee
venues and positive after costs. **Eighteen reached simulation. Zero filled.**

The reason is not the market. Every cycle that reached route-building carries its own
answer in the refusal text — `spends X and guarantees only Y` — so the detected edge
and the re-priced edge can be compared directly, 157 times:

| detected − re-priced | min | p25 | median | p75 | max |
|---|---|---|---|---|---|
| bps | 0.37 | 7.00 | 12.21 | 19.50 | 74.01 |

And it sorts almost perfectly by the route's own fee, which is the finding:

| round-trip fee | attempts | median re-priced edge | still positive |
|----------------|----------|-----------------------|----------------|
| under 4 bps    | 26       | −1.69 bps             | **6 (23%)**    |
| 4 – 7 bps      | 47       | −4.50 bps             | 0              |
| 7 – 10 bps     | 25       | −10.77 bps            | 0              |
| 20 – 40 bps    | 37       | −11.44 bps            | 2 (5%)         |
| over 40 bps    | 22       | −17.30 bps            | 0              |

A route that costs 3 bps needs a 3 bp disagreement, and those are permanently available
because nobody else can profit from them either. A route that costs 30 bps needs a 30 bp
disagreement, which on a liquid pair is a transient somebody faster already took, and on
an illiquid one is not a disagreement at all — it is a dead pool whose price drifted and
stayed there. Confirmed from outside: the Raydium CLMM 100 bp WSOL/USDC pool sat at
103.4636 for 25 consecutive seconds without a single `sqrt_price` change while the Orca
4 bp pool moved ten times, a standing 70 bp gap against a 104 bp round trip.

**Eight cycles re-priced *positive*** — +0.05, +0.09, +0.17, +1.49, +1.61, +1.69, +1.71
and +1.93 bps — **and every one was refused.** `slippage_bps` was `1`, an integer, so two
hops cost 2 bps of floor against an edge under 2. The route-build invariant `s < e / n`
was being honoured against an `e` this market does not offer. The unit was the bug: a
floor cannot be measured in a coarser grain than the market it protects.

Three more found in the same pass:

- **A tip that is never paid was charged on every cycle.** There is no transfer to a tip
  account anywhere in `cb-executor`, and `priority_micro_lamports` is 0 — yet the gate
  subtracted `JITO_TIP_FLOOR_SOL × sol_price` = $0.00077 against a median believable
  opportunity of $0.00044. 879 cycles were declined as "net negative after tip" while
  being positive against the fee the wallet actually pays. Competition is real, but it is
  not a cost: a race lost is caught in simulation and never submitted.
- **The refusal cooldown was still sized for the old failure mode.** 1500 ms suppressed
  1,056 of the 2,494 takeable detections. That was correct when a refusal meant a revert
  and a breaker strike; it stopped being correct when re-pricing moved ahead of building.
- **`not attempted — one trade per sweep, or no encoder`** pooled a queue with a missing
  encoder. They ask for opposite things and could not be told apart in the ledger.

**Consequences.** With the floor at 0.3 bps/hop, an execution ceiling of 4 bps of route
fee, the phantom tip gone and the cooldown at 400 ms, the same fifteen hours contain
**180 moments** clearing every gate, worth **$0.36 gross / $0.27 after base fees** —
median $0.00148 against a $0.00051 fee, spread across all 15 hours, entirely on
`RAY-CL 1bp ↔ ORCA 2bp` and `RAY-CL 1bp ↔ RAY-CL 2bp` SOL/stable round trips. At the
23% survival rate the cheap band actually measured, that is a few cents a day on $9.20 —
which is the first time this instrument has had a number that is small rather than fake.

### 4.11 — The capital ladder is flat because the quote stops at one tick — **found 2026-09-08, open**

With the floor, tip, cooldown and fee ceiling fixed, 230 moments over 19.1 hours clear
every gate — 12.0 an hour. Their capital ladder:

| book | gross/day | net of the 5,000-lamport fee | median per trade |
|---|---|---|---|
| $9.20 (today) | $0.584 | $0.436 | $0.00128 |
| $100 | $0.935 | $0.787 | $0.00137 |
| $1,000 | $0.935 | $0.787 | $0.00137 |
| $10,000 | $0.935 | $0.787 | $0.00137 |

**Identical from $100 up.** Every rung above $100 is capped by the same thing, and the
optimal sizes say what it is: median $8, p75 $18, **max $26** across all 230.

The first explanation reached for was the model. [`clmm::capacity_for_input`] refuses to
quote past the current tick interval by design, so the suspicion was that the cap is an
artifact of that conservatism and real depth is far larger. **It is not.** Measured on
chain, at one moment, for the pools these cycles run through — one tick is 1 bps:

| pool | spacing | a whole tick holds | quotable at this instant |
|---|---|---|---|
| RAY-CL 1bp WSOL/USDC | 1 (1 bps) | **$4.48** | $3.36 USDC / $1.12 SOL |
| RAY-CL 2bp WSOL/USDC | 1 (1 bps) | **$39.77** | $0.59 USDC / $39.18 SOL |
| RAY-CL 5bp WSOL/USDC | 10 (10 bps) | $291.60 | $92.21 / $199.46 |
| ORCA 4bp SOL/USDC | 4 (4 bps) | $54,503 | $27,969 / $26,537 |

The Orca side is deep and never the constraint. The cheap Raydium pools — the only ones
whose round trips ever survive re-pricing — hold **four to forty dollars per basis
point of price**. Multi-tick quoting would not unlock size, because crossing ticks *is*
moving the price: pushing $100 through the 1 bp pool walks it about 22 ticks, which is
22 bps, against a dislocation of one to three. The optimal sizes this instrument
already reports — median $8, max $26 — are the correct answer to "where does impact eat
the edge", not a bound imposed by the model.

**So the ceiling is real and it is the market's.** $0.44/day net at $9.20, $0.79/day
with unlimited capital, and roughly a quarter to a half of that once the fill rate is
applied: **ten to forty cents a day.** More capital does not help and neither does a
better depth model. The dislocation sits open for seconds precisely *because* it is too
small for anyone to be paid for taking it, and that is also why it is available to us.

What is left, in order of what the measurements support:

1. **Fill rate.** 23% of attempts on this band survived re-pricing. Halving the round
   trips (§4.10's follow-up) should raise it; nothing else measured will.
2. **More cheap pairs — and there are almost none to have.** Checked against the venue
   directories: of 744 pools listed, exactly **seven pairs** anywhere on Solana offer a
   sub-4-bps encodable round trip, and the registry already carried all seven. Lowering
   the TVL floor from $150k to $25k adds not one more. Six of the seven have exactly two
   pools, so they contribute one round trip each and there is nothing to add.

   SOL/USDC is the exception and the busiest, and a $125k floor admits an Orca 2 bp pool
   at $133k that takes it from one cheap combination to three. Together with dropping
   the 20 `STA/ST`/`ST/STB` pools that reach no base mint, the registry now offers **18
   directed cheap round trips against 12**, at 106 subscriptions against 104. That is
   the whole of the expansion available.

   Four of the seven produce **nothing**: `SOL/JitoSOL`, `USX/USDC`, `JupUSD/USDC` and
   `USD1/USDC` have zero simultaneous detections in 19 hours, because their two pools
   agree to 0.00 bps. `SOL/mSOL` produces 117 detections whose best is $0.00009. Every
   moment worth having comes from SOL/USDC and SOL/USDT.

3. **Not the stablecoin triangle.** `SOL → USDC → USDT → SOL` costs 3 bps on three 1 bp
   pools and is enumerated: it led the sweep 3,333 times with edges up to 71.84 bps. It
   has never once become an opportunity, and the reason is not a bug. Its edge is
   positive only 12.5% of the time with a median of −0.93 bps; when it is strongly
   positive the moment is slot-skewed (the 71.84 bps reading sat beside two-hop rows at
   the same second with slot spreads of 107, 129 and 473); and a three-leg cycle needs
   all three legs to have capacity in the right direction at once, which on
   tick-spacing-1 stable pools parked at a boundary is often zero. The two-hop view of
   the same dislocation is cheaper and deeper every time.

4. Nothing else. Venue expansion adds thin pools, which §4.9 already showed manufacture
   phantom edge rather than real edge.

### 4.12 — Three walls, none of which wrote a log line — **fixed 2026-09-08**

Nothing had ever been submitted. Not rejected on chain — **never sent**: `grep -c
"submitted "` over the entire log returns 0, and the wallet's last five signatures are
all funding transfers from July and August. Six hours of the current build produced
143,877 detections, 91 attempts, 19 simulations and no transaction.

Three separate blocks, each sufficient on its own, and none of which announced itself.

**1. A lost race counted as a defect.** `Outcome` draws the line in its own doc comment
— `Missed` is "lost the race, costs nothing but time", `Failed` is "rejected for a
reason that suggests a defect rather than competition" — and `config.example.toml` says
it again: *"A lost race does not count: losing races is what racing is."* `Plan::execute`
recorded every simulation rejection as `Failed`. That was right while floors came from
detection-time quotes; since `hops_for` began re-pricing against fresh state a missed
floor means the price moved in one round trip, which is competition exactly. Losing is
the normal outcome, so six in a row arrived fast and halted trading for the full
ten-minute cooldown, repeatedly — **372 halt announcements**.

**2. The wallet had no token account for USDT.** A cycle's profit is read from the
owner's lamport balance and must clear `pre_balance + guaranteed gain`. Opening an ATA
costs 2,039,280 lamports out of that same balance, so a trade that opens one is asked to
show a **twenty-one cent** gain on a cycle worth a tenth of a cent — short by a factor
of **383**, every time. The USDC account existed; USDT did not; roughly seventy per cent
of everything that cleared the other gates ran SOL↔USDT.

**3. The output floor could not cover the transaction fee.** Each hop spends what the
one before it *guaranteed*, so the route ends holding the last hop's quote having
promised that quote less **one** haircut, not `n` of them. On a wSOL cycle the fee
leaves the same balance the profit is read from, so that single haircut is the entire
margin. At $9.20 the fee is 5,000 lamports against 89,000,000 of input: the haircut must
be at least **0.56 bps** and the shipped value was **0.3**. Every trade that survived to
the profit check failed it — and silently, because a balance short of its floor is not
an error the chain reports, it is a comparison this code makes and rejects.

A constant cannot serve: the floor is pushed down by `n·s < e` and up by the fee. It is
chosen per trade now, the widest the edge will bear.

**That simulation charges the fee at all was measured, not assumed** — §4.3's rule
applied to this codebase's own arithmetic. A one-signature transaction was simulated
against mainnet with `sigVerify: false` and the fee payer requested in the accounts
array: it came back at 132,741,877 lamports against a real balance of 132,746,877,
exactly 5,000 lighter. Worth checking in both directions: had simulation been fee-free,
demanding the headroom would have refused good trades for no reason at all.

Two more found in the same audit:

- Routes were refused outright when a leg's tick held less than the plan asked for —
  **35 of 91 attempts**, all on cycles that were profitable and merely too big. Sized
  down now, with the USD figures scaled to match.
- Cycles entered at USDC or USDT — **195 of 232** — asked to spend nine dollars of a
  token the wallet does not hold. They would have failed as insufficient funds, counted
  as defects, and pushed the run toward a halt. Refused now, which frees the sweep's one
  attempt for the same loop entered from SOL.

Replayed over the ledger: **1,368 SOL-entered moments** clear every gate in nineteen
hours, of which **997** carry twice the fee in margin.

The pattern in all twelve: **an internal check cannot catch an error in what the code
believes about the outside world** — including what it believes its own numbers mean.
Three of the four blocks above were arithmetic this code did to itself and then declined
to explain. A refusal that does not say *which* number failed is a refusal nobody can
act on, and four of them stacked end to end look exactly like a quiet market.
Every new decoder must be pinned against a value the decoder itself did not produce,
every headline must name which search produced it, and every threshold must be stated in
a unit finer than the thing it is thresholding.

---

## 5. Open items, ranked

### 5.1 — The headline edge was a rate nobody could trade — **fixed 2026-08-22**

Design: `docs/superpowers/specs/2026-08-22-reporting-integrity-design.md`.

The old run reported `best edge 1156.44 bps` on `SOL → TRUMP → USDC → SOL`, held for
7+ seconds. It never became a fill — the largest TRUMP entry in `paper_fills` is
12.7 bps — because two different searches ran each sweep and the report read the wrong
one. `survey_from_base` prices *every* cycle at infinitesimal size; `find_from_base`
returns only cycles with a feasible size, and only those ever become fills. A cycle
with a downstream leg parked at its tick boundary has an enormous marginal rate and
almost no capacity, so it topped a board ranked on rate alone. The signature was in
the data — depth *fell* as the reported edge rose, backwards from a real dislocation:

```
band            sweeps   avg depth $
under 50 bps      6,494      1,838.72
50-200 bps           85        913.28
over 200 bps         51        156.14
```

Depth was also computed wrong: first leg only, when the binding constraint is usually
downstream.

**What changed.**

- `cb_core::path::cycle_depth_base` computes the bottleneck across *every* leg,
  converting each leg's capacity back to base units through the marginal rates ahead
  of it. It is a first-order upper bound, so `optimal_input` always sizes at or under
  it — the right direction for a depth figure to err.
- The sweep now publishes two named numbers. `Sweep::tradeable` is the best edge among
  cycles whose depth clears the tradable capital, and is the headline everywhere:
  status event, report, histogram, clearing rate. `Sweep::best` keeps the raw marginal
  maximum as an explicit diagnostic. When nothing qualifies, tradeable is `None` and
  renders as `—`, never as zero.
- `clearing` now requires depth as well as a positive edge, and is counted over every
  cycle priced rather than over the truncated leaderboard (which silently capped it
  at 12).
- The leaderboard renders two groups, *tradeable now* above *rate only*. Both stay
  visible; nothing untradeable leads.
- The gap between the two searches is reported rather than smoothed away — the report
  prints how often the leading rate had no size behind it, because that number
  measures how much of the visible book is untouchable at this capital.

Ledger columns added: `tradeable_edge_bps`, `tradeable_dislocation_bps`,
`tradeable_fee_bps`, `tradeable_depth_usd`, `tradeable_route`, `stale_excluded`,
`depth_measured`. Rows written before this cannot tell *nothing was tradeable* from
*we never looked*, so `depth_measured = 0` excludes them from tradeable statistics
instead of counting them as zeros; `--report` says so at the top when it opens such a
ledger.

### 5.2 — Stale state between reconciles — **fixed 2026-08-22**

For an AMM, *no update means no change* — the account only moves when someone swaps
it. That makes a silently dropped subscription indistinguishable from a quiet pool
from the stream alone. `reconcile()` repaired it every 180 s; nothing excluded it from
the sweep in the meantime, and `PoolStore::stale_pools()` had no caller at all.

`PoolStore::snapshot_fresh(max_lag)` now builds each sweep's snapshot without pools
lagging the newest slot by more than `MAX_STALE_LAG_SLOTS`, and returns the count
excluded. It surfaces as `Sweep::stale_excluded` → status event →
`sweeps.stale_excluded` → a dashboard tile, and warns on change.

**The threshold was set by measurement, and the first guess was wrong.** At 300 slots
(~1–2 min) the guard looked prudent and was actively harmful: 37% of sweeps dropped
~40 of 84 pools and cycles priced fell from ~1260 to ~600. The reason is that
`reconcile()` already re-reads every account every 180 s and refreshes its slot whether
or not it traded — so a tight guard mostly excludes pools the last reconcile *just
proved correct*, for the crime of being quiet since. Losing a third of the cycle graph
understates every rate the instrument reports, and does it invisibly, which is a worse
failure for a measurement than the extra staleness it was buying against a bound
reconcile already holds.

It is now 1800 slots — above the reconcile cadence — so it fires only once reconcile
has itself stopped repairing, the one failure with no other backstop. In normal running
it excludes nothing, which makes a non-zero `stale_excluded` a real signal instead of
routine noise. The two constants live in different files, so
`the_staleness_guard_does_not_fire_while_reconcile_is_working` in `main.rs` fails if
anyone tightens one without the other.

Stated plainly because it matters: slot lag **cannot** tell a quiet-but-correct pool
from a dropped-subscription-and-wrong one. It bounds how long an unrepaired quote can
survive; it does not prove anything right.

That guard has one hole it structurally cannot cover — if the feed dies completely
every pool ages together and nothing ever looks stale. Only a wall clock catches that,
so ledger recording now pauses when feed data age exceeds `FEED_STALL_SECS` (5 s).
Sweeps continue for the dashboard, labelled stalled. A measurement that knows its
clock has stopped does not go on writing numbers.

### 5.2b — `--verify` faults now name who served the quote — **fixed 2026-08-22**

51 checked, 5 faults, clustered at +21 to +25 bps on pools that previously passed
clean. Jupiter now serves routes labelled `Aquifer` and `Flux` — RFQ market-maker
liquidity, not the AMM pool being asked about — and the audit's premise ("better than
the router ⇒ we are wrong") assumed the router quotes AMM liquidity. Against that, a
direct cross-check of SOL/USDC across three independent programs agreed with Jupiter
to under 1 bps, so the decoders are not uniformly biased.

`jupiter_quote` now parses `routePlan[].swapInfo.{label, ammKey}`. Each row prints
which venues actually served it, and faults split three ways:

- the router routed through **our own pool** (`ammKey` match) and still paid less —
  the strongest evidence a decode fault can produce, counted as a fault;
- the route touched a venue we decode — counted as a fault, as before;
- the route touched nothing we watch — counted separately as *off-premise* and
  reported as "inspect by hand", never as a pass and never as a fault.

An unrecognised venue label counts as not-ours, which only ever downgrades a fault to
"inspect this" — never the reverse.

**Still open:** re-run `--verify` against the new build and attribute those 5 faults
concretely. The classifier makes them explainable; it has not yet explained them.

### 5.3 — Run longer

Everything measured so far is hours, not weeks. Nothing here says how the edge
distribution behaves through a real volatility spike. The supervisor exists precisely
so this can run unattended.

### 5.4 — Housekeeping

**Done 2026-08-23.** `crates/evaluator` and `crates/executor` were one-line
placeholders on nothing's dependency path and are deleted. `crates/dex/src/pumpswap.rs`
is kept, with its status stated explicitly in §3 — a finished decoder awaiting registry,
feed and audit work, not an unfinished one.

---

## 6. Explicitly scoped out

**Meteora DLMM** — the last large un-decoded venue, and deliberately not built. Three
independent reasons, any one sufficient:

1. **Its cheapest tier is 1 bps on SOL/USDC — exactly what Raydium CLMM already
   provides.** It cannot lower the fee wall.
2. **Its fee is not the fee it stores.** DLMM adds a volatility surcharge on top of the
   base rate. 52 of 102 live SOL/USDC pairs were carrying a non-zero volatility
   accumulator at the moment I checked. Quoting the base fee would understate cost,
   which overstates profit — the exact error class that produced the phantom $400.
   Pricing it honestly means tracking the volatility state.
3. **Capacity lives elsewhere.** The active bin's reserves are in a `BinArray` account
   that *changes as price moves*, so it needs dynamic subscription management the feed
   does not have.

If someone does build it: the layout is already located. `token_x_mint@88`,
`token_y_mint@120`, which pins `StaticParameters(32) + VariableParameters(32)` and
therefore `base_factor@8`, `variable_fee_control@16`, `volatility_accumulator@40`,
`active_id@76`, `bin_step@80`, `status@82`. Account length 904. Base fee is
`base_factor × bin_step × 10 / 1e9`. Price is `(1 + bin_step/10000)^active_id`.

DLMM is also mathematically different — constant-*sum* within a bin, so zero slippage
until the bin is exhausted. That fits `Leg` better than it looks: for a constant-sum
leg the marginal rate is `γ · R_out/R_in` with the reserves read as a price ratio,
which is structurally identical to the constant-product case. `is_profitable()` and
`marginal_edge_bps()` would need no change; only `quote()` would branch.

**Going live.** Not blocked on engineering. Blocked on the measurement, which says the
expected value is a fraction of a cent per opportunity against a per-attempt cost that
does not shrink. India VDA tax makes it worse: 30% flat on gains, no loss set-off, no
carry-forward, which penalises high-churn strategies specifically.

---

## 7. Ledger files

| File | What it holds |
|---|---|
| `cryptobot.db` | current run — first with the tradeable/marginal split |
| `cryptobot-pre-tradeable.db` | ~10h, correct fills, but every `sweeps` edge in it is a marginal rate (§5.1) |
| `cryptobot-pre-cyclekey.db` | ~7h, correct decoders, but mirror-double-counted opportunity counts |
| `cryptobot-contaminated-by-cpmm-bug.db` | kept as the record of what the phantom looked like |

All gitignored. `sweeps` is the valuable table — it records what the market looked like
whether or not anything cleared, which is what turns "found nothing" into a
measurement. A run that records only its wins can report the size of a win but never
the odds.

Two schema migrations matter when reading an old file. `paper_fills.cycle_key` was
added later, and `episodes()` falls back to the printed route for rows predating it.
`sweeps.depth_measured` marks rows written after the tradeable/marginal split; rows
without it are excluded from every tradeable statistic rather than counted as zeros,
and `--report` says so at the top when it opens one. Both are why each run was archived
at the change rather than continued — old and new rows in one file would group and
aggregate by different rules, and nothing downstream would show it.

`scripts/archive-ledger.sh <name>` does the move, taking the `-wal` and `-shm` with it.
A SQLite database in WAL mode is three files, and copying only the `.db` silently
leaves the most recent writes behind.

---

## 8. Invariants — do not break these

1. **Live execution exists. Read this before changing anything under it.**
   **Rewritten 2026-08-30**, for the second time in one day, because the thing the
   previous version rested on is gone.

   That version said: *`cb-bot` depends on neither `cb-executor`, `cb-wallet`, nor
   `solana-sdk` — that binary contains no code that can sign. This is the strongest
   property in the workspace and the only one that holds regardless of any config, any
   environment variable, and any mistake in the application.* It also said that adding
   one of those dependencies was the change that needed justifying.

   All three are now dependencies of `cb-bot`, and `crates/bot/src/execute.rs` builds
   real swaps. **The strongest property in the workspace has been deleted on request.**
   Nothing brings it back except deleting that module, and no amount of care in what
   follows is equivalent to it — the difference between "cannot" and "will not" is the
   whole of what was lost. Say so plainly to whoever asks next.

   What carries the guarantee now, in the order a mistake meets it. None of these is
   absolute and all of them are checkable:

   - **Two switches, owned by different parties.** `mode = "live"` in the config, which
     the application writes, **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment, which
     it deliberately does not set. `main.rs` refuses to start half-armed rather than
     warning and continuing in paper.
   - **A passphrase that arrives on stdin at spawn.** The key is encrypted at rest, and
     a live config on its own loads nothing and signs nothing. Not a config value, not
     an environment variable, not a file beside the key — stdin is the only channel that
     closes after start-up and never appears in a process listing. The consequence is
     deliberate: `cryptobot-desk` must feed it, and a `cb-bot` started by hand in live
     mode blocks waiting for one.
   - **`dry_run`, defaulting to true**, and separate from `mode` so that arming
     execution and spending money are two decisions. A config that does not mention it
     is a dry run; `cb-core` has a test that fails if that stops being true.
   - **The risk gate**, checked before the chain is asked anything. A non-positive size
     is refused, which is not a formality: the first version of `Trader::attempt` passed
     zero and was refused at the gate before it ever reached mainnet, and every unit
     test passed while it was broken.
   - **Simulation against live state, every time, with the profit read from the
     resulting balance** rather than from the quote. `Plan::execute` has no path that
     submits without simulating first. This is what makes an unverified account order
     survivable — a wrong instruction fails in simulation and costs a round trip.
   - **The route's two floor invariants** (`crates/executor/src/route.rs`): each hop's
     output floor must cover the next hop's input, and the last must exceed the first's
     input. A route violating either refuses to encode. So a transaction that lands is
     profitable by construction, enforced by the AMM programs rather than by this
     codebase's arithmetic.

   `cb_desk::config::EXECUTION_IMPLEMENTED` is now `true` and no longer carries a safety
   claim; it says only that the machinery exists, which is why `set_mode` stopped
   refusing on it and started checking the environment switch instead.

   Note that `Config::load` merges `Env::prefixed("CRYPTOBOT_")` over the file, so
   `CRYPTOBOT_MODE=live` and `CRYPTOBOT_DRY_RUN=false` both override `config.toml` and
   the application cannot prevent it. The Mode panel reports the effective mode and says
   when the environment is winning.

1b. **What is verified — completed 2026-08-30.** `cb-verify-encode --as <funded
   address>` now runs clean over the whole registry:

   | check | pass | fail |
   |---|---|---|
   | vault offsets | 154 | 0 |
   | tick-array derivation | 53 | 0 |
   | Orca `swap` account order | 29 | 0 |
   | Raydium `swap`, bitmap included | 27 | 0 |
   | Raydium `swap`, bitmap omitted | 27 | 0 |

   The account order is no longer an open question. Getting there needed the probe to
   create the simulating address's token accounts itself — without that the program
   stopped at position 3 of 11, on an account the address did not hold, and everything
   after it was untested. It needs ~0.00204 SOL of rent per account, which is why this
   could not be answered until the wallet was funded.

   Two things fell out of it:

   - **`BitmapPolicy` is settled: both work.** 27 pools accept the tick-array bitmap
     extension and the same 27 accept its absence. The uncertainty documented in
     `venue/raydium.rs` is resolved by measurement — either is valid.
   - **Raydium panics on a duplicated tick array.** `ticks::resolve` repeats the last
     live array when fewer than three exist, which Orca accepts and Raydium does not:
     it borrows each remaining account mutably and dies with
     `already mutably borrowed: BorrowError`, an SBF panic that costs the transaction.
     The encoder now passes distinct arrays only. This would have broken every Raydium
     swap on a pool with a thin array set, and no test could have found it.

   The 21 skipped swaps are the 21 pools with no tick arrays at all — the correlation is
   exact, and they are untradeable by anyone rather than mis-encoded.

   **What is still not established is the arithmetic of a trade.** These probes swap a
   token the address does not hold, so they prove the instruction is well formed and
   stop at the balance. Whether a cycle is profitable is a different question, and only
   a funded dry run answers it.

2. **Every priced number comes from chain.** Venue APIs are a directory only. No
   hardcoded price of anything, SOL included — the USD index walks the pool graph out
   from USDC/USDT and nothing else is assumed to be a dollar.
3. **A new decoder is not trusted until something outside it agrees.** Pin it against a
   field the decoder did not use, or an independent router, or another venue's price
   for the same pair. Preferably all three.
4. **Refuse rather than extrapolate.** A quote past a tick's capacity, an adaptive-fee
   pool, a Token-2022 mint, a fee schedule — decline it. Overstating profit is the only
   error direction that loses money.
5. **Keep `--verify` one-sided.** Being *worse* than the router is fine and expected;
   being *better* is the fault. The check exists because errors that flatter need an
   adversary.
6. **An opportunity is an episode, not a detection.** And one loop is one opportunity
   however many mints you could enter it at.

---

---

## 9. The application

`crates/desk` — `cryptobot-desk.exe`. Design:
`docs/superpowers/specs/2026-08-23-cryptobot-desk-design.md`.

It exists because the dashboard was **served by the process it observed**. With the bot
down there was no server, so there was no interface at all, and a WSL restart that
killed an overnight run announced itself only when someone thought to ask. The app
inverts the dependency: the app is durable, the bot is a child process it supervises,
and the ledger is read from disk with `cb-ledger` in-process — so history renders with
nothing running.

What it does: start/stop, live telemetry over the same WebSocket, the history panels,
`config.toml` editing, ledger archiving, a log tail, and a tray icon that shows run
state without opening anything.

Three things in it are load-bearing and should not be casually undone:

1. **It refuses to start into a bound port 8787.** Two processes writing one SQLite
   ledger corrupts the measurement rather than duplicating it. A port held by something
   this app did not start reads as `Foreign`, and the Stop button stays disabled —
   it will not kill a process it cannot identify.
2. **Changing a trading parameter archives the run.** §7: rows recorded under different
   parameters aggregate by different rules and nothing downstream reveals the mixture.
3. **There is no control that can set `mode = "live"`.** Invariant #1 is enforced by
   the absence of a mechanism, not by a dialog. `config.rs` writes four keys and no
   others.

Auto-restart of a dead bot is opt-in and **off by default**, and it fires on `Failed`
only, never `Stopped` — a run stopped deliberately stays stopped, and a resurrected one
is a new run rather than a continuation.

`cryptobot-desk.exe --start` begins the run on launch, which is what makes "launch with
Windows" mean anything. `--no-tray` exists for bisecting event-loop problems.

### The one that cost an hour: blocking the main thread

**Every Tauri command that touches disk, a socket, or a process must be `async` and do
its work in `tauri::async_runtime::spawn_blocking`.** Synchronous commands run on the
main thread — the thread pumping the window's event loop. `read_history` walks every
sweep in the ledger to build episodes, which takes tens of seconds, and doing that
inline pinned a core and froze the window for the whole of startup.

It presented as a spin in the event loop, and cost a four-way bisection (tray off, bot
off, release build, blank page) to find. The blank page was what proved it: 0.1% CPU,
so the fault was in the frontend's calls rather than the loop itself. `routes.rs`
already had the same lesson written into it for the same reason, and it was not carried
over. It is written here so the third occasion is cheaper than the second.

Related: the ledger's WAL had grown to 172 MB without checkpointing, because a
long-lived reader blocks it. `PRAGMA wal_checkpoint(TRUNCATE)` with the bot stopped
folds it back in. That is housekeeping, not the cause of the freeze — the read is slow
because of the episode query, not the file size.

---

*Last updated 2026-08-23. 259 tests passing, clippy clean under `-D warnings`.
Runs natively on Windows; WSL no longer required. `crates/executor` and
`crates/evaluator` deleted — there is no execution code, and an empty crate with a
confident name is worse than none.
Test names are sentences on purpose — `one_standing_gap_is_one_opportunity_not_a_thousand_trades`
is the specification, and the assertion is the proof.*

# Handover

For whoever picks this up next — a fork, a fresh session, or me in a week.

Read this before touching anything. The most useful thing in it is not the
architecture; it is the list of ways this instrument has already lied, and the one
way it is probably lying right now.

---

## 1. What this is, and what it has concluded

A **measurement instrument** for Solana AMM arbitrage, running in paper mode against
live mainnet. It has never signed a transaction and has no key material. It is not a
money-maker and the measurements say it cannot become one at this capital.

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

`config.toml` — `mode = "paper"`. **Leave it there.** Nothing in `crates/executor`
should be given keys on the strength of these measurements.

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
crates/evaluator, crates/executor   present, not on the measurement path
scripts           env.sh, build.sh, run-forever.sh, build_registry.py
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

`crates/dex/src/pumpswap.rs` exists but is not wired into `live.rs` and no registry
pool uses it. Either finish it or delete it; right now it is neither.

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

**Two prices that were never on screen together — under investigation 2026-08-23.**
`MAX_STALE_LAG_SLOTS` admits a pool up to 1800 slots (~6 min) behind the head, and
`Cycle::slot` reports the *stalest* leg without anything rejecting a loop whose legs
are minutes apart. So a cycle can price one leg from now and the other from six minutes
ago and report the difference as dislocation. It is not one: you cannot trade against a
price that has already gone.

What is **measured**, on the 17.85 h run: claimed dislocation rises monotonically with
the route's fee tier — 6.6 bps on sub-5 bp routes, 9.3, 33.6, then 81.6 bps on
over-50 bp routes — and **93% of all claimed value sits on routes costing more than
5 bps**, 63% above 20 bps. The cheap, liquid, genuinely-arbitraged tier produced $3.12
of the $44.55.

That ordering is backwards. Expensive pools should be *less* arbitrageable, not more.
The hypothesis was that they trade less, so update less, so are staler, so skew more —
and the skew is being read as opportunity. `Cycle::slot_spread` and the
`WERE THE TWO PRICES EVER ON SCREEN AT THE SAME TIME?` report section exist to settle
it: flat across spread bands means the gaps were real, rising means they were not.

**First reading does not support the hypothesis.** 309 fills: same-slot 6.69 bps,
1 slot 5.28, 2–10 slots 5.93, 11–100 slots 6.80. Flat. 81.9% of loops do price their
legs from different slots, but the claimed gap does not grow with the distance.

That sample is one minute of cheap routes — mean fee under 5.5 bps in every band — and
contains none of the 20–100 bp routes where the anomaly lives, so it is not yet an
answer. **The fee-tier correlation remains measured and unexplained.** Whoever picks
this up: let the run cover the expensive routes, then read this table again. If it is
still flat, the time-skew explanation is dead and the concentration of value in
high-fee pools needs a different one — and it needs one before any of that value is
believed.

> **`--verify` structurally cannot catch this.** It checks one pool against a router at
> one instant, and this is a gap that only exists *across two*. It returned 118 checked,
> 0 faults while the pattern above was sitting in the same data. A clean audit is
> evidence about decoders, not about the arithmetic built on top of them.

The pattern in all seven: **an internal check cannot catch an error in what the code
believes about the outside world** — including what it believes its own numbers mean.
Every new decoder must be pinned against a value the decoder itself did not produce,
and every headline must name which search produced it.

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

`crates/evaluator` and `crates/executor` are off the measurement path; either wire them
in or remove them. `crates/dex/src/pumpswap.rs` is in the same state.

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

1. **Paper mode.** `mode = "paper"`. No key material anywhere.
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

*Last updated 2026-08-23. 252 tests passing, clippy clean under `-D warnings`.
Runs natively on Windows; WSL no longer required.
Test names are sentences on purpose — `one_standing_gap_is_one_opportunity_not_a_thousand_trades`
is the specification, and the assertion is the proof.*

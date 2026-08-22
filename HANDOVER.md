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

**Build and run from WSL, not Windows.** There is no MSVC linker on this machine and
Git Bash shadows the error confusingly.

```bash
wsl -d Ubuntu -- bash -c 'cd /mnt/d/Dev/Quant/cryptobot && ./scripts/build.sh'
```

Always use `scripts/build.sh`, never bare `cargo build`. `scripts/env.sh` sets
`CARGO_TARGET_DIR` to `$HOME/.cargo-target/cryptobot`, both to keep `target/` off the
`/mnt/d` 9p mount (~10x on build time) and because that is where the supervisor looks
for the binary. A bare `cargo build` writes to `./target`, succeeds, and leaves the
supervisor faithfully running whatever was there before — silently. That cost two
builds' worth of confusion before `env.sh` existed; do not reintroduce it.

```bash
scripts/run-forever.sh          # supervised run, restarts on process death
cb-bot --report                 # read the ledger without stopping the run
cb-bot --verify                 # audit decoders against an independent router
```

Dashboard on `http://127.0.0.1:8787` while running.

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
crates/server     event bus + static dashboard
crates/evaluator, crates/executor   present, not on the measurement path
dashboard/dist    single-file dashboard
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

The pattern in all five: **an internal check cannot catch an error in what the code
believes about the outside world.** Every new decoder must be pinned against a value
the decoder itself did not produce.

---

## 5. Open items, ranked

### 5.1 — The report's headline edge is a rate nobody can trade *(highest priority)*

The current run reports `best edge 1156.44 bps` on `SOL → TRUMP → USDC → SOL`, held
steady for 7+ consecutive seconds. It is not a real opportunity, and chasing down why
found a genuine reporting bug.

**It never became a fill.** The largest TRUMP entry in `paper_fills` is 12.7 bps. The
1156 bps figure lives only in `sweeps.best_edge_bps`.

**The mechanism.** Two different searches run each sweep. `survey_from_base` prices
*every* cycle by `marginal_edge_bps` — the rate at infinitesimal size. `find_from_base`
returns only cycles with a feasible, profitable size. The leaderboard and
`sweeps.best_edge_bps` come from the first; fills come from the second. A cycle with a
downstream leg parked at its tick boundary has near-zero capacity and an enormous
marginal rate, so it tops the survey while being completely untradeable. Note the
signature in the data — depth *falls* as the reported edge rises:

```
band            sweeps   avg depth $
under 50 bps      6,494      1,838.72
50-200 bps           85        913.28
over 200 bps         51        156.14
```

That is backwards from a real dislocation, where a bigger gap is worth more, not less.

**What this contaminates, and what it does not.** `mean edge`, `best edge`, and
`mean price dislocation` in the report are computed from the marginal survey and are
overstated — 51 of 6,356 samples sit above 200 bps and drag the mean. Everything
derived from `paper_fills` — episodes, survival, what an opportunity is worth,
break-even capital — requires a feasible size and is **not** affected. So the §1
conclusions stand; the headline distance-to-profitable numbers do not.

Suggested fix: report the marginal survey and the tradeable set as two separate
things rather than one number, or weight the survey by quotable depth so a rate with
nothing behind it cannot lead. Do not simply clamp the outliers — the gap between the
two searches is real information about how much of the book is untouchable.

### 5.2 — Stale state between reconciles is unguarded

Not the cause of §5.1, but a real hole found while chasing it.

For an AMM, *no update means no change* — the account only moves when someone swaps
it. That makes a silently dropped subscription indistinguishable from a quiet pool
from the WebSocket stream alone, and `crates/bot/src/live.rs:606` says so explicitly.
The answer in place is `reconcile()`, which re-reads every watched account over HTTP
every 180 s and folds the result back into the store, so a stale pool *is* repaired —
but only on that cadence, and nothing excludes it from the sweep in the meantime.

`PoolStore::stale_pools()` exists at `crates/scanner/src/store.rs:79` and is **never
called from the sweep path**. A per-sweep slot-lag guard, with the exclusion count
surfaced the way subscribe errors and reconcile drift already are, would close the
window without waiting on the timer.

### 5.2b — `--verify` reports 5 faults and it is not explained

51 checked, 5 faults, clustered at +21 to +25 bps, on pools that previously passed
clean. This is *not* explained by §5.1: `--verify` quotes real sizes through real
pools, not marginal rates.

- Jupiter now serves routes labelled `Aquifer` and `Flux` — market-maker/RFQ
  liquidity, not the AMM pool being asked about. The audit's premise ("better than the
  router ⇒ we are wrong") assumed the router quotes AMM liquidity.
- Against that, a direct cross-check of SOL/USDC across three independent programs
  agreed with Jupiter to under 1 bps ($93.84–93.88 vs $93.84), with the new DAMM v2
  pool right in line — so the decoders are not uniformly biased.

Worth recording *which* venue Jupiter actually routed through, so a mismatch is visible
rather than inferred, and re-running before drawing conclusions.

### 5.3 — Run longer

Everything above is hours, not weeks. Nothing here says how the edge distribution
behaves through a real volatility spike. The supervisor exists precisely so this can
run unattended; it just has not yet.

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
| `cryptobot.db` | current run — **suspect, see §5.1** |
| `cryptobot-pre-cyclekey.db` | ~7h, correct decoders, but mirror-double-counted opportunity counts |
| `cryptobot-contaminated-by-cpmm-bug.db` | kept as the record of what the phantom looked like |

All gitignored. `sweeps` is the valuable table — it records what the market looked like
whether or not anything cleared, which is what turns "found nothing" into a
measurement. A run that records only its wins can report the size of a win but never
the odds.

Note the schema migration: `paper_fills.cycle_key` was added later, and `episodes()`
falls back to `route || venues` for rows that predate it. Old and new rows in one
database therefore group by different rules — which is why the pre-fix run was archived
rather than continued.

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

*Last updated 2026-08-22, at commit `7bb5b19` plus this file. 184 tests passing, clippy
clean under `-D warnings`. Supervised run live and collecting.
Test names are sentences on purpose — `one_standing_gap_is_one_opportunity_not_a_thousand_trades`
is the specification, and the assertion is the proof.*

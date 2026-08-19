# Design — Solana Arbitrage Research & Execution System

**Date:** 2026-08-19
**Status:** Draft, pending user approval
**Codename:** `cryptobot`

---

## 1. What we are building, and why this shape

A **paper-first arbitrage research system** for Solana with a real-time dashboard. It
detects cross-DEX price discrepancies, computes exactly what each would have earned net
of tips and tax, records everything, and — only once the measured edge justifies it —
executes for real.

The framing matters. This is not "a bot that makes money." It is **an instrument that
measures whether an edge exists**, which can be switched into execution mode if it does.
That framing is forced by three findings from research:

1. **Failed attempts cost $0** (Jito bundle atomicity). So there is no penalty for
   attempting aggressively — but also no way to "learn by losing money slowly." You
   either have infra good enough to win races, or you don't.
2. **Fixed infra ($138+/mo) exceeds the trading capital ($10–20) by 7–14×.** So the
   spend decision must be made *from measured data*, not before.
3. **The edge, if any, is in uncontested long-tail opportunities**, not in racing
   Frankfurt for SOL/USDC. That makes *coverage* the design goal, not nanoseconds.

The success criterion for v1 is therefore: **after 30 days of paper running, we can state
with evidence whether a real edge exists, and how big it is after tip and tax.**

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        bot (Rust binary)                        │
│                                                                 │
│   feed ──► pool_store ──► scanner ──► evaluator ──► executor    │
│  (WS/gRPC)  (in-mem)     (cycles)   (exact math)   (paper|live) │
│                                          │             │        │
│                                          └──► ledger ◄──┘       │
│                                                (SQLite)         │
│                                     │                           │
│                          tokio::broadcast (lossy)               │
│                                     ▼                           │
│                           server (Axum HTTP + WS)               │
└─────────────────────────────────────┬───────────────────────────┘
                                      │ WebSocket + REST
                                      ▼
                    dashboard (Next.js + React 19)
```

**The single most important structural rule: the trading hot path must never block on
the UI.** The bridge between them is a bounded `tokio::sync::broadcast` channel that is
explicitly allowed to drop messages. If the dashboard stalls, disconnects, or a browser
tab freezes, the scanner does not notice. Backpressure never propagates backwards into
trading. The UI is a *lossy observer*, never a participant.

### Crates

| Crate | Responsibility | Depends on |
|---|---|---|
| `core` | domain types (`Pool`, `Opportunity`, `Fill`, `Money`), integer math, config, errors | — |
| `dex` | per-DEX account decoders + off-chain quote math, behind one `Quoter` trait | `core` |
| `feed` | data ingestion behind a `Feed` trait; `WsFeed` now, `GrpcFeed` later | `core` |
| `scanner` | maintains pool graph, finds candidate cycles on updates | `core`, `dex` |
| `evaluator` | exact profit incl. fees, slippage, tip, tax; go/no-go decision | `core`, `dex` |
| `executor` | tx construction, simulation, Jito bundles; `PaperExecutor` + `LiveExecutor` | `core` |
| `ledger` | SQLite persistence, P&L, tax ledger, metrics | `core` |
| `server` | Axum REST + WebSocket, serves dashboard state | all |
| `bot` | binary; wiring, config, supervision, circuit breakers | all |

Each is independently testable. `dex` and `evaluator` are pure functions over inputs —
no I/O — so the money-critical math is unit-testable without a network.

### Key trait boundaries

```rust
// One integration point per DEX. Adding a DEX = implementing this.
trait Quoter {
    fn program_id(&self) -> Pubkey;
    fn decode_pool(&self, account: &AccountData) -> Result<Pool>;
    fn quote(&self, pool: &Pool, in_mint: Pubkey, amount_in: u64) -> Result<Quote>;
    fn build_swap_ix(&self, pool: &Pool, params: &SwapParams) -> Result<Instruction>;
}

// Swap the data source without touching strategy code.
trait Feed {
    async fn subscribe(&self, filters: &[Filter]) -> Result<Receiver<AccountUpdate>>;
}

// Paper and live are the same interface. Paper is not a special case.
trait Executor {
    async fn execute(&self, opp: &Opportunity) -> Result<Outcome>;
}
```

`PaperExecutor` runs the identical code path as `LiveExecutor` up to and including
`simulateTransaction` against real chain state — it only stops short of signing and
submitting. This means paper results reflect real compute limits, real account
constraints and real failure modes, not a toy model. It is the difference between a
simulation that means something and one that doesn't.

## 3. Money math

**All monetary values are integers.** Lamports and token base units, `u64`/`u128`. No
floats anywhere in the profit path. A `Money` newtype prevents mixing mints or decimals.
Float arithmetic is permitted only for display and charting.

Quote math is reimplemented off-chain per DEX because an RPC round-trip per candidate
quote is fatal — the scanner evaluates thousands of candidates per second against
in-memory pool state.

Profit is computed as:

```
net = amount_out − amount_in − priority_fee − jito_tip − rent_delta
```

and the opportunity is taken only if `net > threshold`, where `threshold` covers
estimation error. Tax is applied at the *ledger* layer, not the decision layer (it is a
period-level liability, not a per-trade cost), but is surfaced in the headline P&L.

## 4. Safety design

Non-negotiable, given this touches real funds.

| Control | Mechanism |
|---|---|
| Paper by default | `mode: paper` in config; live requires config change **and** `CRYPTOBOT_ALLOW_LIVE=1` env var. Two independent switches. |
| Capital cap | Hot wallet is a dedicated burner, funded only to the configured cap. Never the Phantom seed phrase. |
| Daily loss limit | Circuit breaker halts trading for the day when breached. |
| Consecutive failures | N consecutive failures → halt + alert. |
| Max position size | Hard ceiling per trade, independent of what the optimiser suggests. |
| On-chain profit guard | (Phase 3) own program re-checks profit at execution and reverts. Preserves the free-failure property. |
| Kill switch | Dashboard button + `SIGTERM` handler, both flush state and stop cleanly. |
| Token safety filter | Reject mints with freeze authority, hostile Token-2022 extensions, or not on a verified list. |

The **two-switch rule for live mode** is deliberate: no single accidental edit, merge, or
mis-click can start trading real money.

## 5. Dashboard

Screens, in priority order:

1. **Live** — opportunity feed (virtualised table, high-frequency), current P&L, system
   status pills (feed connected, slot lag, RPC health), latency histogram.
2. **P&L** — equity curve, per-day bars, **after-tax as the headline number**, win rate,
   distribution of profit per win.
3. **Opportunities** — historical explorer, filter by token/DEX/size, scatter of size vs
   profit, "would we have won this?" analysis.
4. **Pools** — what we monitor, spread heatmap across DEX pairs, staleness indicators.
5. **Execution** — attempted/landed/failed with reasons; per-tx Solscan links.
6. **Config** — mode toggle (with the live guard), thresholds, caps, kill switch.

**Stack:** Next.js 15 + React 19, Tailwind v4, shadcn/ui, TanStack Table + Virtual for
high-frequency tables, Lightweight Charts for time-series, uPlot for dense latency
histograms, zustand with transient updates to avoid re-render storms.

**Aesthetic:** dark quant-terminal. Tabular-lining numerals throughout (JetBrains Mono
for figures) so digits don't jitter as they update — this matters more than it sounds
when numbers change 10×/second. Green/red reserved *exclusively* for P&L sign; status
uses a separate hue family so "connected" is never confused with "profitable".

## 6. Storage

**SQLite** with WAL mode via `rusqlite`. Reasons: zero-ops, single file, excellent write
throughput for this volume, trivial backup, and the dashboard can read concurrently
while the bot writes. DuckDB is better for analytics but a poor concurrent-write store;
if analysis gets heavy we export Parquet and query with DuckDB separately.

Tables: `pools`, `pool_snapshots`, `opportunities`, `executions`, `fills`, `tax_lots`,
`metrics`.

Every detected opportunity is persisted, including ones we skip and why. **The skipped
ones are the most valuable research data in the system** — they're how we learn whether
the thresholds are right.

## 7. Phasing

| Phase | Deliverable | Live money? |
|---|---|---|
| 0 | Repo, config, CI, SQLite schema, domain types | no |
| 1 | Feed + pool decoding for 2–3 DEXs, pool store, dashboard skeleton showing live prices | no |
| 2 | Scanner + evaluator + paper executor; full dashboard; **30-day measurement run** | no |
| 3 | Decision gate: does measured edge beat infra cost? If yes → on-chain guard program, Jito submission, live mode with caps | yes, $10–20 |
| 4 | Broaden DEX coverage, long-tail discovery, gRPC upgrade if justified | yes |

**Phase 3 is a gate, not a milestone.** If phase 2 shows no edge, the correct outcome is
to stop and keep the instrument. That is a successful project, not a failed one.

## 8. Explicitly out of scope

- Sandwich attacks / adversarial MEV against users. Not building it.
- Anything custodial or multi-user.
- Selling, distributing, or signalling this to third parties.
- Strategies requiring capital we don't have (CEX-DEX arb, funding-rate arb).

## 9. Open questions for the user

1. **Budget ceiling for infra**, if paper trading proves an edge? (Determines whether
   tier 2 at $138/mo is even on the table.)
2. **Comfort with Rust** — the plan uses Rust for the core. A TypeScript-only version
   ships faster but gives up the math safety and latency headroom.
3. **Is the $10–20 figure firm**, or an opening position? It doesn't change the design,
   only the phase-3 caps.

None block phases 0–2, which is the bulk of the work. Assumptions taken meanwhile:
Rust core, $0 infra, paper-only.

---

# Revisions after research (2026-08-19)

Research completed after the draft above. Five findings changed the design.

## R1. Half the market is structurally unmodellable — and that is fine

Prop AMMs (HumidiFi, BisonFi, SolFi, ZeroFi, Obric, Tessera, GoonFi) are ~40–65% of
Jupiter-routed volume and publish **no IDL, SDK, or docs**. We cannot quote them
off-chain without reverse-engineering BPF.

**Design response:** do not try. Phase-1 venue set is the open AMMs only — Raydium v4 +
CPMM, Orca Whirlpools, Meteora DLMM, PumpSwap. This is not a compromise; the deepest,
most contested flow lives behind those closed venues, and it is flow we were never going
to win. Three independent findings (tip asymmetry, infra cost, venue opacity) now point
at the same place: the open long tail.

## R2. Jupiter cannot be the scanner's quote source

Keyless is **0.5 RPS**; free-with-key is 1 RPS; limits are per *organisation*, not per
key. Jupiter also **rejects circular routes** (`inputMint == outputMint`) — Metis is a
DAG search, not a cycle finder.

**Design response:** the scanner quotes **locally** from cached account state, which was
already the plan but is now mandatory rather than an optimisation. Jupiter is demoted to
three narrow roles: cross-checking our math, reaching prop-AMM liquidity we cannot model,
and execution routing. Arb legs must be stitched from two quotes with disjoint
`dexes` sets.

## R3. The on-chain program moves from "phase 3 nice-to-have" to load-bearing

Two independent reasons converged:

1. **It is the only honest profit check.** A transfer fee or transfer hook can make
   off-chain math wrong in ways that are invisible until execution. Only an on-chain
   assertion on *actual post-transfer balances* cannot be lied to.
2. **It is the best available security control.** Hardcoding the profit destination to a
   **cold address inside the program** means a stolen hot key costs the gas float, not
   the profits. Nearly free to implement, and it caps the worst case.

It also preserves the free-failure property: without an on-chain revert, a mispriced arb
lands and loses money instead of reverting for free.

Reference implementation to follow: `buffalojoec/arb-program` (Anza engineer) — copy the
pattern, not the pinned versions.

## R4. Build environment is WSL2, not Windows

MSVC build tools are absent (`link.exe` not found), and Git Bash's coreutils `link`
shadows it confusingly. WSL2 Ubuntu 24.04 has gcc 13.3 / ld 2.42, 16 cores, 11 GB RAM —
and the Solana on-chain toolchain is Linux-native regardless. Building in WSL also
matches the Linux VPS this would deploy to.

`CARGO_TARGET_DIR` must point at the WSL filesystem; writing `target/` across the 9p
`/mnt/d` boundary is roughly an order of magnitude slower.

## R5. Tier-0 measurement has a known bias that must be reported, not hidden

Plain `accountSubscribe` WebSocket runs hundreds of ms behind, **coalesces rapid updates
so intermediate states are missed entirely**, and degrades past a few hundred
subscriptions. gRPC is single-digit-to-low-tens of ms.

This biases paper results in *both* directions: we miss opportunities that existed
(understating edge), and we may credit ourselves with races we'd have lost (overstating
it).

**Design response:** the dashboard must separate two distinct questions and never merge
them into one number:

- **"Did an opportunity exist?"** — from our (lagged, coalesced) view of state.
- **"Would we have won it?"** — modelled against observed competition: did someone else
  take it in the same or next slot, and at what tip?

Reporting a single "paper P&L" without that split would be self-deception, and the entire
point of phase 2 is to avoid exactly that.

## Consequent changes to phasing

| Phase | Change |
|---|---|
| 1 | Venue set fixed: Raydium v4/CPMM, Orca, Meteora DLMM, PumpSwap. Raydium v4 **must** read vault token accounts, not `AmmInfo`, for reserves. |
| 2 | Dashboard reports "existed" vs "would have won" separately. Token-2022 screening with **deny-unknown-extension** allowlist. |
| 3 | On-chain profit-or-revert program with **hardcoded cold profit destination**. Tested via LiteSVM → Mollusk (CU) → surfpool (mainnet fork) → `simulateTransaction` → live. |

## Testing stack (adopted)

LiteSVM for math and multi-instruction flows (inject real mainnet account bytes) →
Mollusk for CU benchmarking (compute regressions cause landing failures) → surfpool for
mainnet-fork integration → `simulateTransaction` with `replaceRecentBlockhash` as the
final pre-send gate → on-chain revert as the actual safety net.

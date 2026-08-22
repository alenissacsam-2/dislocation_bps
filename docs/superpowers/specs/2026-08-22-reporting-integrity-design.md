# Reporting integrity: tradeable headline, staleness guard, verify attribution

Date: 2026-08-22
Scope: HANDOVER §5.1, §5.2, §5.2b
Status: approved design, pre-implementation

## Problem

Three defects in what the instrument reports, ranked as in HANDOVER §5:

1. **§5.1 — The headline edge is a rate nobody can trade.** `sweep()` runs two
   searches: a marginal survey over every cycle and a feasible-size search for
   fills. The dashboard status, leaderboard, ledger `sweeps.best_edge_bps`, the
   report's mean/best edge, its histogram, and its clearing rate all come from the
   *survey* side, which prices cycles at infinitesimal size with no depth behind
   them. A cycle with a downstream leg parked at a tick boundary has near-zero
   capacity and an enormous marginal rate, so it leads while being untradeable —
   observed as 1156 bps on `SOL → TRUMP → USDC → SOL` that never became a fill.
   Depth is also computed wrong on the survey side: first leg only, when the
   binding constraint can be any downstream leg.

2. **§5.2 — Stale state between reconciles is unguarded.** A silently dropped
   WebSocket subscription looks identical to a quiet pool from the stream alone;
   only `reconcile()` (every 180 s) repairs it. `PoolStore::stale_pools()` exists
   but nothing on the sweep path calls it.

3. **§5.2b — `--verify` reports faults it cannot explain.** Jupiter now serves
   RFQ liquidity under labels like `Aquifer` and `Flux`. The audit's premise —
   "better than the router ⇒ we are wrong" — assumed the router quotes AMM
   liquidity. The quote response's `routePlan` carries which venue actually served
   each leg; it is discarded today.

Everything derived from `paper_fills` (episodes, survival, break-even capital) is
*not* affected by §5.1; the §1 conclusions stand.

## Design

### §5.1 — Two named numbers, bottleneck depth

**Cycle depth is a real property of the cycle, computed over all legs.**
New function in `crates/core/src/path.rs`, beside `marginal_edge_bps`:

```
cap_i      = max_in.min(reserve_in)          // leg i, in leg i's input mint
r_j        = marginal out-per-in rate of leg j
depth_base = min_i ( cap_i / ∏_{j<i} r_j )    // every capacity expressed in base units
```

Each downstream leg's capacity is converted back to base-mint units through the
linearized rates of the legs ahead of it; the minimum is the bottleneck. Exact to
first order, one pass, no iteration. `EdgeRow.depth_usd` becomes
`depth_base × usd_per_base_unit(base)` — replacing the current first-leg-only
figure. The fill path needs no change: `optimal_input` already solves the composed
curve exactly.

**The survey publishes two numbers instead of one ambiguous one.** Threshold =
tradable capital (`capital_usd − fee_buffer`, already passed into `sweep()`;
$4.80 today):

- **`tradeable_edge_bps`** — best `edge_bps` among surveyed cycles whose cycle
  depth ≥ threshold. This is the headline everywhere: status event, report
  mean/best, histogram, clearing rate. When no cycle qualifies it is NULL,
  rendered "—", never substituted.
- **`best_edge_bps`** keeps today's meaning — raw marginal maximum over all
  cycles regardless of depth — as an explicit diagnostic column.

The gap between the two stays visible: the report derives and prints how many
samples had a marginal leader that was not tradeable. Outliers are not clamped.

**Clearing redefined:** a sample's `clearing` count includes only cycles with
edge > 0 **and** depth ≥ threshold. Semantic change versus old databases;
documented in the schema comment.

**Leaderboard renders two groups:** rows with depth ≥ threshold sorted by edge
under "tradeable now", the remainder beneath under "marginal only". Both stay
visible; nothing untradeable leads. `Routes` gains `tradeableMinUsd`; the status
event gains `tradeableEdgeBps` / `tradeableRoute`; existing fields keep their
meanings and values. Dashboard header promotes the tradeable pair.

### §5.2 — Per-sweep staleness guard

In `sweep()`: pools whose slot lags the snapshot's `newest_slot` by more than
**300 slots (~60 s)** are excluded from that sweep's snapshot, using the existing
`PoolStore::stale_pools()` (second snapshot build per sweep; ≤90 pools, negligible).
The exclusion count surfaces three ways: `Sweep.stale_excluded` → status event
field → new `sweeps.stale_excluded` column. Warn-level log on change only.

Stated honestly in code and docs: slot-lag cannot distinguish quiet-but-correct
from dropped-subscription-and-wrong. It bounds how wrong a quote can be (~60 s)
instead of proving it right; before this guard the bound was the 180 s reconcile.

**Feed-stall halt closes the guard's own hole:** if the WS dies entirely,
`newest_slot` freezes and nothing ever looks stale. When feed data age exceeds
**5 s**, ledger recording pauses — sweep samples and fills stop being written —
status flags stalled, warn fires. Sweeps continue for the dashboard, labelled. A
measurement that knows its clock stopped does not keep writing numbers.

### §5.2b — Verify shows who served the quote

`jupiter_quote` additionally parses `routePlan[].swapInfo.{label, ammKey}`.
Each probe row prints a "routed via" column (labels joined). When the route used
our own pool (`ammKey` match) and still quoted less output, that is printed
explicitly — the strongest possible confirmation of a decode fault.

Fault classification remains one-sided (being worse is fine) but splits:

- Route includes a venue we decode (`Orca` / `Raydium` / `Meteora` labels):
  hard fault, counted as today.
- No leg matches anything we watch (the `Aquifer`/`Flux` RFQ case): separate
  counter and verdict line — "router served non-AMM liquidity; premise broken
  here, inspect manually". Visible, never silently excused.

### Schema, tests, docs

`sweeps` gains `tradeable_edge_bps REAL`, `tradeable_route TEXT DEFAULT ''`,
`stale_excluded INTEGER NOT NULL DEFAULT 0`, added via checked `ALTER TABLE` so
existing databases open cleanly; readers COALESCE for old rows. Current run
database is already flagged suspect (HANDOVER §7) and will be archived at deploy.

Tests, named as sentences per repo convention, including at minimum:

- `a_bottleneck_downstream_leg_bounds_the_reported_depth`
- `the_headline_edge_ignores_cycles_that_cannot_absorb_the_capital`
- `a_sample_with_no_tradeable_cycle_records_null_not_zero`
- `excluded_stale_pools_are_counted_not_hidden`
- `a_frozen_feed_pauses_the_measurement_instead_of_recording_it`
- `a_fault_against_liquidity_we_do_not_watch_is_labelled_not_counted`
- migration test: old-schema sweeps table opens, columns appear, summary works.

HANDOVER.md marks §5.1/§5.2/§5.2b resolved with pointers to this spec.

## Non-goals

- No config knobs; constants follow main.rs convention (`MAX_STALE_LAG_SLOTS`,
  `FEED_STALL_SECS`), revisitable once `stale_excluded` data accumulates.
- No DLMM, no live trading, no changes to fills/episode semantics (§8 invariants).
- No change to `find_from_base` pricing or `canonical_key`.

## Invariants honoured (HANDOVER §8 numbering)

- **§8.1 Paper mode** untouched; no key material.
- **§8.2 Every priced number from chain** — USD conversion unchanged.
- **§8.3 New decoder pinned from outside** — not triggered here; no decoder changes.
- **§8.4 Refuse rather than extrapolate** — the depth-qualified headline and the
  staleness guard are both instances of refusing to state more than the data supports.
- **§8.5 `--verify` stays one-sided** — attribution makes faults explainable, not excused.
- **§8.6 An opportunity is an episode** — `canonical_key` and fills semantics untouched.

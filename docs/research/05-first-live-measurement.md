# First Live Measurement — 2026-08-21

The instrument is pointed at mainnet. This is what it says.

## Setup

Four real Raydium AMM v4 pools, read over free public WebSocket (`accountSubscribe`,
`processed` commitment). Reserves taken from each pool's two SPL vaults, net of
uncollected protocol fees. Cycles searched up to 3 hops, sized against $4.80 of
tradable capital.

| Pool | Address |
|---|---|
| SOL/USDC | `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2` |
| SOL/USDT | `7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX` |
| RAY/SOL | `AVs9TA4nWDzfPJE9gGVNJMVhcQy3V9PGazuz33BfG2RA` |
| RAY/USDC | `6UmmUiYoBjSrhakAobJw8BvkmJtDVxaeBtbt7rxWo1mg` |

Feed health over the run: **180 updates, 0 dropped, 0 reconnects.**

## The result

| Metric | Value |
|---|---|
| Cycles evaluated | 348 |
| Cycles that cleared fees | **0** |
| Best cycle | SOL→USDC→RAY→SOL (3 hops) |
| **Best edge** | **−62.05 bps** |
| Price dislocation on that route | **12.95 bps** |
| Swap fees on that route | **75.00 bps** (3 × 25 bp) |
| Best edge ever seen in the run | −61.55 bps |

Live prices at the time: SOL/USDC **$89.22**, SOL/USDT **$89.32**.

## What it means

**Fees are the binding constraint, not latency.**

The venues *are* genuinely dislocated — 12.95 bps of real price inefficiency was
sitting there. But a three-hop route across 25 bp pools costs 75 bps to traverse. The
route is short by ~62 bps, and no amount of speed changes that arithmetic. There is no
race being lost here; there is no race at all.

This is worth stating plainly because it contradicts the obvious next move. The
instinct after "no opportunities found" is to buy faster infrastructure — gRPC at
$99–499/month, a VPS near the block engine. **On these pools that spend would return
exactly nothing**, because the cycles are not being sniped before we reach them; they
are unprofitable at the moment they exist.

## What would actually change the number

Ranked by how much of the 62 bp gap each closes:

1. **Lower-fee venues.** The single largest term is fee stacking. Orca Whirlpools and
   Raydium CLMM offer 1–5 bp tiers on major pairs versus 25 bp here. Three hops at 5 bp
   costs 15 bps instead of 75 — which turns today's 12.95 bp dislocation from −62 bps
   into roughly **−2 bps**. Still short, but within touching distance rather than a
   different postcode.
2. **Two-hop routes instead of three.** Two 25 bp hops cost 50 bps rather than 75.
   Requires two venues quoting the *same* pair, which none of these four do — a
   consequence of the pool set, not of the market.
3. **More volatile pairs.** 12.95 bps of dislocation on SOL/USDC is what a tightly
   arbitraged major looks like. Long-tail and newly-created pools dislocate far more,
   which is exactly where the strategy thesis in
   [`01-strategy-thesis.md`](01-strategy-thesis.md) pointed.
4. **Faster data.** Last, and only after the above. Speed matters when you are losing
   races. Right now nothing is racing.

## Honest caveats

- **One short observation window** on four pools. This is a snapshot, not a study. The
  30-day paper run is what produces a distribution.
- **The free WebSocket coalesces updates.** Brief dislocations between the updates we
  receive are invisible to us, so 12.95 bps is a *lower bound* on peak dislocation. It
  does not change the fee arithmetic, which is structural.
- **Fee assumption:** 25 bp per hop is read from each pool's own `swap_fee_numerator`,
  not assumed — so the 75 bp figure is measured, not estimated.
- Priority fees, Jito tips and rent are excluded from the 62 bp gap. Including them
  widens it.

## Consequence for the plan

The next build step is **not** better infrastructure. It is **Orca Whirlpool and
Raydium CLMM decoders**, to reach the 1–5 bp fee tiers. That is the change with the
largest effect on the only number that matters, and it costs nothing but code.

The decision gate in the design doc — "does measured edge beat $138/month of infra?" —
currently answers **no, and infra would not help**. That is a genuinely useful answer
to have for $0, and it is exactly what the instrument was built to produce.

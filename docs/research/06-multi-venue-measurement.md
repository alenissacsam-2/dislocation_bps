# Multi-Venue Measurement — 2026-08-22

The previous measurement ([05](05-first-live-measurement.md)) put the market at
**−62 bps**: a 13 bps price dislocation against a 75 bps fee wall, three hops across
Raydium AMM v4. The conclusion was that fees, not latency, were binding, and that the
next move was cheaper venues rather than faster infrastructure.

That was done. This is what happened.

## What changed

| | Before | Now |
|---|---|---|
| Venues decoded | 1 (Raydium AMM v4) | 4 (Orca Whirlpool, Raydium CLMM, Raydium CP-Swap, Raydium AMM v4) |
| Pools watched | 4 | 83 of 88 |
| Distinct mints | 4 | 29 |
| Pairs quoted by >1 venue | 0 | 17 |
| Cheapest complete round trip | 50 bps | **2 bps** |
| Shortest cycle available | 3 hops | **2 hops** |

The shape of the search changed with it. With one venue per pair the only closed loop
is a triangle. With seventeen pairs quoted several times over, the shortest loop is a
**direct round trip** — buy SOL/USDC on a 1 bp pool, sell it back on another 1 bp pool,
no third token involved. Three basis points of fees instead of fifty.

## The result

Continuous sampling of every cycle from every base mint, five times a second.

| Metric | Value |
|---|---|
| Mean distance to profitable | **+0.09 bps** |
| Mean price dislocation | 2.61 bps |
| Mean fee wall | 2.52 bps |
| Moments when *something* cleared | **29%** |
| Best edge observed | 7.09 bps |
| Widest dislocation observed | 11.09 bps |

The market now sits **at break-even**, oscillating either side of zero, rather than 62
bps away from it. Roughly three moments in ten, some route on Solana is genuinely
profitable to trade.

Edge distribution:

```
  -1.0 ..   0.0    ██████████████████████████████████████████████  71%
   0.0 ..   1.0    ████████████                                    18%
   1.0 ..   2.0    ████                                             7%
   2.0 ..   5.0    ██                                               3%
  above   5.0                                                       1%
```

**The prediction held.** Fees were the binding constraint, cutting them by 25× moved
the edge by ~62 bps, and nothing about latency or infrastructure had to change.

## And then the money turns out not to be there

| | |
|---|---|
| The whole opportunity, median | **$0.0013** |
| What $4.80 of capital captures | $0.00018 — **13%** of it |
| Best single opportunity in the run | **$0.0096** |
| Fixed cost per attempt (tip floor + base fee) | $0.0012 |
| Break-even capital at the median edge | **$3.34** |

A profitable route exists roughly a third of the time. Taking it pays about two
hundredths of a cent. The best moment in the entire run was worth **under one cent to
someone with unlimited capital**.

This is not a capital problem, and that is the important part. `profit_at_optimal` is
the profit at the size that *maximises* profit — the whole pie, at any account size.
When the pie is $0.0013, no amount of borrowed money makes it $13.

### Why flash loans do not rescue this

The natural response is leverage: borrow $10,000 atomically, take the trade, repay in
the same transaction. Kamino, Save and MarginFi all support it, and a failed attempt
costs nothing because the transaction reverts.

It does not help, for a reason that has nothing to do with the loan. **Profit as a
function of trade size is unimodal**: it rises, peaks, and falls, because your own
trade moves the price against you. Past the optimum, borrowing more *reduces* profit.
The optimum on these routes is tens to low hundreds of dollars, set by how deep the
thinnest pool on the route is. Borrowing $10,000 to take a $200 opportunity buys the
worse side of the curve plus a flash-loan fee.

Leverage would take our capture from 13% of the pie to something near 100%. On a
median pie of $0.0013 that is a move from $0.00018 to $0.0013 — real in relative
terms, meaningless in absolute ones.

### Why the opportunities exist at all

They exist *because* they are this small. A searcher with real infrastructure needs an
opportunity to cover priority fees, servers and staff. Anything worth a tenth of a cent
is beneath their floor, so it survives. The moment you scale into these routes with
size, you are visible, and you are racing the people who ignored them only because they
were too small — and they are faster than you.

That is the structural bind, and it is not softened by better code:

> **The opportunities that remain are precisely the ones too small to be worth
> competing for. Taking them at size removes the reason they were available.**

## Two ways the instrument lied, and how they were caught

Both matter more than the numbers above, because they are the reason to trust the
numbers above.

### 1. A phantom arbitrage worth $400

For four hours the instrument reported a **68 bps** dislocation on WSOL/ALNOOR, standing
open, clearing continuously, worth $400 over the run. Both pools decoded cleanly. Both
mints were verified: classic SPL Token, no mint authority, no freeze authority. Fresh
RPC reads reproduced the gap exactly.

It was entirely fake.

Raydium's CP-Swap `PoolState` grew a third pair of fee buckets — `creator_fees_token_0`
and `creator_fees_token_1` — in bytes that used to be reserved padding. A decoder built
from the older struct reads them as zero. That pool had accrued **6.888 SOL** of creator
fees, which were being counted as tradable reserve. The result was a price 52 bps below
the truth — in the direction that *invents* an arbitrage rather than hiding one.

Every internal check passed. The arithmetic was correct; the input was wrong. No amount
of self-consistency could have found it.

What found it was **asking someone else**. Jupiter, routing the same swap, sent both
legs through the same CLMM pool and quoted the round trip at −50 bps. Forced onto
CP-Swap, it was worse in *both directions* — which is impossible for a real dislocation.
A genuine price gap is better one way and worse the other, always. That asymmetry is now
a permanent, on-demand test:

```bash
cb-bot --verify
```

It quotes every decoded pool in both directions against an independent router and flags
any pool claiming an output nobody else can route. Deliberately one-sided: being *worse*
than the router is fine and expected, since it sees venues we do not watch. Being
*better* means we are decoding something wrong. Current state: **100 checked, 0 faults.**

### 2. One gap counted sixty-five thousand times

The sweep re-prices the whole graph five times a second. A gap that stands for an hour
is therefore detected eighteen thousand times, and the ledger was summing every
detection as a separate trade. One standing gap became "$105 per hour".

Detections now collapse into **episodes** — a maximal run of detections of one route —
and an episode is worth its single best moment, because taking an arbitrage is what
removes it. On the same data that read $0.43/hour instead of $105. The report prints
the naive figure alongside, so the size of the correction stays visible.

## Cross-chain

The question was whether the same trade works between chains. It does not, and the
reason is structural rather than a matter of engineering effort.

**Same-chain arbitrage on Solana is atomic.** Both legs are in one transaction. If the
price moves between simulation and execution, the transaction reverts and costs the base
fee — or nothing at all, inside a Jito bundle that does not land. You are never left
holding the position.

**Cross-chain arbitrage is not atomic and cannot be made so.** The fastest USDC path
between Solana and an EVM chain is Circle's CCTP v2 Fast Transfer, at roughly **8–20
seconds**. Standard transfers wait for source-chain finality: 13–19 minutes from
Ethereum. Nothing in that window can be reverted.

Twenty seconds is 50 Solana blocks. At SOL's typical volatility, price moves about
**4 bps (1σ)** in that window — larger than the entire 2.6 bps of dislocation this run
measured. You would be taking a coin-flip on the price to capture an edge smaller than
the coin-flip, and the bridge fee and both chains' gas come out before that.

Professionals do trade cross-chain, but not this way. They **pre-position inventory on
both chains** and rebalance with bridges on a slower cycle. That is not arbitrage; it is
market-making with directional exposure, and it needs enough capital to hold meaningful
inventory on every chain at once. With $5 it is not a strategy, it is a wire fee.

**On Solidity and Foundry:** they are the right tools for EVM contracts, and irrelevant
here. Nothing in the viable path touches an EVM chain. Solana programs are Rust, and the
one contract this project might eventually want — an on-chain profit-or-revert guard —
would be a Solana program. Foundry stays installed and unused.

## What this run does *not* establish

- **Short window.** Hours, not weeks. It says nothing about how the edge behaves across
  a weekend, a listing, or a volatility spike.
- **We assume we win every race.** Every figure above is an upper bound taken on the
  assumption that a detected opportunity is ours. At these sizes nobody is competing,
  which is the only reason that assumption is even arguable.
- **No landing model.** Transactions are assumed to land. In practice some fail, and a
  failed transaction still costs the base fee.
- **No price impact from our own trade.** An episode's value is capped at its best
  single detection, which is conservative; harvesting one in several bites would earn
  more and cost more in fees. The true figure is between the two bounds given.
- **Tax is not modelled in the totals.** India's VDA regime takes 30% of gains with no
  loss set-off and no carry-forward, so gross wins are taxed even in a losing year. On
  numbers this size it is academic, but it is a real drag on any high-churn strategy.

## What is worth doing next

Ranked by what each would actually change.

1. **Run for a week, not an afternoon.** Every number here is a few hours old. The one
   thing that would most change the conclusion is discovering that the edge distribution
   has a fat tail during volatility that a quiet afternoon cannot show. The instrument
   records continuously and `cb-bot --report` reads it; this costs nothing but time.

2. **Meteora DLMM and DAMM v2.** The remaining large Solana venues. DLMM is bin-based
   with a constant-sum invariant inside a bin, so quotes have no slippage within a bin —
   a genuinely different shape that could widen dislocations against curve-based pools.
   It needs `BinArray` accounts alongside the pair account, so it costs more
   subscriptions and more decoder work than anything done so far.

3. **A landing model.** Replace "assumed landed" with a measured probability. This
   requires submitting real transactions, which is a different project with a different
   risk profile.

4. **Nothing about speed.** Measured sweep time is ~6 ms against a 400 ms block. Latency
   has never been the constraint and buying infrastructure would not have helped at any
   point in this project.

## The honest summary

The strategy is real. It is measurable, it clears about a third of the time, and after
two rounds of correcting the instrument the numbers survive an independent audit.

It is also worth about a cent an hour on $5, and the reason it is available is that it
is too small for anyone with the means to take it seriously to bother. Both of those
statements are the same finding.

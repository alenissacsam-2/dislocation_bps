# Infrastructure Economics & Break-Even

## Cost ladder (2026 prices)

| Tier | Product | $/mo | What you get | Latency |
|---|---|---|---|---|
| 0 | Public RPC + Helius Free | **$0** | 1M credits/mo, 10 req/s, WebSocket | 100–300ms |
| 1 | Helius Developer | $49 | 10M credits, 50 req/s, **no gRPC** | 100–300ms |
| 2 | Subglow gRPC (flat) | $99 | Yellowstone gRPC | <50ms |
| 2b | VPS (Singapore/Frankfurt) | $39 | commodity box near Block Engine | — |
| 3 | Helius Business | $499 | 100M credits, 200 req/s, LaserStream gRPC ×10 conns | <50ms |
| 4 | Helius Professional | $999 | 200M credits, 500 req/s | <50ms |
| 4b | Data add-on (10 TB) | $750 | streaming bandwidth | — |
| 5 | Dedicated node, managed | $2,200 | bare metal, NVMe, 512GB RAM, CPU pinning | lowest |

Yellowstone/Geyser gRPC streams account updates from validator memory: **sub-50ms vs
100–300ms** for standard WebSocket. That gap is the whole game at tier 3+.

## The constraint that defines this project

> **The infrastructure costs more per month than the entire trading capital.**

Working capital available: **$10–20.**
Cheapest credible gRPC setup: **$99 + $39 = $138/month.**

The infra bill is **~7–14× the trading capital, every month.** No arrangement of a $20
account services a $138/month fixed cost. At the $1.58 mean arb, tier 2 needs ~87–175
wins/month (3–6/day) merely to break even on infra, before any profit.

This is not a reason to abandon the project. It is the reason to **sequence** it
correctly, and it kills the naive plan ("rent good infra, run bot, profit") before a
rupee is spent.

## What follows: prove the edge at $0 before paying for speed

Since failed attempts are free (Jito bundle atomicity) and the binding cost is *fixed
infra*, the correct order is:

1. **Tier 0, paper mode, $0/month.** Build the full pipeline on free RPC + WebSocket.
   Detect opportunities, compute exact profit, simulate execution, log everything. Never
   sign a transaction.
2. **Measure the edge you would have had.** The dashboard's job is to answer one
   question: *of the opportunities we detected, how many would we have won, and at what
   net profit after tip and tax?* Tier 0 latency means we lose most contested races — but
   that is itself the measurement. What matters is the **uncontested** tail.
3. **Decide from data, not hope.** Only if measured paper edge > $138/month does paying
   for tier 2 make sense. If it doesn't, the honest answer is that the edge isn't there,
   and $0 was spent finding out.
4. **Go live small.** $10–20 hot wallet, hard caps, kill switches.

This sequencing is why the system is designed **paper-first, with live trading as a flag
that defaults to off** — not as a disclaimer, but as the actual engineering plan.

## Where tier 0 is *not* fatally handicapped

Latency matters in proportion to competition. On uncontested long-tail opportunities —
pools minutes old, odd pairs, DEXs few bots integrate — a 200ms disadvantage often costs
nothing, because there is no one to race. This is the same conclusion the tip data gave,
arrived at independently from the cost side.

Two facts now reinforce it:

- **SIMD-0437 cut rent 90%** (SPL token account: $0.159 → $0.0159). Holding token
  accounts for hundreds of long-tail mints just became 10× cheaper — removing the main
  cost that made broad coverage impractical for a small operator.
- **SIMD-0296 raised max tx size 1232 → 4096 bytes.** Longer hop-cycles now fit in one
  atomic transaction, opening routes that were previously unconstructable.

Both landed in the last week. The long-tail/coverage strategy is *cheaper and more
capable this month than it has ever been*, at precisely the moment the latency game got
harder (200ms slots). That is a real, defensible reason to build this now, and it is
the opposite of the influencer framing.

## Honest break-even table

Net per win assumes uncontested (tip ≈ floor, keep ~full profit) vs contested (tip 50–70%).

| Monthly infra | Wins/mo needed @ $1.58 uncontested | @ $0.63 contested (65% tip) | Wins/day (uncontested) |
|---|---|---|---|
| $0 (tier 0, paper) | 0 | 0 | — |
| $138 (tier 2) | 87 | 219 | ~3 |
| $538 (tier 3 + VPS) | 341 | 854 | ~11 |
| $2,239 (tier 5) | 1,417 | 3,554 | ~47 |

Add India's 30% VDA tax on gross wins with no loss set-off, and every figure in the
"wins needed" columns rises by roughly 43% (1 / 0.7).

**Tax-adjusted tier 2 break-even: ~125 uncontested wins/month, ~4/day.**

That is the number the paper-trading phase exists to test. Everything in the build plan
serves measuring it honestly.

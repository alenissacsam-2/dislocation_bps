# Key Numbers — Reality Check

Running log of hard, sourced numbers. Updated as research lands.

## Solana arbitrage market size (2025 full-year, Jito detection algorithm)

| Metric | Value | Source |
|---|---|---|
| Successful arbitrage transactions detected | 90,445,905 | Helius Solana MEV Report |
| Total arbitrage profit | $142.8M | Helius Solana MEV Report |
| Share denominated in SOL | $126.7M (88.7%) | Helius Solana MEV Report |
| **Average profit per successful arbitrage** | **$1.58** | Helius Solana MEV Report |
| Largest single arbitrage | $3.7M | Helius Solana MEV Report |
| Arbitrage share of Solana DEX volume | ~50% | multiple |

**Interpretation.** The mean arb is worth about the price of a coffee. The $3.7M outlier
and a heavy tail drag the mean up, so the *median* is well below $1.58. A profitable
operation is therefore a **volume + win-rate** business, not a big-score business.
Winning 100 arbs a day at the mean nets ~$158/day gross, *before* tips and infra.

## Tip economics

| Metric | Value | Source |
|---|---|---|
| Jito tip as share of expected profit | 50–70% typical | multiple guides |
| Recommended starting tip | 50% of estimated profit | multiple guides |
| Infra advantage: dedicated vs public RPC | ~8x more profitable attempts captured | Dysnix |

**Interpretation.** Apply the tip haircut to the mean: $1.58 gross → roughly $0.47–$0.79
net per win. Infrastructure is not a nice-to-have; it is the entire edge. The same
strategy code earns ~8x more on dedicated infra than on public RPC.

## India tax treatment (user appears to be India-based) — informational, not tax advice

| Rule | Detail |
|---|---|
| VDA gains tax | Flat 30%, regardless of slab |
| Deductions allowed | Cost of acquisition ONLY |
| **Loss set-off** | **NOT permitted** — losses cannot offset gains |
| **Loss carry-forward** | **NOT permitted** |
| TDS | 1% on consideration for VDA transfer; ₹50,000/yr threshold (specified persons), ₹10,000 (others) |
| Crypto-to-crypto swaps | Taxable transfer events |
| Reporting | Schedule VDA in ITR |

**Interpretation — this is the finding that most changes the plan.** An arbitrage bot
generates thousands of taxable VDA transfer events. Because losses cannot be set off
against gains, the tax is assessed on **gross winning trades**, not on net P&L. A bot
that wins ₹100 and loses ₹90 (net ₹10) can still owe 30% on the ₹100 of wins. A
strategy that is marginally profitable pre-tax can be **structurally loss-making
post-tax** under this regime.

Whether 1% TDS is enforceable on self-custodial on-chain DEX swaps is genuinely
ambiguous — the deduction mechanism assumes an exchange/broker intermediary. This is a
question for a qualified Indian CA, not for me. But the 30%-with-no-set-off rule alone
is enough to require modelling tax explicitly in the P&L engine.

**Design consequence:** the bot must track a per-trade tax ledger and report
*after-tax* P&L on the dashboard as the headline number. Pre-tax P&L is a vanity metric here.

## Open items
- Break-even infra cost — pending research agent
- Median (not mean) arb profit — pending research agent

## Live market snapshot — 2026-08-19

| Metric | Value |
|---|---|
| SOL price | **$76.97** (CoinGecko) |
| Base tx fee | 5,000 lamports/signature = 0.000005 SOL = **$0.000385** |

### Jito tip floor, live (`bundles.jito.wtf/api/v1/bundles/tip_floor`, 2026-08-18)

Values are **SOL**, not lamports (the endpoint's field names are misleading).

| Percentile | SOL | USD @ $76.97 |
|---|---|---|
| 25th | 0.00000261 | $0.00020 |
| 50th | 0.00000750 | $0.00058 |
| 75th | 0.00001418 | $0.00109 |
| 95th | 0.00012215 | $0.00940 |
| 99th | 0.00082000 | $0.06311 |
| EMA 50th | 0.00000617 | $0.00047 |

**Critical distinction:** the tip *floor* is the price of admission to the bundle
stream — it is NOT the price of winning a contested opportunity. The floor is
fractions of a cent. The winning bid on an arb that several searchers can all see is
50–70% of that arb's expected profit. Two different numbers, two different purposes:

- **Floor** → what you pay on an *uncontested* opportunity (long-tail, new pool, weird pair).
- **50–70% of profit** → what you pay on a *contested* one (SOL/USDC and other majors).

This asymmetry is the single most important strategic fact in the whole project, and it
points directly at where a small operator's only real edge lives: **opportunities nobody
else is looking at.** On those, you keep ~100% of a small profit instead of ~35% of a
larger one that you usually lose the race for anyway.

### Cost floor per attempt

A 2-hop atomic arb, 1 signature, with a floor tip and modest priority fee:
- base fee: $0.000385
- priority fee (typical): ~$0.0002–0.002
- Jito tip at median floor: $0.00058

→ **~$0.001–0.003 per attempt.** Failed attempts still cost the base + priority fee
(the tip is only paid if the bundle lands). So the loss on a miss is ~$0.0006, and the
gain on an uncontested win is ~$1.58 mean. **The per-attempt asymmetry is genuinely
favourable — roughly 1000:1.** What kills you is not the cost per attempt; it is the
*hit rate* and the *infra bill*. That reframes the whole engineering problem: maximise
the number of high-quality attempts per dollar of fixed cost.

---

## CORRECTION: failed attempts are FREE (Jito bundle atomicity)

My cost-floor estimate above (~$0.0006 lost per failed attempt) is **wrong** for a
correctly-built bundle bot. Verified against Jito docs:

- "If any transaction in a bundle fails, none of the transactions in the bundle will be
  committed to the chain." All-or-nothing.
- If the bundle is not selected: **no transactions land, you pay nothing.**
- The tip is only paid if the bundle lands. Best practice per Jito: put the tip transfer
  **inside the same transaction** as the arb, so a failed arb pays no tip.

**Therefore: a losing arbitrage attempt costs $0 in on-chain fees.** Base and priority
fees are only charged for transactions that actually execute.

### What this does to the economics

This is the most important fact in the project, and it inverts the usual narrative
("you'll bleed out on gas"). The correct model is:

```
profit = (wins × net_profit_per_win) − fixed_infrastructure_cost
```

Failed attempts do not appear in that equation at all. There is **no variable cost of
being wrong**. The only real cost is the monthly infra bill.

Consequences for the design:

1. **Be maximally aggressive about attempting.** Since misses are free, the optimal
   policy is to fire at every opportunity clearing a small profit threshold. Precision
   matters far less than recall. A scanner that finds 10,000 marginal opportunities and
   wins 50 beats one that finds 100 certain ones and wins 40.
2. **The profit-or-revert guard must be ON-CHAIN**, not in the bot. The free-failure
   property only holds if the transaction itself reverts when unprofitable. This is
   exactly why serious searchers deploy their own arb program: it re-checks profit at
   execution time against the *real* state and aborts. Off-chain checks against stale
   state cannot give this guarantee.
3. **The break-even question is the only question:** how many wins per month to cover
   fixed infra? At the $1.58 mean, a $50/mo RPC bill needs ~32 mean-sized wins/month
   (~1/day) to break even. That is a genuinely low bar — which is precisely why the
   space is crowded and why the marginal opportunity is competed away.

### Jito operational facts

| Item | Value |
|---|---|
| Mainnet Block Engine | `https://mainnet.block-engine.jito.wtf` |
| Regions | Amsterdam, Dublin, Frankfurt, London, NY, Salt Lake City, **Singapore**, **Tokyo** |
| Max bundle size | 5 transactions |
| Execution | sequential + atomic, same slot |
| Tip accounts | 8 fixed addresses (pick at random per bundle to avoid contention) |

**From India, Singapore is the nearest Block Engine region** (~50–70ms RTT vs ~120–150ms
to Frankfurt). Region choice is the single cheapest latency win available.

## Agave 4.2 — network changes landing RIGHT NOW (Aug 2026)

Mainnet feature activation began **17 Aug 2026** — two days before this project started.
Most tutorials and repos online predate these and are stale.

| SIMD | Change | Detail | Impact on this project |
|---|---|---|---|
| **SIMD-0296** | Max tx size **1232 → 4096 bytes** | new tx `v1` format; `v0`/legacy still work | **Large.** More hops + more accounts per atomic arb. Address Lookup Tables stop being a hard requirement for multi-hop routes. Enables 3–4 hop cycles that previously did not fit. |
| **SIMD-0525** | Slot time **400ms → 200ms** | four 50ms steps, each feature-gated; halts if skip rate rises | Halves the latency budget. Hurts remote/high-RTT operators → **reinforces coverage-over-latency thesis**. |
| **SIMD-0437** | Rent constant **6960 → 696** lamports/byte (−90%) | SPL token account rent $0.159 → **$0.0159** | Cheap to create many ATAs → cheap to cover many long-tail tokens. **Directly subsidises the coverage strategy.** |
| Alpenglow | ~150ms finality | feature-complete in 4.2, activates in **Agave 4.3, Oct 2026** | Watch, don't design around it yet. |

Note SIMD-0437 and the coverage thesis reinforce each other: holding token accounts for
hundreds of long-tail mints just became 10x cheaper, which is exactly the cost that
previously made broad coverage impractical for a small operator.

**Local toolchain is stale:** installed Solana CLI is 1.18.17. Needs upgrading to
current Agave before any on-chain work.

## Competitive benchmark: SolanaMevBot (commercial, closed-source)

Useful as a yardstick for what a working product looks like.

| Item | Value |
|---|---|
| Strategies | (a) Jupiter self-hosted-quote bot, (b) on-chain bot monitoring specific mints/pools with on-chain optimal-size calc |
| Fee | **15% of successful arb profit**, after Jito tip; nothing if nothing lands |
| Min hardware | 1 core / 4GB RAM (Jupiter bot); "basically any machine" (on-chain bot) |
| Claim | "never lose on gas, only land profitable txs" — consistent with verified bundle atomicity |
| Public data | Dune dashboard `dune.com/cetipo/solanamevbot-dashboard` (HTTP 500 on fetch; retry later) |

Two takeaways: (1) the hardware bar is genuinely low, which corroborates that this is
buildable on commodity kit; (2) a commercial operator charging 15% of profit implies the
strategy does produce positive returns for at least some users — otherwise there would be
no fee to collect. Their public per-user dashboard is the best available
survivorship-corrected evidence and should be examined before going live.

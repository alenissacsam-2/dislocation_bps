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

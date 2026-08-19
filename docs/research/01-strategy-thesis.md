# Strategy Thesis — Where a Small Operator Can Actually Win

Draft. Written from confirmed numbers in `00-key-numbers.md`; will be revised as
research agents report.

## 1. What the influencer story actually was

The claim — "$1 turned into $1,000–2,000, using Aave flash loans on Solana" — contains
one verifiable factual error and one statistical impossibility.

**The factual error.** Aave is an EVM protocol. It runs on Ethereum, Base, Arbitrum,
Optimism, Polygon, Avalanche and similar chains. It does not run on Solana, and Solana's
account model means an Aave deployment there would be a ground-up rewrite, not a port.
Anyone describing "Aave flash loans on Solana" is describing something that does not
exist. Solana *does* have flash loans — Kamino, Solend/Save, MarginFi — but that is not
what was said. The error matters because it is diagnostic: it is the kind of mistake
made by someone repeating vocabulary they do not understand, which is the signature of
either a content farm or a funnel.

**The statistical impossibility.** The mean profit of a successful Solana arbitrage in
2025 was **$1.58**, across 90.4M detected successful arbitrages totalling $142.8M. To
get from $1 to $2,000 you need roughly 1,265 mean-sized wins *with zero losses and zero
costs*, compounding from a base that cannot pay for its own transaction fees. It does
not happen. What does happen: someone shows a wallet with a big number in it, and sells
you a bot, a course, a Telegram subscription, or a token.

None of this means Solana arbitrage is fake. $142.8M was real, and someone earned it.
It means the *distribution* of who earned it is nothing like the story.

## 2. The one asymmetry worth building on

From the tip data, two numbers that are usually conflated:

- The **Jito tip floor** — admission price to the bundle stream — is a *fraction of a
  cent* (median 0.0000075 SOL ≈ $0.0006).
- The **winning tip on a contested opportunity** is *50–70% of that opportunity's
  expected profit*.

These describe two different worlds.

On SOL/USDC and other major pairs, dozens of well-capitalised searchers with bare-metal
machines co-located next to validators see the same opportunity within microseconds. The
auction resolves it. You lose the race almost always, and on the rare occasion you win
you keep ~30–50% of the profit. Competing here from a home connection in India — where
the round-trip to Frankfurt or New York alone exceeds the entire opportunity window — is
not a strategy. It is a donation.

On long-tail opportunities — a pool created eleven minutes ago, an odd token pair, a DEX
that most bots have not integrated, a stale oracle-based pool — often *nobody else is
looking*. There, you pay the floor. You keep nearly all of a smaller profit.

> **The thesis: don't buy the contested $5 arb for $3.50 in tips and lose the race 99
> times out of 100. Find the uncontested $0.40 arb, pay $0.0006, and win it.**

This is the only version of the business that is geometrically available to a solo
operator, and it dictates every engineering decision downstream. It means **coverage
beats latency**. Our edge is not being faster than Frankfurt; it is *looking in more
places than the people who are faster than us bother to look*.

## 3. What that implies for the build

Because the edge is coverage, not speed, the system is optimised differently from a
classic HFT stack:

| Conventional MEV bot | This system |
|---|---|
| Minimise tail latency at all costs | Maximise *breadth* of pools monitored |
| Co-locate, bare metal, kernel bypass | Commodity hardware is acceptable |
| A few deep, hot markets | Many shallow, cold markets |
| Outbid rivals in the tip auction | Avoid auctions we would lose |
| Latency is the KPI | Hit-rate and coverage are the KPIs |

That is a genuinely tractable engineering problem for one developer, and it is why this
project is worth building even though the influencer story is false.

## 4. The tax constraint, which changes the target

Under India's VDA regime: gains are taxed at a flat 30%, **losses cannot be set off
against gains, and cannot be carried forward.** Every crypto-to-crypto swap is a taxable
transfer.

An arbitrage bot emits thousands of swaps. The tax is therefore assessed on the sum of
*winning* trades, with no credit for losing ones. Consider a bot that in a month makes
₹10,000 across winners and loses ₹8,000 across losers — ₹2,000 net. Tax is 30% of
₹10,000 = ₹3,000. The operator is **₹1,000 down on a profitable strategy.**

The consequence is not "don't build it". The consequence is that the strategy must clear
a much higher bar than break-even, and the system must therefore:

1. compute and display **after-tax** P&L as the headline figure, not gross;
2. maintain a per-trade tax ledger from day one, because reconstructing thousands of
   swaps at filing time is miserable;
3. strongly prefer **high win-rate** strategies over high-gross-churn strategies, since
   the no-set-off rule penalises churn specifically.

This reinforces the same conclusion as §2: few, clean, uncontested wins beat many
contested attempts. The tax code and the tip auction happen to point the same direction.

*(Informational only — not tax advice. Whether 1% TDS attaches to self-custodial DEX
swaps is genuinely unsettled and is a question for a qualified CA.)*

## 5. Honest expected outcome

Stated plainly, so there is no ambiguity later:

- With **$10–20**, the realistic goal is **not profit**. It is a working, instrumented
  system, validated on real data, that has proven whether it has an edge — at a total
  risk of $10–20 rather than $10,000.
- The system should run in **paper mode first**, and the decision to go live should be
  made from *its own measured hit-rate*, not from optimism.
- Most likely outcome, stated in advance: the bot finds real opportunities, and the
  measured net-of-tip, net-of-tax edge is somewhere near zero. That is a *successful*
  experiment — it costs $20 to learn, and the instrumentation is the durable asset.
- The genuinely valuable output is the skill and the codebase. A person who can build a
  low-latency multi-DEX data pipeline with a real-time dashboard is employable in a way
  that a person who bought a bot is not.

This is the honest frame. The build proceeds on it.

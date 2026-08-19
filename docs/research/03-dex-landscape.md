# DEX Landscape & Quote Math (research, Aug 2026)

## The finding that most changes the strategy

**Proprietary "prop AMMs" have no public SDK, IDL, docs, or frontend — and they are
roughly 40–65% of Jupiter-routed volume.**

| DEX | Program ID | Type | SDK |
|---|---|---|---|
| HumidiFi | `9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp` | oracle/prop | ⛔ none |
| BisonFi | (2026 launch, ~35% of prop-AMM volume) | oracle | ⛔ none |
| SolFi | `SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe` | oracle (Pyth) | ⛔ none |
| ZeroFi | `ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY` | oracle | ⛔ none |
| Obric v2 | `obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y` | oracle + CL | ⛔ none |
| Tessera V (Wintermute) | `TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH` | oracle | ⛔ none |
| GoonFi | `goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j` | oracle | ⛔ none |
| Lifinity v2 | `2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c` | oracle-rebalancing | ⚠️ IDL public, **curve closed** |

You cannot quote these off-chain without disassembling BPF and reconstructing both the
account layout and the oracle read.

**Why this is good news for us, not bad.** The deepest, most contested liquidity now sits
behind venues a small operator structurally cannot model. Competing there was never on
the table. It removes the temptation to try, and pushes all effort onto the open AMMs —
which is exactly where the long-tail thesis said to go. Three findings have now
independently converged on the same conclusion.

*Lead worth auditing:* the `swaps` crate (docs.rs/swaps) claims off-chain quote
implementations for 25 Solana DEXs **including reverse-engineered HumidiFi / SolFi V2 /
Tessera / Obric / BisonFi**. If sound, that is a large shortcut. Treat as untrusted
third-party code until audited against the checklist in `04-security.md`.

## Open AMMs we can quote ourselves

| DEX | Program ID | Math | Raw-bytes readable |
|---|---|---|---|
| Raydium AMM v4 | `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8` | constant product | ✅ `AmmInfo` (`repr(C,packed)`, `Pack`, no discriminator) |
| Raydium CPMM | `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` | constant product, T22-aware | ✅ Anchor `PoolState` |
| Raydium CLMM | `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` | concentrated liquidity | ✅ `PoolState` + `TickArrayState` |
| Orca Whirlpools | `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` | concentrated liquidity | ✅ `Whirlpool` + `TickArray` |
| Meteora DLMM | `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` | discrete bins | ✅ `LbPair` + `BinArray` |
| Meteora DAMM v2 | `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG` | CP + dynamic fee | ✅ Anchor IDL |
| PumpSwap | `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` | constant product | ✅ `Pool` |
| Pump.fun curve | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | virtual-reserve CP | ✅ `BondingCurve` |
| Phoenix v1 | `PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY` | on-chain orderbook | ✅ header + ladder |
| OpenBook v2 | `opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb` | Anchor CLOB | ✅ `Market` + `BookSide` |
| Invariant | `HyaB3W9q6XdA5xwpU4XnSZV94htfmbmqJXZcEbRaJutt` | concentrated liquidity | ✅ `Pool` + `Tickmap` |

**Phase-1 target set: Raydium v4 + CPMM, Orca Whirlpools, Meteora DLMM, PumpSwap.**
Constant-product venues first (v4, CPMM, PumpSwap) because the closed-form optimal size
applies directly.

## Implementation gotchas that will silently produce wrong quotes

1. **Raydium v4 `AmmInfo` does not contain reserves.** True reserves = the two SPL vault
   token account balances **minus** `need_take_pnl_coin` / `need_take_pnl_pc`. You must
   subscribe to the vault accounts as well as the pool account. Getting this wrong yields
   quotes that look right and lose money.
2. **Orca** `swap_quote_by_input_token` needs 3–5 `TickArray` accounts around
   `tick_current_index`, plus `oracle` for adaptive-fee pools and `transfer_fee_a/b` for
   Token-2022. Tick arrays hold 88 ticks; under-prefetching **silently truncates** the
   quote rather than erroring.
3. **Meteora DLMM** fee = base + variable, where the variable part derives from
   `VolatilityAccumulator` decaying against `last_update_timestamp`. Quote drifts unless
   you pass a current clock timestamp. Bin price = `(1 + bin_step/10_000)^bin_id`.
4. **Concentrated-liquidity and bin legs have no global closed form.** Use the
   constant-product formula as a seed, then ternary search on the real quote function
   (profit is unimodal in size; ~40 iterations to 1e-12 relative).

## Jupiter API (2026)

| Item | Value |
|---|---|
| Keyless | `lite-api.jup.ag` — **0.5 RPS / 30 RPM** |
| Free w/ key | 1 RPS / 60 RPM |
| Developer | $25/mo — 10 RPS |
| Launch / Pro | $100 / $500 per mo — 50 / 150 RPS |
| Quote | `GET /swap/v1/quote` |

Rate limits are a 60s sliding window **per organisation, not per key** — buying extra
keys gains nothing.

- **No circular routes.** `/quote` rejects `inputMint == outputMint`; Metis is a DAG
  search, not a cycle finder. Arbitrage requires stitching two quotes (A→B, B→A) with
  disjoint `dexes`/`excludeDexes` sets, composed via `/swap-instructions`.
- `restrictIntermediateTokens` (default true) limits mid-hops to curated liquid tokens —
  **set false when hunting long-tail cycles**, which is our whole thesis.
- `maxAccounts` caps account locks so both legs fit one transaction. Critical for atomic
  composition (and much less binding now that SIMD-0296 raised the tx limit to 4096 B).
- Self-hosted binary moved to `jup-ag/metis-binary` and is now **gated behind a binary
  key** — no longer a free download. Needs ~64 GB RAM.

**Conclusion: Jupiter is unusable as the scanner's primary quote source** at 0.5–1 RPS.
It is useful for (a) cross-checking our own math, (b) reaching prop-AMM liquidity we
cannot model, (c) execution routing. The scanner must quote locally from cached state.

## Data ingestion

Yellowstone gRPC `SubscribeRequest` filters: `accounts` (by `account[]`, `owner[]`,
`memcmp`/`datasize`), `slots`, `transactions`, `blocks`, `blocks_meta`, `entry`; plus
`accounts_data_slice {offset,length}` and `commitment`.

For our scanner: subscribe `accounts` with `owner ∈ {target program IDs}` at `processed`,
and use `accounts_data_slice` to ship only reserve/tick bytes — the single biggest
bandwidth lever.

| Provider | gRPC entry | Note |
|---|---|---|
| Subglow | $99/mo (2 streams) | cheapest |
| Shyft | from $199/mo | Yellowstone-compatible |
| Chainstack | ~$399/mo | Yellowstone |
| Helius LaserStream | $499/mo Business | ⚠️ **proprietary, not Yellowstone wire-compatible** — client rewrite |
| QuickNode | ~$499/mo | Yellowstone |
| Triton | usage-priced; Fumarole GA Jun 2026 | resumable, multi-consumer |

**Honest limitation of the free tier we start on:** plain `accountSubscribe` WebSocket
runs hundreds of ms behind, **coalesces rapid updates so intermediate states are simply
missed**, and degrades past a few hundred subscriptions. gRPC is single-digit-to-low-tens
of ms. On WebSocket we are structurally last in line — roughly one or more full slots
behind.

This must be stated plainly in the paper-trading results: tier-0 measurements
**understate** achievable edge (we miss states entirely), while simultaneously
**overstating** it if we assume we'd have won races we'd actually have lost. The
dashboard must therefore separate "opportunity existed" from "we would have won it",
and never conflate the two.

## Verified: closed-form optimal size

Independently derived in `crates/core/src/amm.rs` and cross-checked against the published
form — **algebraically identical**, and numerically confirmed optimal against a parameter
sweep (8/8 unit tests pass).

```
x* = [ √(γ₁·γ₂·A·B·C·D) − A·C ] / [ γ₁·(C + γ₂·B) ]

profitable iff   γ₁·γ₂·B·D > A·C      ← one multiply, no sqrt; gate before sizing
```

where pool 1 = `(A, B)` (spend A, receive B), pool 2 = `(C, D)` (spend B, receive A),
`γ = 1 − fee`.

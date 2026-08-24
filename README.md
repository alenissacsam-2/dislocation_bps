# dislocation_bps

**A measurement instrument for Solana AMM arbitrage.** It watches live mainnet, prices
every arbitrage cycle it can see, and records what *would* have cleared.

It is not a trading bot and has never signed a transaction. There is no execution code,
no key handling, and no path from this repository to a real order. It exists to answer
one question honestly: **is there a tradeable edge, and how big is it?**

[![ci](https://github.com/alenissacsam-2/dislocation_bps/actions/workflows/ci.yml/badge.svg)](https://github.com/alenissacsam-2/dislocation_bps/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/alenissacsam-2/dislocation_bps?color=e0b341)](https://github.com/alenissacsam-2/dislocation_bps/releases/latest)
[![licence](https://img.shields.io/badge/licence-MIT-blue)](LICENSE)
![platform](https://img.shields.io/badge/platform-Windows%20x64-lightgrey)
![mode](https://img.shields.io/badge/mode-paper%20only-brightgreen)

![The application, showing a completed run](docs/app.png)

## The answer, so far

| | |
|---|---|
| Cheapest complete round trip anywhere in the universe | **2 bps** |
| Median opportunity, at the size that maximises it, at any capital | **$0.0013** |
| Max lifetime of every opportunity worth more than $0.10 | **0 slots** |

Opportunity size and opportunity lifetime run in **opposite directions**. Sub-cent gaps
loiter for seconds; every one worth more than a dime was gone before the next slot
began. There is no size at which an opportunity is both worth taking *and* still there
when you arrive — which answers "just add more capital" and "just go faster" with data
rather than argument.

The run in the screenshot above is representative: **26,131 opportunities** seen over
15 hours, 669 of them clearing, **$2.50** realised. Priced at a $10,000 book the same
opportunities come to $112 — and at unlimited capital, $118. The ladder flattens because
the cycles run out of *depth*, not funding, which is also the measurement that says a
flash loan would have added nothing.

### One finding is still open, and it matters

Edge rises with the fee of the route it is on — 2.16 bps on sub-5 bp routes, 11.95 bps
on routes over 50 bps — and most of the reported value sits on the expensive ones. That
ordering is backwards for real arbitrage: crossing a more expensive pool should leave
*less* behind, not more.

Two explanations have been tested. One was a definitional tautology in how the metric
was computed. The other, clock skew between pool observations, turned out to be real but
far too small to account for a 5× gradient. A later run did not reproduce the gradient at
all. **Until this is explained, treat the headline profit as unproven** — see §4 of
[`HANDOVER.md`](HANDOVER.md), which catalogues every way this instrument has already been
caught lying, including the ones that passed all their own tests.

## Install

Download `Cryptobot.Desk_<version>_x64-setup.exe` from the
[latest release](https://github.com/alenissacsam-2/dislocation_bps/releases/latest) and
run it. It installs per-user, so there is no admin prompt, and it carries both the
application and the bot.

**Nothing else is required** — no Rust, no WSL, no Node, no database server. Windows 10
(1809+) or 11, 64-bit, with the WebView2 runtime that ships with both.

An installed copy has no repository to read, so it keeps its config, ledger and archives
in `%LOCALAPPDATA%\cryptobot`, seeding a paper-mode config on first launch. The
Parameters tab prints the exact folder it settled on — a ledger whose location is a guess
is a ledger nobody can go and check.

## Build from source

Requires the Rust toolchain and Visual Studio Build Tools (the C++ workload, for a
linker). `scripts\build.ps1` checks for the latter up front and prints the one-line
`winget` command if it is missing.

```powershell
scripts\build.ps1
$env:LOCALAPPDATA\cryptobot-win-target\release\cryptobot-desk.exe --start
```

To produce the installer:

```powershell
cargo install tauri-cli --version "^2" --locked
scripts\installer.ps1
```

Run from a checkout, the repository *is* the root: the config beside `crates/` is the one
being edited, and the ledger lands where `cb-bot --report` will look for it.

## The two binaries

**`cryptobot-desk`** is the application. It starts and stops the bot, edits its
parameters, archives runs, and reads the ledger **whether or not anything is running** —
that last part is why it exists, because the browser dashboard it replaced was served by
the very process it was observing, so a stopped bot showed no history at all.

**`cb-bot`** is the instrument itself, and runs headless:

```powershell
cb-bot --report     # read the ledger without stopping the run
cb-bot --verify     # audit every decoder against an independent router
```

> Run `cb-bot` from the repository root. It creates its ledger in the working directory,
> so started elsewhere it quietly opens a second, empty one — which reads exactly like a
> run that found nothing. This has already happened once.

Its API binds **loopback only** (`127.0.0.1:8787`) and has no authentication.

## How it works

```
crates/core       money math — constant-product and concentrated-liquidity quoting,
                  optimal input sizing, marginal edge
crates/dex        one pure decode function per venue, no network
crates/feed       websocket account subscriptions against mainnet
crates/scanner    pool store, snapshots, cycle enumeration
crates/ledger     SQLite — every sample, every clearing cycle, and the queries that
                  collapse detections into opportunities
crates/bot        the event loop, venue dispatch, --report and --verify
crates/server     event bus and three read-only API endpoints
crates/desk       the Windows application (Tauri v2)
```

**The central mathematical fact**, in `crates/core/src/clmm.rs`: a concentrated-liquidity
pool inside its current tick is *exactly* a constant-product pool over virtual reserves
`x = L/√P`, `y = L·√P`. That identity is why five venues share one quote engine, and why
there is no second implementation to disagree with the first.

Currently decoding Orca Whirlpool, Raydium CLMM, Raydium CP-Swap, Raydium AMM v4, and
Meteora DAMM v2 — 84 pools live.

## Safety

Live trading requires **two independent switches** — `mode = "live"` in config *and*
`CRYPTOBOT_ALLOW_LIVE=1` in the environment — and neither does anything, because live
execution is not implemented. The application deliberately exposes **no control that can
write `mode`**: the guarantee is the absence of a mechanism, not a dialog someone can
click through. No key material is ever committed, and the placeholder `executor` crate was
deleted precisely so nobody mistakes an empty crate with a confident name for something
that was built and tested.

Read [`docs/research/04-security.md`](docs/research/04-security.md) before running **any**
third-party Solana bot code, including this.

## Documentation

- [`HANDOVER.md`](HANDOVER.md) — start here. The most useful part is not the
  architecture; it is §4, the catalogue of ways this instrument has already lied, and the
  one way it may be lying now. **Every entry passed all of its own tests.**
- [`docs/research/06-multi-venue-measurement.md`](docs/research/06-multi-venue-measurement.md)
  — the current measurement, and why cross-chain and flash loans do not rescue it.
- [`docs/research/`](docs/research/) — the numbers, the DEX landscape, and the
  infrastructure economics, in the order they were worked out.

## Design principles worth keeping

1. **Refuse rather than extrapolate.** A quote past a tick's capacity, an adaptive-fee
   pool, an unknown fee schedule — decline it. Overstating profit is the only error
   direction that loses money.
2. **A new decoder is not trusted until something outside it agrees.** Errors that
   flatter need an adversary, not more of your own tests.
3. **An opportunity is an episode, not a detection.** One standing gap re-seen five
   hundred times is one opportunity. Summing detections once turned $0.43/hour into
   $105/hour.
4. **Record what the market looked like even when nothing cleared.** A run that records
   only its wins can report the size of a win but never the odds.

## Licence

[MIT](LICENSE).

Nothing here is financial advice, and the headline profit is explicitly unproven. If you
point this at real money you are doing so against the recommendation of its own
measurements.

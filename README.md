# cryptobot

A **measurement instrument** for Solana AMM arbitrage, running in paper mode against
live mainnet.

It is not a trading bot and has never signed a transaction. There is no execution code,
no key handling, and no path from this repository to a real order. What it does is
answer one question honestly: *is there a tradeable edge, and how big is it?*

## What it has measured

| | |
|---|---|
| Cheapest complete round trip anywhere in the universe | **2 bps** |
| Median opportunity, at the size that maximises it, at any capital | **$0.0013** |
| Max lifetime of every opportunity worth more than $0.10 | **0 slots** |

Opportunity size and opportunity lifetime run in **opposite** directions. Sub-cent gaps
loiter for seconds; every one worth more than a dime was gone before the next slot
began. There is no size at which an opportunity is both worth taking and still there
when you arrive — which answers "just add more capital" and "just go faster" with data
rather than argument.

**One finding is open and it matters.** Edge rises with the fee of the route it is on —
2.16 bps on sub-5 bp routes, 11.95 bps on routes over 50 bps — and most of the reported
value sits on the expensive ones. That ordering is backwards for real arbitrage. Two
explanations have been tested and one is dead. Until it is explained, **the headline
profit should be treated as unproven.** See §4 of [`HANDOVER.md`](HANDOVER.md).

## Install it

Grab `Cryptobot Desk_<version>_x64-setup.exe` from the
[releases page](../../releases) and run it. It installs per-user, so there is no admin
prompt, and it carries both the application and the bot.

An installed copy has no repository to read, so it keeps its config, ledger and archives
in `%LOCALAPPDATA%\cryptobot`, seeding a paper-mode config on first launch. The
Parameters tab shows the exact folder it settled on.

To build the installer yourself:

```powershell
cargo install tauri-cli --version "^2" --locked
scripts\installer.ps1
```

## Run it from a checkout

Windows, natively. Requires Visual Studio Build Tools (the C++ workload) for a linker.

```powershell
scripts\build.ps1
$env:LOCALAPPDATA\cryptobot-win-target\release\cryptobot-desk.exe --start
```

Run from a checkout, the repository *is* the root: the config beside `crates/` is the
one being edited and the ledger lands where `cb-bot --report` will look for it.

`cryptobot-desk` is the application: it starts and stops the bot, edits its parameters,
archives runs, and reads the ledger **whether or not anything is running**. That last
part is the reason it exists — the old browser dashboard was served by the process it
observed, so a stopped bot showed no history at all.

Headless, and for the reports:

```powershell
cb-bot --report     # read the ledger without stopping the run
cb-bot --verify     # audit every decoder against an independent router
```

> Run `cb-bot` from the repository root. It creates its ledger in the working directory,
> so started elsewhere it quietly opens a second, empty one — which reads exactly like a
> run that found nothing.

The API binds **loopback only** (`127.0.0.1:8787`) and has no authentication.

## Read this first

- [`HANDOVER.md`](HANDOVER.md) — start here. The most useful part is not the
  architecture; it is §4, the catalogue of ways this instrument has already lied, and
  the one way it may be lying now. Every entry passed all of its own tests.
- [`docs/research/06-multi-venue-measurement.md`](docs/research/06-multi-venue-measurement.md)
  — the current measurement, and why cross-chain and flash loans do not rescue it.
- [`docs/research/04-security.md`](docs/research/04-security.md) — read before running
  **any** third-party Solana bot code, including this.

## Safety

Live trading requires **two independent switches** — `mode = "live"` in config *and*
`CRYPTOBOT_ALLOW_LIVE=1` in the environment — and neither does anything, because live
execution is not implemented. The application deliberately exposes no control that can
write `mode`: the guarantee is the absence of a mechanism, not a dialog someone can
click through. No key material is ever committed.

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

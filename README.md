# cryptobot

A **paper-first Solana arbitrage research system** with a real-time dashboard.

This is not a "make money" bot. It is an instrument that measures whether a tradeable
edge exists — and can be switched into execution mode only once the data says so.

## Status

Phase 0 — scaffolding. Core AMM math implemented and verified (8/8 tests).

## Read this first

- [`docs/research/00-key-numbers.md`](docs/research/00-key-numbers.md) — the numbers that
  set expectations. Mean Solana arbitrage profit is **$1.58**.
- [`docs/research/01-strategy-thesis.md`](docs/research/01-strategy-thesis.md) — why the
  edge, if any, is in *uncontested* opportunities rather than speed.
- [`docs/research/02-infrastructure-economics.md`](docs/research/02-infrastructure-economics.md)
  — why infra costs more per month than the trading capital, and what follows.
- [`docs/research/04-security.md`](docs/research/04-security.md) — read before running
  **any** third-party Solana bot code.

## Build

Build in **WSL2 Ubuntu**, not Windows — MSVC build tools are absent and the Solana
on-chain toolchain is Linux-native.

```bash
wsl -d Ubuntu
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot   # keep target/ off the 9p mount
cd /mnt/d/Dev/Quant/cryptobot
cargo test
```

## Safety

Live trading requires **two independent switches**: `mode = "live"` in config **and**
`CRYPTOBOT_ALLOW_LIVE=1` in the environment. Default is paper. No key material is ever
committed; see `.gitignore`.

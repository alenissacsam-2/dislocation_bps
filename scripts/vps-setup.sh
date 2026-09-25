#!/usr/bin/env bash
# Build the bot on a fresh Ubuntu server. See docs/vps.md.
#
# Installs the compiler and the Rust toolchain for the current user, then builds the
# release binary into ./target. Idempotent: re-running it only rebuilds.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

sudo apt-get update -y
sudo apt-get install -y build-essential pkg-config libssl-dev tmux curl git

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"

cargo build --release -p cb-bot
echo
echo "built target/release/cb-bot"
echo "next: copy config.toml and keypair-encrypted.json here, then see docs/vps.md"

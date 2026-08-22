#!/usr/bin/env bash
# Where this project builds, and where it therefore runs from.
#
# `.cargo/config.toml` cannot express this: the path is under $HOME, which Cargo's
# config does not expand. So it has to come from the shell — and "remember to export
# a variable" is exactly the kind of instruction that silently does not happen. The
# symptom is nasty rather than loud: `cargo build` succeeds, writes a binary to
# ./target, and the supervisor keeps running the *old* one from ~/.cargo-target
# without a word. Sourcing one file from both places is what stops that.
#
# Keeping target/ off the /mnt/d 9p mount is also worth roughly 10x on build time.
# Non-login shells (`bash -c`, cron, anything spawned from Windows) do not read the
# profile that puts cargo on PATH. Without this the build fails with "cargo: command
# not found" while the *report* still runs happily off the last binary — which reads
# exactly like the build having worked.
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target/cryptobot}"
export CB_BOT_BIN="${CB_BOT_BIN:-$CARGO_TARGET_DIR/release/cb-bot}"

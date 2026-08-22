#!/usr/bin/env bash
# Build the release binary the supervisor will actually run.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# shellcheck source=env.sh
. scripts/env.sh
cargo build --release -p cb-bot "$@"
echo "built $CB_BOT_BIN"
ls -l --time-style=+%Y-%m-%d\ %H:%M:%S "$CB_BOT_BIN"

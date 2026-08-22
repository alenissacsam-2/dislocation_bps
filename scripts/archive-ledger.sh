#!/usr/bin/env bash
# Move the active ledger aside under a new name, WAL and shm with it.
#
# A SQLite database in WAL mode is three files, and copying only the .db leaves the
# most recent writes behind in the -wal. Renaming all three together keeps the
# archive complete and lets the next run start on a clean database.
set -euo pipefail
to="${1:?usage: archive-ledger.sh <new-name-without-extension>}"
for suffix in "" "-wal" "-shm"; do
  src="cryptobot.db${suffix}"
  [ -f "$src" ] && mv "$src" "${to}.db${suffix}" && echo "moved $src -> ${to}.db${suffix}"
done
exit 0

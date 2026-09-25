#!/usr/bin/env bash
# Measure the round trip from this machine to the RPC provider and each Jito block
# engine. See docs/vps.md.
#
# Times the TCP handshake, which is exactly one round trip and sends no API request:
# Jito allows one request a second per IP across every region, and a probe that spent
# that allowance would throttle a bot running on the same machine. Median of five.
set -euo pipefail

rtt() {
  local host=$1 i out=()
  for i in 1 2 3 4 5; do
    out+=("$(curl -s -o /dev/null -w '%{time_connect}' --max-time 5 "https://$host/" || echo 9)")
    sleep 0.3
  done
  printf '%s\n' "${out[@]}" | sort -n | sed -n 3p
}

printf '%-44s %s\n' "endpoint" "round trip (s)"
printf '%-44s %s\n' "mainnet.helius-rpc.com" "$(rtt mainnet.helius-rpc.com)"
for region in amsterdam frankfurt london ny slc tokyo singapore; do
  host="$region.mainnet.block-engine.jito.wtf"
  printf '%-44s %s\n' "$host" "$(rtt "$host")"
done

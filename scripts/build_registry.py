#!/usr/bin/env python3
"""Generate the pool registry the bot subscribes to.

Venue APIs are used as a **directory** — which pools exist, what they trade, roughly
how deep they are. Nothing here ends up in a quote. Every number that touches money
(reserves, sqrt price, liquidity, fee) is read from the chain at run time by the
decoders in `crates/dex`. The `fee_ppm` and `tvl_usd` fields written below are for
display and ordering only, and the bot overwrites `fee_ppm` with the on-chain value.

Three filters do the real work:

1. **Classic SPL Token program only.** Token-2022 mints can carry transfer fees and
   transfer hooks that silently skim a swap. A route through one would show a profit
   in our arithmetic and a loss in the wallet.

2. **Degree pruning.** A mint that appears in only one pool cannot be an intermediate
   hop in any cycle — you can get in but never out by another road. Pruning those
   repeatedly until the graph stops shrinking removes pools that could never
   contribute to a cycle, which is most of the long tail.

3. **A subscription budget.** Concentrated-liquidity pools cost one WebSocket
   subscription each because their price lives in the pool account. Raydium AMM v4
   costs three, since its reserves live in two separate vaults. The budget is spent
   accordingly.

Usage:  python3 scripts/build_registry.py [--budget 120] [--min-tvl 150000]
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import urllib.request
from collections import defaultdict

SPL_TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"

SOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
USDT = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"

# Tokens we can actually start and end a cycle in, because we can hold them.
BASE_MINTS = [SOL, USDC, USDT]

ORCA_POOLS = "https://api.orca.so/v2/solana/pools"
RAYDIUM_POOLS = "https://api-v3.raydium.io/pools/info/list"

# One subscription per concentrated pool; three per constant-product pool, whose
# reserves live in two separate vault accounts alongside the pool.
SUB_COST = {
    "orca_whirlpool": 1,
    "raydium_clmm": 1,
    "raydium_v4": 3,
    "raydium_cpmm": 3,
    "meteora_damm_v2": 1,
}

# Meteora DAMM v2 has no directory API, so its pools come from the chain itself. That
# is the better source anyway: `getProgramAccounts` cannot describe a pool that does
# not exist, and every number below is read from the account rather than reported.
DAMM_V2_PROGRAM = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"
DAMM_V2_LEN = 1112
RPC_URL = "https://api.mainnet-beta.solana.com"

# Raydium runs four separate AMM programs and its API calls three of them "Standard".
# They share no account layout. Keying the decoder off the API's `type` field rather
# than the program id would have fed a stable-swap pool to the constant-product
# decoder: the account is long enough to pass the length check, so it would decode to
# plausible-looking nonsense rather than failing.
RAYDIUM_PROGRAMS = {
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": "raydium_v4",
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C": "raydium_cpmm",
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK": "raydium_clmm",
    # 5quBtoiQqxF9Jv6KYKctB59NT3gtJD2Y65kdnB1Uev3h is Raydium's stable-swap AMM. It
    # quotes USDT/USDC at 2 bp, which is tempting, but its invariant is not constant
    # product and we have no decoder for it. Left out deliberately rather than
    # silently mis-priced.
}


def get(url: str) -> dict:
    req = urllib.request.Request(url, headers={"User-Agent": "cryptobot-registry/1"})
    with urllib.request.urlopen(req, timeout=90) as r:
        return json.load(r)


def fetch_orca(pages: int) -> list[dict]:
    out, seen = [], set()
    for token in BASE_MINTS:
        for page in range(pages):
            url = (
                f"{ORCA_POOLS}?token={token}&limit=50&sortBy=tvl"
                f"&sortDirection=desc&offset={page * 50}"
            )
            try:
                data = get(url).get("data", [])
            except Exception as e:  # noqa: BLE001 - a dead page should not kill the run
                print(f"  orca page {page} for {token[:6]} failed: {e}", file=sys.stderr)
                break
            if not data:
                break
            for p in data:
                if p["address"] in seen:
                    continue
                seen.add(p["address"])
                out.append(
                    {
                        "address": p["address"],
                        "dex": "orca_whirlpool",
                        "mint_a": p["tokenA"]["address"],
                        "mint_b": p["tokenB"]["address"],
                        "sym_a": p["tokenA"]["symbol"].strip(),
                        "sym_b": p["tokenB"]["symbol"].strip(),
                        "dec_a": p["tokenA"]["decimals"],
                        "dec_b": p["tokenB"]["decimals"],
                        "prog_a": p["tokenA"]["programId"],
                        "prog_b": p["tokenB"]["programId"],
                        "fee_ppm": int(p["feeRate"]),
                        "tvl_usd": float(p["tvlUsdc"]),
                        "pool_type": p.get("poolType", ""),
                        # Orca overloads tickSpacingSeed on adaptive-fee pools; the
                        # decoder rejects those, so drop them here too.
                        "tick_spacing": p.get("tickSpacing"),
                    }
                )
    return out


def fetch_raydium(pages: int) -> list[dict]:
    out, seen = [], set()
    unknown_programs: dict[str, int] = defaultdict(int)
    for pool_type in ("concentrated", "standard"):
        for page in range(1, pages + 1):
            url = (
                f"{RAYDIUM_POOLS}?poolType={pool_type}&poolSortField=liquidity"
                f"&sortType=desc&pageSize=100&page={page}"
            )
            try:
                body = get(url)["data"]["data"]
            except Exception as e:  # noqa: BLE001
                print(f"  raydium {pool_type} page {page} failed: {e}", file=sys.stderr)
                break
            if not body:
                break
            for p in body:
                if p["id"] in seen:
                    continue
                seen.add(p["id"])
                dex = RAYDIUM_PROGRAMS.get(p.get("programId", ""))
                if dex is None:
                    unknown_programs[p.get("programId", "?")] += 1
                    continue
                cfg = p.get("config") or {}
                fee_ppm = int(cfg.get("tradeFeeRate", round(float(p["feeRate"]) * 1_000_000)))
                out.append(
                    {
                        "address": p["id"],
                        "dex": dex,
                        "mint_a": p["mintA"]["address"],
                        "mint_b": p["mintB"]["address"],
                        "sym_a": p["mintA"]["symbol"].strip(),
                        "sym_b": p["mintB"]["symbol"].strip(),
                        "dec_a": p["mintA"]["decimals"],
                        "dec_b": p["mintB"]["decimals"],
                        "prog_a": p["mintA"]["programId"],
                        "prog_b": p["mintB"]["programId"],
                        "fee_ppm": fee_ppm,
                        "tvl_usd": float(p.get("tvl") or 0.0),
                        "pool_type": p["type"],
                        "has_dynamic_fee": bool(p.get("hasDynamicFee")),
                    }
                )
    for prog, n in sorted(unknown_programs.items(), key=lambda kv: -kv[1]):
        print(f"  skipped {n:>4} pools on unsupported program {prog}", file=sys.stderr)
    return out


_B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def base58(raw: bytes) -> str:
    """Encode 32 raw bytes as a Solana address."""
    n = int.from_bytes(raw, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = _B58[r] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


def rpc(method: str, params: list) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    req = urllib.request.Request(
        RPC_URL, data=body.encode(), headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        out = json.load(r)
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out


def _u(d: bytes, o: int, n: int) -> int:
    return int.from_bytes(d[o : o + n], "little")


def fetch_meteora_damm_v2(known: dict[str, dict]) -> list[dict]:
    """Discover DAMM v2 pools on mints we already track, straight from the chain.

    Two passes on purpose. There are tens of thousands of these pools, nearly all of
    them dust from token launches, and pulling 1112 bytes for every one would move
    tens of megabytes to discard almost all of it. So the first pass asks only for the
    64 bytes holding the two mints, and the second reads in full only the pools whose
    pair we can actually route through.
    """
    out: list[dict] = []
    candidates: dict[str, tuple[str, str]] = {}

    for quote in BASE_MINTS:
        try:
            res = rpc("getProgramAccounts", [DAMM_V2_PROGRAM, {
                "encoding": "base64",
                "commitment": "processed",
                "filters": [{"dataSize": DAMM_V2_LEN}, {"memcmp": {"offset": 200, "bytes": quote}}],
                "dataSlice": {"offset": 168, "length": 64},
            }])["result"]
        except Exception as e:  # noqa: BLE001 - one dead quote must not kill the build
            print(f"  damm v2: {quote[:8]}… lookup failed: {e}", file=sys.stderr)
            continue
        hits = 0
        for a in res:
            d = base64.b64decode(a["account"]["data"][0])
            mint_a = base58(d[0:32])
            if mint_a in known and mint_a != quote:
                candidates[a["pubkey"]] = (mint_a, quote)
                hits += 1
        print(f"  damm v2: {len(res):>6} pools quoted in {known.get(quote, {}).get('sym', quote[:6])}"
              f", {hits} on pairs we track", file=sys.stderr)

    # Second pass: full accounts, in batches, for the shortlist only.
    addrs = list(candidates)
    for i in range(0, len(addrs), 100):
        batch = addrs[i : i + 100]
        vals = rpc("getMultipleAccounts", [batch, {"encoding": "base64", "commitment": "processed"}])
        for addr, v in zip(batch, vals["result"]["value"]):
            if not v:
                continue
            d = base64.b64decode(v["data"][0])
            if len(d) < DAMM_V2_LEN:
                continue
            mint_a, mint_b = candidates[addr]
            ka, kb = known[mint_a], known[mint_b]
            amt_a = _u(d, 680, 8) / 10 ** ka["dec"]
            amt_b = _u(d, 688, 8) / 10 ** kb["dec"]
            # Value the quote side and double it. Every quote here is a BASE_MINT, so
            # its price is either $1 or comes from the anchor pass in main().
            out.append({
                "address": addr,
                "dex": "meteora_damm_v2",
                "mint_a": mint_a,
                "mint_b": mint_b,
                "sym_a": ka["sym"],
                "sym_b": kb["sym"],
                "dec_a": ka["dec"],
                "dec_b": kb["dec"],
                "prog_a": SPL_TOKEN if d[482] == 0 else "token-2022",
                "prog_b": SPL_TOKEN if d[483] == 0 else "token-2022",
                "fee_ppm": -(-_u(d, 8, 8) // 1_000),   # numerator over 1e9, rounded up
                "tvl_usd": 0.0,                        # filled once prices are known
                "_amt_a": amt_a,
                "_amt_b": amt_b,
                # Price of A in B, from sqrt_price. NOT from the balance ratio: a
                # concentrated pool holds its two tokens in a ratio set by where spot
                # sits inside its range, so balances imply a price only for a
                # full-range pool. Reading it that way put SOL at $32 against a real
                # $99, because the deepest pool happened to be a narrow one.
                "_price_ab": (_u(d, 456, 16) / 2**64) ** 2 * 10 ** (ka["dec"] - kb["dec"]),
                "pool_type": "damm_v2",
                # A dynamic fee or a fee schedule means the stored base fee is not the
                # price. The decoder refuses both; reject here so the pool never
                # occupies a subscription in the first place.
                "has_dynamic_fee": d[56] != 0 or any(d[16:48]),
                "disabled": d[481] != 0,
            })
    return out


def price_damm_v2(pools: list[dict]) -> dict[str, float]:
    """Fill in TVL for DAMM v2 pools, in place, and return the prices used.

    Self-contained on purpose: the pools were read from the chain, so their own state
    implies every price needed. A stablecoin-quoted pool prices its base mint, and
    those prices then value everything quoted in one of them. No API is asked for a
    price and no dollar value is hardcoded.
    """
    usd: dict[str, float] = {USDC: 1.0, USDT: 1.0}
    damm = [p for p in pools if p["dex"] == "meteora_damm_v2"]

    def tvl(p: dict) -> float:
        """Both sides valued separately — a concentrated pool is not 50/50."""
        return p["_amt_a"] * usd.get(p["mint_a"], 0.0) + p["_amt_b"] * usd.get(p["mint_b"], 0.0)

    # Two rounds outward from the stablecoins: the first prices whatever they quote,
    # the second values pools quoted in those. Deepest quote wins each mint, since a
    # thin pool can sit far from the market with nobody bothering to correct it.
    for _ in range(2):
        implied: dict[str, list[tuple[float, float]]] = defaultdict(list)
        for p in damm:
            if p["mint_a"] not in usd and p["mint_b"] in usd and p["_price_ab"] > 0:
                implied[p["mint_a"]].append((tvl(p), p["_price_ab"] * usd[p["mint_b"]]))
        if not implied:
            break
        for mint, quotes in implied.items():
            usd[mint] = max(quotes)[1]

    for p in damm:
        p["tvl_usd"] = tvl(p)
        for k in ("_amt_a", "_amt_b", "_price_ab"):
            p.pop(k, None)
    return usd


def usable(p: dict, min_tvl: float) -> str | None:
    """Return a rejection reason, or None if the pool is usable."""
    if p["prog_a"] != SPL_TOKEN or p["prog_b"] != SPL_TOKEN:
        return "token-2022 mint (may carry transfer fees or hooks)"
    if p["tvl_usd"] < min_tvl:
        return f"tvl ${p['tvl_usd']:,.0f} below floor"
    if p["mint_a"] == p["mint_b"]:
        return "degenerate pair"
    if p.get("has_dynamic_fee"):
        return "dynamic fee or fee schedule not read from chain"
    if p.get("disabled"):
        return "swaps disabled on the pool"
    if p["dex"] == "orca_whirlpool" and p["pool_type"] != "whirlpool":
        return f"unsupported orca pool type {p['pool_type']!r}"
    if not (0 <= p["fee_ppm"] < 1_000_000):
        return f"implausible fee {p['fee_ppm']}ppm"
    return None


def prune_to_cycles(pools: list[dict]) -> list[dict]:
    """Drop pools whose mints cannot participate in any closed cycle.

    A mint reachable through exactly one pool is a dead end: a cycle entering it has
    no second road out. Removing such mints can strand others, so this repeats until
    the graph stops changing.
    """
    pools = list(pools)
    while True:
        degree: dict[str, set[str]] = defaultdict(set)
        for p in pools:
            degree[p["mint_a"]].add(p["address"])
            degree[p["mint_b"]].add(p["address"])
        dead = {m for m, ps in degree.items() if len(ps) < 2 and m not in BASE_MINTS}
        if not dead:
            return pools
        before = len(pools)
        pools = [p for p in pools if p["mint_a"] not in dead and p["mint_b"] not in dead]
        if len(pools) == before:
            return pools


def choose(pools: list[dict], budget: int, max_v4: int) -> list[dict]:
    """Spend the subscription budget on the pools most likely to matter.

    Ranked by cheapness first, depth second. Fee tier leads because a 75 bp round trip
    is unwinnable at any speed while an 8 bp one is not — so a cheap shallow pool is
    worth more to us than a deep expensive one. Depth barely constrains a $5 trade,
    and a thinner pool is if anything *more* likely to be dislocated.

    Raydium AMM v4 is capped separately. At 25 bp it can only clear against an
    unusually large dislocation, and at three subscriptions per pool it is eight times
    the cost of a concentrated pool. A handful of the deepest ones earn their place:
    they carry the retail flow, which is what pushes a price out of line in the first
    place, so they are the venue most likely to be the *wrong* side of a real
    dislocation.
    """
    ranked = sorted(pools, key=lambda p: (p["fee_ppm"], -p["tvl_usd"]))
    v4 = [p for p in ranked if p["dex"] == "raydium_v4"]
    keep_v4 = {p["address"] for p in sorted(v4, key=lambda p: -p["tvl_usd"])[:max_v4]}

    spent, chosen = 0, []
    for p in ranked:
        if p["dex"] == "raydium_v4" and p["address"] not in keep_v4:
            continue
        cost = SUB_COST[p["dex"]]
        if spent + cost > budget:
            continue
        chosen.append(p)
        spent += cost
    return chosen


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--budget", type=int, default=120, help="websocket subscription budget")
    ap.add_argument("--min-tvl", type=float, default=150_000.0)
    ap.add_argument("--pages", type=int, default=3)
    ap.add_argument("--max-v4", type=int, default=6, help="cap on Raydium AMM v4 pools")
    ap.add_argument("--out", default="crates/bot/pools.json")
    ap.add_argument("--no-damm-v2", action="store_true", help="skip the chain scan")
    args = ap.parse_args()

    print("fetching venue directories...", file=sys.stderr)
    candidates = fetch_orca(args.pages) + fetch_raydium(args.pages)
    print(f"  {len(candidates)} pools listed", file=sys.stderr)

    if not args.no_damm_v2:
        # DAMM v2 has no directory API. It needs to know which mints are worth
        # looking at, which the API venues have already told us.
        known: dict[str, dict] = {}
        for p in candidates:
            known.setdefault(p["mint_a"], {"sym": p["sym_a"], "dec": p["dec_a"]})
            known.setdefault(p["mint_b"], {"sym": p["sym_b"], "dec": p["dec_b"]})
        print("scanning Meteora DAMM v2 on chain...", file=sys.stderr)
        try:
            damm = fetch_meteora_damm_v2(known)
            usd = price_damm_v2(damm)
            candidates += damm
            print(f"  {len(damm)} damm v2 pools on tracked pairs"
                  f" (SOL priced at ${usd.get(SOL, 0):,.2f} from their own balances)",
                  file=sys.stderr)
        except Exception as e:  # noqa: BLE001 - the other venues are still usable
            print(f"  damm v2 discovery failed, continuing without it: {e}", file=sys.stderr)

    rejects: dict[str, int] = defaultdict(int)
    kept = []
    for p in candidates:
        why = usable(p, args.min_tvl)
        if why:
            rejects[why.split("(")[0].strip()] += 1
        else:
            kept.append(p)
    print(f"  {len(kept)} pass the filters", file=sys.stderr)
    for why, n in sorted(rejects.items(), key=lambda kv: -kv[1])[:6]:
        print(f"    rejected {n:>5}  {why}", file=sys.stderr)

    cyclable = prune_to_cycles(kept)
    print(f"  {len(cyclable)} can appear in a cycle", file=sys.stderr)

    chosen = prune_to_cycles(choose(cyclable, args.budget, args.max_v4))
    print(f"  {len(chosen)} fit the subscription budget", file=sys.stderr)

    mints: dict[str, dict] = {}
    for p in chosen:
        mints.setdefault(p["mint_a"], {"symbol": p["sym_a"], "decimals": p["dec_a"]})
        mints.setdefault(p["mint_b"], {"symbol": p["sym_b"], "decimals": p["dec_b"]})

    registry = {
        "note": (
            "Generated by scripts/build_registry.py. fee_ppm and tvl_usd are metadata "
            "for display and ordering; the bot reads the authoritative fee and all "
            "pricing state from chain."
        ),
        "base_mints": BASE_MINTS,
        "subscriptions": sum(SUB_COST[p["dex"]] for p in chosen),
        "mints": mints,
        "pools": [
            {
                "address": p["address"],
                "dex": p["dex"],
                "label": f"{p['sym_a']}/{p['sym_b']}",
                "mint_a": p["mint_a"],
                "mint_b": p["mint_b"],
                "fee_ppm": p["fee_ppm"],
                "tvl_usd": round(p["tvl_usd"], 2),
            }
            for p in sorted(chosen, key=lambda p: (p["fee_ppm"], -p["tvl_usd"]))
        ],
    }

    with open(args.out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(registry, f, indent=2)
        f.write("\n")

    by_dex: dict[str, int] = defaultdict(int)
    by_fee: dict[int, int] = defaultdict(int)
    for p in chosen:
        by_dex[p["dex"]] += 1
        by_fee[p["fee_ppm"]] += 1
    print(f"\nwrote {args.out}", file=sys.stderr)
    print(f"  {len(chosen)} pools, {len(mints)} mints, {registry['subscriptions']} subscriptions", file=sys.stderr)
    print(f"  by venue: {dict(by_dex)}", file=sys.stderr)
    print("  by fee tier (ppm -> pools): "
          + ", ".join(f"{k}:{v}" for k, v in sorted(by_fee.items())[:10]), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

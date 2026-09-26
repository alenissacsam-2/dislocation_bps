#!/usr/bin/env node
// Arbitrage census: where do closed-loop arbitrages actually land, and how long had
// the state they traded against been sitting there?
//
//   CRYPTOBOT_RPC_HTTP_URL=... node scripts/census.cjs [runs] [blocks-per-run] [slots-between-runs]
//
// Reads runs of consecutive blocks in full (jsonParsed, with logs), gzip on the wire. For
// every successful transaction whose fee payer ends with more SOL, USDC or USDT and
// exactly the same amount of every other token, and whose logs show at least two swaps
// (two pools whose holdings each rose in one asset and fell in another), it
// records the venues, the tip, the profit, whether it touched a pool this bot watches,
// and its AGE: how many slots before it the most recently changed account it wrote to
// had last been written by anyone else.
//
// Age is the number that decides what a home setup can win. Age 0 is a backrun inside
// the block that created the opportunity, which needs to be colocated with the leader.
// Age 1 or more means the state stood for at least a slot, which a next-slot sender
// can reach.
//
// The two-swap rule is what separates arbitrage from a sale that closes its token
// account or a creator collecting fees: both also end with more SOL and no tokens, and
// the first version of this census counted them.
//
// Bandwidth: roughly 0.35 MB per block compressed. Run large censuses while the
// bot is stopped; the download competes with its sends on a home connection.

const fs = require("fs");
const path = require("path");

const RPC = process.env.CRYPTOBOT_RPC_HTTP_URL;
if (!RPC) {
  console.error("set CRYPTOBOT_RPC_HTTP_URL");
  process.exit(2);
}
const RUNS = +(process.argv[2] || 2);
const PER_RUN = +(process.argv[3] || 8);
const SPACING = +(process.argv[4] || 300);
const WARMUP = 2; // blocks at the start of each run that only build history
// Pause between blocks, so a census can run beside the live bot without crowding its
// sends on a home connection.
const THROTTLE_MS = +(process.env.THROTTLE_MS || 0);

const WSOL = "So11111111111111111111111111111111111111112";
const STABLES = {
  EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v: "USDC",
  Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB: "USDT",
};
const VENUES = {
  "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": "raydium_v4",
  CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK: "raydium_clmm",
  CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C: "raydium_cpmm",
  whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc: "orca",
  LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo: "meteora_dlmm",
  cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG: "meteora_damm2",
  Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB: "meteora_damm1",
  pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA: "pumpswap",
  "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P": "pumpfun",
  PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY: "phoenix",
  opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb: "openbook2",
  JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4: "jupiter",
  MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms: "manifest",
};
// Programs that never price a swap, so never count as one.
const UTILITY = new Set([
  "11111111111111111111111111111111",
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
  "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
  "ComputeBudget111111111111111111111111111111",
  "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
  "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo",
  "AddressLookupTab1e1111111111111111111111111",
  "T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt",
]);

// Programs a transaction invoked from inside another, other than utilities and the
// known venues: for labelling only, since a helper (pump.fun's fee calculator, which
// PumpSwap calls on every trade) looks exactly like a swap venue from the logs. A
// program invoking itself is an Anchor event being emitted.
function calleesIn(logs) {
  const stack = [];
  let swaps = 0;
  const unknown = [];
  for (const l of logs || []) {
    const m = /^Program (\w+) invoke \[(\d+)\]/.exec(l);
    if (m) {
      const [, id, d] = m;
      const depth = +d;
      stack.length = depth - 1;
      const parent = stack[depth - 2];
      stack[depth - 1] = id;
      if (parent === id || UTILITY.has(id) || id.startsWith("pfee")) continue;
      if (VENUES[id]) swaps++;
      else if (depth >= 2) { swaps++; unknown.push(id); }
    }
  }
  return { swaps, unknown };
}

// How many pools traded in a transaction, counted from balances rather than programs:
// an account holder other than the fee payer whose holdings rose in one asset and fell
// in another. Program-agnostic, so it counts venues nobody has labelled, and a fee
// collector (which only gains) or a helper program (which holds nothing) never counts.
// A pool that keeps SOL as lamports rather than in a token account (pump.fun's bonding
// curve) is caught by its own lamport change.
function poolsSwapped(tx, payer, keys) {
  const m = tx.meta;
  const byOwner = new Map();
  const add = (owner, mint, v) => {
    if (!owner || owner === payer || v === 0n) return;
    let o = byOwner.get(owner);
    if (!o) byOwner.set(owner, (o = new Map()));
    o.set(mint, (o.get(mint) || 0n) + v);
  };
  const tokenIdx = new Set();
  for (const b of m.preTokenBalances || []) { tokenIdx.add(b.accountIndex); add(b.owner, b.mint, -BigInt(b.uiTokenAmount.amount)); }
  for (const b of m.postTokenBalances || []) { tokenIdx.add(b.accountIndex); add(b.owner, b.mint, BigInt(b.uiTokenAmount.amount)); }
  keys.forEach((k, i) => {
    if (i === 0 || k.signer || tokenIdx.has(i) || TIP_ACCOUNTS.has(k.key)) return;
    add(k.key, WSOL, BigInt(m.postBalances[i]) - BigInt(m.preBalances[i]));
  });
  let n = 0;
  for (const o of byOwner.values()) {
    let up = false, down = false;
    for (const v of o.values()) { if (v > 0n) up = true; if (v < 0n) down = true; }
    if (up && down) n++;
  }
  return n;
}

// Programs and sysvars that are never a pool, so never an arbitrage's "trigger".
const NOT_STATE = new Set([
  "11111111111111111111111111111111",
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
  "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
  "ComputeBudget111111111111111111111111111111",
  "SysvarRent111111111111111111111111111111111",
  "SysvarC1ock11111111111111111111111111111111",
  "Sysvar1nstructions1111111111111111111111111",
  WSOL,
]);
const TIP_ACCOUNTS = new Set([
  "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
  "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
  "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
  "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
  "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
  "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
  "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
  "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
]);

const registry = new Set(
  JSON.parse(fs.readFileSync(path.join(__dirname, "..", "crates", "bot", "pools.json"), "utf8"))
    .pools.map((p) => p.address),
);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function call(method, params) {
  for (let i = 0; i < 5; i++) {
    try {
      const r = await fetch(RPC, {
        method: "POST",
        headers: { "content-type": "application/json", "accept-encoding": "gzip" },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
      });
      const j = await r.json();
      if (j.error) {
        // Skipped slots and blocks not yet available are answers, not failures.
        if (/skipped|not available|missing/i.test(j.error.message || "")) return null;
        throw new Error(j.error.message);
      }
      return j.result;
    } catch (e) {
      if (i === 4) throw e;
      await sleep(1000 * (i + 1));
    }
  }
}

function keysOf(tx) {
  const keys = tx.transaction.message.accountKeys.map((k) => ({
    key: k.pubkey,
    signer: k.signer,
    writable: k.writable,
  }));
  return keys;
}

// The fee payer's change in every asset, SOL counting lamports and wrapped SOL together.
function deltas(tx, payer, keys) {
  const m = tx.meta;
  const out = {};
  const add = (mint, v) => (out[mint] = (out[mint] || 0n) + v);
  for (const b of m.preTokenBalances || []) if (b.owner === payer) add(b.mint, -BigInt(b.uiTokenAmount.amount));
  for (const b of m.postTokenBalances || []) if (b.owner === payer) add(b.mint, BigInt(b.uiTokenAmount.amount));
  let lam = BigInt(m.postBalances[0]) - BigInt(m.preBalances[0]);
  // Rent coming home is not profit. A token account of the payer that existed before
  // and is closed here hands its rent back, which the first census read as a
  // 1,488,440-lamport "arbitrage". Wrapped SOL inside it is already counted below.
  for (const b of m.preTokenBalances || []) {
    if (b.owner !== payer) continue;
    const i = b.accountIndex;
    if (BigInt(m.postBalances[i]) !== 0n) continue;
    let rent = BigInt(m.preBalances[i]);
    if (b.mint === WSOL) rent -= BigInt(b.uiTokenAmount.amount);
    lam -= rent;
  }
  add("SOL", lam + (out[WSOL] || 0n));
  delete out[WSOL];
  // Tips leave the payer as lamports; measured on the receiving side.
  let tip = 0n;
  keys.forEach((k, i) => {
    if (TIP_ACCOUNTS.has(k.key)) tip += BigInt(m.postBalances[i]) - BigInt(m.preBalances[i]);
  });
  return { d: out, fee: BigInt(m.fee), tip };
}

function classify(tx) {
  if (tx.meta.err) return null;
  const keys = keysOf(tx);
  if (poolsSwapped(tx, keys[0].key, keys) < 2) return null;
  const { unknown } = calleesIn(tx.meta.logMessages);
  const payer = keys[0].key;
  const venues = [...new Set([...keys.map((k) => VENUES[k.key]).filter(Boolean), ...unknown.map((u) => "?" + u.slice(0, 6))])];
  const { d, fee, tip } = deltas(tx, payer, keys);
  const gainSol = d.SOL + fee + tip; // before the fee and the tip
  const others = Object.entries(d).filter(([mint]) => mint !== "SOL");
  const nonzero = others.filter(([, v]) => v !== 0n);
  let asset = null, gross = 0n;
  if (nonzero.length === 0 && gainSol > 0n) {
    asset = "SOL"; gross = gainSol;
  } else if (nonzero.length === 1 && STABLES[nonzero[0][0]] && nonzero[0][1] > 0n && gainSol >= -2_500_000n) {
    // Profit taken in a stable; SOL may still fund the fee, tip and a little rent.
    asset = STABLES[nonzero[0][0]]; gross = nonzero[0][1];
  }
  if (!asset) return null;
  // Exclude whale-sized "profits": those are deposits or withdrawals, not arbitrage.
  if (asset === "SOL" && gross > 50_000_000_000n) return null;
  return { payer, venues, asset, gross, fee, tip, keys };
}

(async () => {
  const head = await call("getSlot", [{ commitment: "finalized" }]);
  const arbs = [];
  let blocks = 0, txs = 0, failedDex = 0;
  for (let r = 0; r < RUNS; r++) {
    const start = head - 50 - r * SPACING - PER_RUN;
    const touched = new Map(); // account -> [slot, index] of its last successful write
    for (let s = start; s < start + PER_RUN; s++) {
      const b = await call("getBlock", [s, {
        encoding: "jsonParsed", transactionDetails: "full", rewards: false,
        maxSupportedTransactionVersion: 1, commitment: "finalized",
      }]);
      if (!b) continue;
      blocks++;
      const warm = s - start >= WARMUP;
      b.transactions.forEach((tx, idx) => {
        txs++;
        const keys = keysOf(tx);
        if (tx.meta.err) {
          if (keys.some((k) => VENUES[k.key])) failedDex++;
          return;
        }
        const a = warm ? classify(tx) : null;
        if (a) {
          // Age: slots since the most recent write by anyone else to any account this
          // arbitrage wrote. Nothing in the run's history means "older than the run".
          let youngest = null;
          for (const k of a.keys) {
            if (!k.writable || k.signer || NOT_STATE.has(k.key) || TIP_ACCOUNTS.has(k.key)) continue;
            const t = touched.get(k.key);
            if (t && t.payer !== a.payer && (youngest === null || t.slot > youngest)) youngest = t.slot;
          }
          arbs.push({
            slot: s, sig: tx.transaction.signatures[0], venues: a.venues.sort().join("+"), asset: a.asset, gross: a.gross,
            net: a.asset === "SOL" ? a.gross - a.fee - a.tip : null,
            // Every account it wrote but did not sign for: its pools, vaults and arrays.
            // scripts/watchlist.cjs resolves which of them are pools.
            writes: a.keys.filter((k) => k.writable && !k.signer && !TIP_ACCOUNTS.has(k.key) && !NOT_STATE.has(k.key)).map((k) => k.key),
            fee: a.fee, tip: a.tip, payer: a.payer,
            watched: a.keys.some((k) => registry.has(k.key)),
            age: youngest === null ? `>${s - start}` : s - youngest,
          });
        }
        const payer = keys[0].key;
        for (const k of keys) if (k.writable && !k.signer) touched.set(k.key, { slot: s, payer });
      });
      process.stderr.write(".");
      if (THROTTLE_MS) await sleep(THROTTLE_MS);
    }
  }
  process.stderr.write("\n");

  const n = arbs.length;
  const by = (f) => arbs.reduce((m, a) => ((m[f(a)] = (m[f(a)] || 0) + 1), m), {});
  const median = (xs) => { const v = [...xs].sort((x, y) => (x < y ? -1 : x > y ? 1 : 0)); return v.length ? v[v.length >> 1] : 0n; };
  const sol = arbs.filter((a) => a.asset === "SOL");
  console.log(`blocks ${blocks}, transactions ${txs}, failed DEX transactions ${failedDex}`);
  console.log(`closed-loop arbitrages ${n} (${(n / Math.max(blocks - RUNS * WARMUP, 1)).toFixed(2)} per block)`);
  console.log("by age in slots:", by((a) => String(a.age)));
  console.log("by venues:", Object.entries(by((a) => a.venues)).sort((x, y) => y[1] - x[1]).slice(0, 15));
  console.log("profit asset:", by((a) => a.asset));
  console.log("touching a pool this bot watches:", arbs.filter((a) => a.watched).length);
  console.log("distinct arbitrageurs:", new Set(arbs.map((a) => a.payer)).size);
  if (sol.length) {
    console.log(`SOL-profit arbitrages: median gross ${median(sol.map((a) => a.gross))} lamports, median tip ${median(sol.map((a) => a.tip))}, median fee ${median(sol.map((a) => a.fee))}`);
    const old = sol.filter((a) => typeof a.age !== "number" || a.age >= 1);
    console.log(`  of which age >= 1 slot: ${old.length}, median gross ${median(old.map((a) => a.gross))} lamports`);
  }
  // What the winners kept, by how old the state was and by whether every venue is one
  // this bot can already execute on. SOL-profit arbitrages only: the rest are in a
  // stable and would need a price to compare.
  const EXEC = new Set(["orca", "raydium_clmm", "raydium_v4", "meteora_dlmm", "jupiter"]);
  const sum = (xs) => xs.reduce((t, a) => t + a.net, 0n);
  const classes = {
    "same block (age 0)": sol.filter((a) => a.age === 0),
    "next block or later": sol.filter((a) => a.age !== 0),
  };
  const warmBlocks = Math.max(blocks - RUNS * WARMUP, 1);
  for (const [name, xs] of Object.entries(classes)) {
    const exec = xs.filter((a) => a.venues.split("+").every((v) => EXEC.has(v)));
    console.log(`${name}: ${xs.length} arbitrages, winners kept ${sum(xs)} lamports (${(Number(sum(xs)) / warmBlocks).toFixed(0)} per block); on venues this bot executes: ${exec.length}, ${sum(exec)} lamports`);
  }
  const unknown = {};
  for (const a of arbs) for (const v of a.venues.split("+")) if (v.startsWith("?")) unknown[v] = (unknown[v] || 0) + 1;
  console.log("unlabelled programs called from inside arbitrages:", Object.entries(unknown).sort((x, y) => y[1] - x[1]).slice(0, 12));
  const out = path.join(process.env.TEMP || "/tmp", `census-${Date.now()}.json`);
  fs.writeFileSync(out, JSON.stringify(arbs, (k, v) => (typeof v === "bigint" ? v.toString() : v), 1));
  console.log("rows written to", out);
})();

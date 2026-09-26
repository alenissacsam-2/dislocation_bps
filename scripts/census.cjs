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

const { WSOL, VENUES, NOT_STATE, TIP_ACCOUNTS, keysOf, classify } = require("./lib/arbs.cjs");

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

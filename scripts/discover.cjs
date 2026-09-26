#!/usr/bin/env node
// Follow live arbitrage to the pools it runs through, and stream the new ones.
//
//   CRYPTOBOT_RPC_HTTP_URL=... node scripts/discover.cjs [--every-ms 20000] [--once]
//
// Every interval it reads one freshly confirmed block, recognises closed-loop
// arbitrage the way scripts/census.cjs does (scripts/lib/arbs.cjs), and keeps each
// arbitrage that ran through at least two pools on venues the bot trades. Pools it has
// not reported before — and that neither the embedded registry nor watchlist.json
// already holds — are printed to stdout as one JSON line in the registry's own format:
//
//   {"pools":[{"address","dex","label","mint_a","mint_b","fee_ppm","tvl_usd","seen"}],
//    "mints":{"<mint>":{"symbol","decimals","token_program"?}}}
//
// The bot runs this as a child process and adds what it prints to the pools it
// watches, without a restart.
//
// # Why
//
// A watchlist built from a census stops describing the market within hours: pools
// rotate with the tokens that are trading. On 2026-09-27 a watchlist built from the
// two censuses of the day before covered 5 of the 74 arbitrages the bot could have
// finished in a census twelve hours later, while arbitrage keeps returning to the same
// pools within minutes (257 of 362 came more than a minute after their pool's first).
//
// Bandwidth: one block is about 0.35 MB compressed; at the default interval that is
// about 1 MB a minute.

const fs = require("fs");
const path = require("path");
const { TIP_ACCOUNTS, NOT_STATE, keysOf, classify } = require("./lib/arbs.cjs");

const RPC = process.env.CRYPTOBOT_RPC_HTTP_URL;
if (!RPC) {
  console.error("set CRYPTOBOT_RPC_HTTP_URL");
  process.exit(2);
}
const args = process.argv.slice(2);
const flag = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const EVERY_MS = +flag("--every-ms", 20000);
const ONCE = args.includes("--once");

// Pool accounts by owning program and exact size, with where their two mints sit —
// the same table scripts/watchlist.cjs uses, and the bot's own decoders' offsets.
const VENUE = {
  CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK: { dex: "raydium_clmm", size: (n) => n === 1544, a: 73, b: 105 },
  whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc: { dex: "orca_whirlpool", size: (n) => n === 653, a: 101, b: 181 },
  LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo: { dex: "meteora_dlmm", size: (n) => n === 904, a: 88, b: 120 },
  "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": { dex: "raydium_v4", size: (n) => n === 752, a: 400, b: 432 },
  cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG: { dex: "meteora_damm_v2", size: (n) => n === 1112, a: 168, b: 200 },
  pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA: { dex: "pumpswap", size: (n) => n >= 200 && n <= 400, a: 43, b: 75 },
  CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C: { dex: "raydium_cpmm", size: (n) => n === 637, a: 168, b: 200 },
};
const TOKEN_2022 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function call(method, params) {
  for (let i = 0; i < 4; i++) {
    try {
      const r = await fetch(RPC, {
        method: "POST",
        headers: { "content-type": "application/json", "accept-encoding": "gzip" },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
      });
      const j = await r.json();
      if (j.error) {
        if (/skipped|not available|missing/i.test(j.error.message || "")) return null;
        throw new Error(j.error.message);
      }
      return j.result;
    } catch (e) {
      if (i === 3) throw e;
      await sleep(1000 * (i + 1));
    }
  }
}
const bs = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function b58(buf) {
  let n = BigInt("0x" + Buffer.from(buf).toString("hex"));
  let s = "";
  while (n > 0n) { s = bs[Number(n % 58n)] + s; n /= 58n; }
  for (const b of buf) { if (b === 0) s = "1" + s; else break; }
  return s;
}
async function accounts(keys, headersOnly) {
  const out = [];
  for (let i = 0; i < keys.length; i += 100) {
    const opts = { encoding: "base64", commitment: "confirmed" };
    if (headersOnly) opts.dataSlice = { offset: 0, length: 0 };
    const r = await call("getMultipleAccounts", [keys.slice(i, i + 100), opts]);
    out.push(...((r && r.value) || []));
  }
  return out;
}

// Everything already watched: the embedded registry and the census watchlist.
const known = new Set();
const knownMints = new Set();
for (const f of [path.join(__dirname, "..", "crates", "bot", "pools.json"), path.join(__dirname, "..", "watchlist.json")]) {
  try {
    const j = JSON.parse(fs.readFileSync(f, "utf8"));
    for (const p of j.pools || []) known.add(p.address);
    for (const m of Object.keys(j.mints || {})) knownMints.add(m);
  } catch (_) { /* absent is fine */ }
}
// Accounts already looked at and found not to be a pool on a traded venue.
const notPool = new Set();
// When each pool was last reported. A pool is reported again once this long has
// passed, because the bot lets go of its oldest discoveries when it is full, and a
// pool that is busy again should come back.
const REPORT_AGAIN_MS = 2 * 60 * 60 * 1000;
const reported = new Map();
const fresh = (k) => !known.has(k) || (reported.has(k) && Date.now() - reported.get(k) > REPORT_AGAIN_MS);

async function once() {
  const head = await call("getSlot", [{ commitment: "confirmed" }]);
  let block = null, slot = head;
  for (; slot > head - 4 && !block; slot--) {
    block = await call("getBlock", [slot, {
      encoding: "jsonParsed", transactionDetails: "full", rewards: false,
      maxSupportedTransactionVersion: 1, commitment: "confirmed",
    }]);
  }
  if (!block) return;
  // The writable, unsigned accounts of every arbitrage in the block.
  const arbs = [];
  for (const tx of block.transactions) {
    const a = classify(tx);
    if (!a) continue;
    const writes = keysOf(tx)
      .filter((k) => k.writable && !k.signer && !TIP_ACCOUNTS.has(k.key) && !NOT_STATE.has(k.key))
      .map((k) => k.key);
    arbs.push(writes);
  }
  const unseen = [...new Set(arbs.flat())].filter((k) => !notPool.has(k));
  if (unseen.length === 0) return;
  const heads = await accounts(unseen, true);
  const isPool = new Map();
  unseen.forEach((k, i) => {
    const h = heads[i];
    const v = h && VENUE[h.owner];
    if (v && v.size(h.space)) isPool.set(k, v);
    else notPool.add(k);
  });
  // Pools of arbitrages that ran through two or more pools on traded venues: a cycle
  // the bot could have taken. One such pool beside an unknown venue is not.
  const wanted = new Set();
  for (const writes of arbs) {
    const mine = writes.filter((k) => isPool.has(k) || known.has(k));
    if (mine.length >= 2) for (const k of mine) if (fresh(k)) wanted.add(k);
  }
  if (wanted.size === 0) return;
  const keys = [...wanted];
  const full = await accounts(keys, false);
  const pools = [];
  keys.forEach((k, i) => {
    const acc = full[i];
    const v = acc && VENUE[acc.owner];
    if (!v) return;
    const d = Buffer.from(acc.data[0], "base64");
    pools.push({ address: k, dex: v.dex, mint_a: b58(d.subarray(v.a, v.a + 32)), mint_b: b58(d.subarray(v.b, v.b + 32)) });
  });
  // Decimals and owning program of mints not already known.
  const newMints = [...new Set(pools.flatMap((p) => [p.mint_a, p.mint_b]))].filter((m) => !knownMints.has(m));
  const mints = {};
  for (let i = 0; i < newMints.length; i += 100) {
    const batch = newMints.slice(i, i + 100);
    const r = await call("getMultipleAccounts", [batch, { encoding: "jsonParsed", commitment: "confirmed" }]);
    ((r && r.value) || []).forEach((acc, j) => {
      const info = acc && acc.data && acc.data.parsed && acc.data.parsed.info;
      if (!info) return;
      const ext = (info.extensions || []).find((e) => e.extension === "tokenMetadata");
      const symbol = ((ext && ext.state.symbol) || "").trim().slice(0, 12) || batch[j].slice(0, 4);
      mints[batch[j]] = { symbol, decimals: info.decimals };
      if (acc.owner === TOKEN_2022) mints[batch[j]].token_program = "token-2022";
    });
  }
  // A pool whose mint could not be read is not one the bot can size.
  const out = pools
    .filter((p) => (knownMints.has(p.mint_a) || mints[p.mint_a]) && (knownMints.has(p.mint_b) || mints[p.mint_b]))
    .map((p) => ({
      ...p,
      label: `${(mints[p.mint_a] || {}).symbol || p.mint_a.slice(0, 4)}/${(mints[p.mint_b] || {}).symbol || p.mint_b.slice(0, 4)}`,
      fee_ppm: 0,
      tvl_usd: 0,
      seen: block.blockTime || 0,
    }));
  for (const p of out) {
    known.add(p.address);
    reported.set(p.address, Date.now());
  }
  for (const m of Object.keys(mints)) knownMints.add(m);
  if (out.length) process.stdout.write(JSON.stringify({ pools: out, mints }) + "\n");
}

// Run by the bot with stdin piped and never written: when the bot exits, however it
// exits, that pipe closes and so does this process. Windows does not end a child with
// its parent, and an orphaned sampler would keep reading blocks for nobody.
if (!process.stdin.isTTY && !ONCE) {
  process.stdin.on("end", () => process.exit(0));
  process.stdin.on("error", () => process.exit(0));
  process.stdin.resume();
}
process.stdout.on("error", () => process.exit(0));

(async () => {
  for (;;) {
    const started = Date.now();
    try {
      await once();
    } catch (e) {
      console.error(String(e && e.message || e).replace(/api-key=[^"&\s]*/g, "api-key=***"));
    }
    if (ONCE) break;
    await sleep(Math.max(1000, EVERY_MS - (Date.now() - started)));
  }
})();

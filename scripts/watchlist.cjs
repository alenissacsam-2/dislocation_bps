#!/usr/bin/env node
// Turn census rows into watchlist.json: the pools where next-block arbitrage was seen
// to land, on venues this bot can execute, for the bot to watch alongside its embedded
// registry (see crates/bot/src/registry.rs, Registry::load).
//
//   CRYPTOBOT_RPC_HTTP_URL=... node scripts/watchlist.cjs <census.json>... [--max 60]
//       [--venues orca_whirlpool,raydium_clmm,raydium_v4,meteora_dlmm]
//
// Only arbitrages whose state was at least a slot old count (age >= 1): a same-block
// backrun needs a validator's neighbour, and watching its pools buys nothing. Only
// arbitrages whose every pool is on an allowed venue count: a cycle the bot cannot
// finish is not one it can take. Pools are ranked by how many such arbitrages touched
// them. Rows from an older census without `writes` are resolved by fetching the
// transaction.

const fs = require("fs");
const path = require("path");

const RPC = process.env.CRYPTOBOT_RPC_HTTP_URL;
if (!RPC) {
  console.error("set CRYPTOBOT_RPC_HTTP_URL");
  process.exit(2);
}
const args = process.argv.slice(2);
const flag = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args.splice(i, 2)[1] : dflt;
};
const MAX = +flag("--max", 60);
const ALLOWED = new Set(flag("--venues", "orca_whirlpool,raydium_clmm,raydium_v4,meteora_dlmm").split(","));
const files = args;

// Pool accounts by owning program and exact size, with where their two mints sit.
// Sizes and offsets are the ones the bot's own decoders use (crates/dex/src).
const VENUE = {
  CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK: { dex: "raydium_clmm", size: (n) => n === 1544, a: 73, b: 105 },
  whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc: { dex: "orca_whirlpool", size: (n) => n === 653, a: 101, b: 181 },
  LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo: { dex: "meteora_dlmm", size: (n) => n === 904, a: 88, b: 120 },
  "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": { dex: "raydium_v4", size: (n) => n === 752, a: 400, b: 432 },
  cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG: { dex: "meteora_damm_v2", size: (n) => n === 1112, a: 168, b: 200 },
  pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA: { dex: "pumpswap", size: (n) => n >= 200 && n <= 400, a: 43, b: 75 },
};

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
      if (j.error) throw new Error(j.error.message);
      return j.result;
    } catch (e) {
      if (i === 4) throw e;
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
async function accounts(keys, slice) {
  const out = [];
  for (let i = 0; i < keys.length; i += 100) {
    const opts = { encoding: "base64" };
    if (slice) opts.dataSlice = { offset: 0, length: 0 };
    const r = await call("getMultipleAccounts", [keys.slice(i, i + 100), opts]);
    out.push(...r.value);
  }
  return out;
}

(async () => {
  const rows = files.flatMap((f) => JSON.parse(fs.readFileSync(f, "utf8")));
  const nextBlock = rows.filter((r) => String(r.age) !== "0");
  for (const r of nextBlock) {
    if (r.writes) continue;
    const t = await call("getTransaction", [r.sig, { encoding: "jsonParsed", maxSupportedTransactionVersion: 1 }]);
    r.writes = t ? t.transaction.message.accountKeys.filter((k) => k.writable && !k.signer).map((k) => k.pubkey) : [];
    await sleep(100);
  }

  // Which written accounts are pools, and on which venue.
  const all = [...new Set(nextBlock.flatMap((r) => r.writes))];
  const heads = await accounts(all, true);
  const candidates = all.filter((k, i) => {
    const h = heads[i];
    return h && VENUE[h.owner] && VENUE[h.owner].size(h.space);
  });
  const full = await accounts(candidates, false);
  const pools = new Map();
  candidates.forEach((k, i) => {
    const acc = full[i];
    if (!acc) return;
    const v = VENUE[acc.owner];
    const d = Buffer.from(acc.data[0], "base64");
    // An adaptive-fee whirlpool keeps its surcharge in an oracle account the bot does
    // not read yet, so the bot refuses to price it; choosing one would waste a slot.
    if (v.dex === "orca_whirlpool" && d.readUInt16LE(43) !== d.readUInt16LE(41)) return;
    pools.set(k, { dex: v.dex, mint_a: b58(d.subarray(v.a, v.a + 32)), mint_b: b58(d.subarray(v.b, v.b + 32)), n: 0 });
  });

  // Count only arbitrages the bot could have finished.
  let usable = 0;
  for (const r of nextBlock) {
    const mine = r.writes.filter((k) => pools.has(k));
    if (mine.length < 1 || !mine.every((k) => ALLOWED.has(pools.get(k).dex))) continue;
    // The census labels venues from program calls too; one it could not resolve to a
    // pool (a prop AMM, say) still makes the cycle unfinishable here.
    if (r.venues.split("+").some((v) => v.startsWith("?"))) continue;
    usable++;
    for (const k of mine) pools.get(k).n++;
  }
  const embedded = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "crates", "bot", "pools.json"), "utf8"));
  const known = new Set(embedded.pools.map((p) => p.address));
  const chosen = [...pools.entries()]
    .filter(([k, p]) => p.n > 0 && !known.has(k))
    .sort((x, y) => y[1].n - x[1].n)
    .slice(0, MAX);

  // Mints: decimals and program from the mint account, symbol from Token-2022 metadata
  // or the DAS index, and an address prefix when neither has one.
  const mintKeys = [...new Set(chosen.flatMap(([, p]) => [p.mint_a, p.mint_b]))].filter((m) => !embedded.mints[m]);
  const mints = {};
  for (let i = 0; i < mintKeys.length; i += 100) {
    const batch = mintKeys.slice(i, i + 100);
    const parsed = await call("getMultipleAccounts", [batch, { encoding: "jsonParsed" }]);
    let assets = [];
    try {
      assets = (await call("getAssetBatch", { ids: batch })) || [];
    } catch (_) { /* not every RPC serves DAS */ }
    batch.forEach((m, j) => {
      const acc = parsed.value[j];
      const info = acc && acc.data && acc.data.parsed && acc.data.parsed.info;
      if (!info) return;
      const ext = (info.extensions || []).find((e) => e.extension === "tokenMetadata");
      const das = assets.find((a) => a && a.id === m);
      const symbol = (ext && ext.state.symbol) || (das && das.content && das.content.metadata && das.content.metadata.symbol) || m.slice(0, 4);
      mints[m] = { symbol: symbol.trim().slice(0, 12) || m.slice(0, 4), decimals: info.decimals };
      if (acc.owner === "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb") mints[m].token_program = "token-2022";
    });
  }
  const sym = (m) => (mints[m] || embedded.mints[m] || { symbol: m.slice(0, 4) }).symbol;
  const out = {
    generated: new Date().toISOString(),
    from: files.map((f) => path.basename(f)),
    mints,
    pools: chosen.map(([k, p]) => ({
      address: k, dex: p.dex, label: `${sym(p.mint_a)}/${sym(p.mint_b)}`,
      mint_a: p.mint_a, mint_b: p.mint_b, fee_ppm: 0, tvl_usd: 0, next_block_arbs: p.n,
    })),
  };
  fs.writeFileSync("watchlist.json", JSON.stringify(out, null, 1));
  console.log(`next-block arbitrages ${nextBlock.length}, finishable on ${[...ALLOWED].join(", ")}: ${usable}`);
  console.log(`pools behind them ${pools.size}; new and chosen ${chosen.length}; new mints ${Object.keys(mints).length}`);
  for (const p of out.pools.slice(0, 20)) console.log(String(p.next_block_arbs).padStart(3), p.dex.padEnd(15), p.label.padEnd(18), p.address);
  console.log("wrote watchlist.json");
})();

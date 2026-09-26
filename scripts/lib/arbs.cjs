// Closed-loop arbitrage, recognised in a parsed transaction. Shared by
// scripts/census.cjs (which measures it) and scripts/discover.cjs (which follows it to
// the pools it ran through). Moved out of census.cjs unchanged on 2026-09-27.

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


module.exports = { WSOL, STABLES, VENUES, UTILITY, NOT_STATE, TIP_ACCOUNTS, keysOf, deltas, classify, calleesIn, poolsSwapped };

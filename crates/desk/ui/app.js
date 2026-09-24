/* cryptobot-desk — window logic.
 *
 * Three independent data paths, deliberately not unified:
 *   1. live telemetry  — WebSocket to the bot, works only while it runs
 *   2. history         — Tauri command reading SQLite, works with the bot stopped
 *   3. control         — Tauri command driving the child process
 * Keeping them separate is why a dead bot degrades path 1 alone: the window still
 * opens, history still renders, and the controls still work.
 *
 * On top of path 1 sits the analysis the Live view leads with: a funnel of where
 * opportunities die, the ranked reasons, per-venue price drift, and feed health. The
 * funnel counts from the event stream — complete, unlike the log, which rate-limits
 * repeated refusals — and is kept per run in localStorage so a window reload does not
 * zero it. Drift comes from the log, the only place the bot writes it.
 */

const invoke = window.__TAURI__.core.invoke;

// The endpoints used when nothing is configured. Kept in step with
// cb_desk::config::PUBLIC_HTTP / PUBLIC_WS.
const PUBLIC_HTTP = "https://api.mainnet-beta.solana.com";
const PUBLIC_WS = "wss://api.mainnet-beta.solana.com";

const S = {
  status: null,
  live: null,          // last status event from the bot
  history: null,
  view: "live",
  ws: null,
  wsLive: false,
  pools: new Map(),    // pool address -> { pair, dex, price }
  pairs: new Map(),    // pair -> Set(pool address)
  div: new Map(),      // pool address -> [bps deviation from consensus, ...]
  divPair: null,
  wall: [],            // [{ disl, fee }, ...]
  routes: [],
  tradeableMin: 0,
  viewingArchive: null,
  traces: { lag: [], age: [], rate: [], sweep: [] },
  lastUpdates: null,
  limits: null,
};

const MAX_POINTS = 240;
const $ = (id) => document.getElementById(id);
const fmt = (n, d = 2) => (Number.isFinite(n) ? n.toFixed(d) : "—");
const money = (n) => (Number.isFinite(n) ? (n < 0 ? "−$" : "$") + Math.abs(n).toFixed(Math.abs(n) < 1 ? 4 : 2) : "—");
const bps = (n) => (Number.isFinite(n) ? (n < 0 ? "−" : "") + Math.abs(n).toFixed(2) : "—");
const int = (n) => (Number.isFinite(n) ? Math.round(n).toLocaleString("en-US") : "—");
/** Dollars at a glance: $5, $5.8k, $127k, $1.3M. Digit grouping follows the locale otherwise, and en-IN writes $1,26,982. */
const usdCompact = (n) => {
  if (!Number.isFinite(n)) return "—";
  const a = Math.abs(n);
  if (a >= 1e6) return "$" + (n / 1e6).toFixed(a >= 1e7 ? 0 : 1) + "M";
  if (a >= 1e3) return "$" + (n / 1e3).toFixed(a >= 1e4 ? 0 : 1) + "k";
  return "$" + Math.round(n);
};
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));

function esc(s) {
  return String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return s + "s ago";
  if (s < 3600) return Math.round(s / 60) + "m ago";
  return (s / 3600).toFixed(1) + "h ago";
}

function dur(secs) {
  if (!Number.isFinite(secs)) return "—";
  if (secs < 60) return Math.round(secs) + "s";
  if (secs < 3600) return Math.floor(secs / 60) + "m";
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60);
  return h + "h " + String(m).padStart(2, "0") + "m";
}

const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

/* ── info reveals ─────────────────────────────────────────────────────── */
/* The long explanations live behind these. Hover shows one for as long as the pointer
 * rests; a click pins it until the next click or Escape. One shared tooltip element,
 * positioned fixed, so it is never clipped by the module it explains. */

const tipEl = $("tip");
let tipOwner = null, tipPinned = false;

function showTip(btn) {
  tipOwner = btn;
  tipEl.textContent = btn.dataset.tip;
  tipEl.hidden = false;
  const r = btn.getBoundingClientRect();
  const w = Math.min(360, window.innerWidth - 24);
  tipEl.style.maxWidth = w + "px";
  const tw = tipEl.offsetWidth, th = tipEl.offsetHeight;
  let left = clamp(r.left + r.width / 2 - tw / 2, 12, window.innerWidth - tw - 12);
  let top = r.bottom + 10;
  if (top + th > window.innerHeight - 12) top = r.top - th - 10;
  tipEl.style.left = left + "px";
  tipEl.style.top = top + "px";
  requestAnimationFrame(() => tipEl.classList.add("show"));
  btn.setAttribute("aria-expanded", "true");
}
function hideTip() {
  if (tipOwner) tipOwner.setAttribute("aria-expanded", "false");
  tipOwner = null; tipPinned = false;
  tipEl.classList.remove("show");
  tipEl.hidden = true;
}
document.addEventListener("click", (e) => {
  const btn = e.target.closest(".info");
  if (btn) {
    e.preventDefault(); e.stopPropagation();
    if (tipOwner === btn && tipPinned) { hideTip(); return; }
    showTip(btn); tipPinned = true;
    return;
  }
  if (tipPinned && !tipEl.contains(e.target)) hideTip();
});
document.addEventListener("mouseover", (e) => {
  const btn = e.target.closest(".info");
  if (btn && !tipPinned && tipOwner !== btn) showTip(btn);
});
document.addEventListener("mouseout", (e) => {
  const btn = e.target.closest(".info");
  if (btn && !tipPinned && !btn.contains(e.relatedTarget)) hideTip();
});
document.addEventListener("keydown", (e) => { if (e.key === "Escape") hideTip(); });
document.addEventListener("scroll", () => { if (!tipPinned) hideTip(); }, true);
for (const b of document.querySelectorAll(".info")) {
  b.setAttribute("aria-label", "About: " + (b.dataset.tip || "").slice(0, 60));
  b.setAttribute("aria-expanded", "false");
}

/* ── canvas ───────────────────────────────────────────────────────────── */

/* Sizes the backing store to the CSS box. The CSS height is load-bearing: without
 * one, clientHeight derives from the height attribute this function just wrote, and
 * every frame multiplies the element until the page is thousands of pixels tall. */
function prep(cv) {
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth, h = cv.clientHeight;
  if (!w || !h) return null;
  if (cv.width !== Math.round(w * dpr)) cv.width = Math.round(w * dpr);
  if (cv.height !== Math.round(h * dpr)) cv.height = Math.round(h * dpr);
  const g = cv.getContext("2d");
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, h);
  return { g, w, h };
}

const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const MONO = '11px "Cascadia Mono", "Cascadia Code", Consolas, monospace';

function grid(g, w, h, pad, lines) {
  g.strokeStyle = css("--rule");
  g.lineWidth = 1;
  for (const y of lines) { g.beginPath(); g.moveTo(pad.l, y); g.lineTo(w - pad.r, y); g.stroke(); }
}

function label(g, text, x, y, align = "right", color = null) {
  g.fillStyle = color || css("--ink-3");
  g.font = MONO;
  g.textAlign = align;
  g.textBaseline = "middle";
  g.fillText(text, x, y);
}

function empty(g, w, h, text) { label(g, text, w / 2, h / 2, "center"); }

/* ── live telemetry ───────────────────────────────────────────────────── */

function connect() {
  try {
    S.ws = new WebSocket("ws://127.0.0.1:8787/api/stream");
  } catch {
    setTimeout(connect, 3000);
    return;
  }
  S.ws.onopen = () => { S.wsLive = true; setStale(false); };
  S.ws.onmessage = (m) => {
    try { onEvent(JSON.parse(m.data)); } catch { /* a malformed frame is not fatal */ }
  };
  S.ws.onclose = () => { S.wsLive = false; markStale(); setTimeout(connect, 2500); };
  S.ws.onerror = () => { try { S.ws.close(); } catch {} };
}

/* A disconnected feed must never leave the last values on screen looking current.
 * Sinking them into their wells is the whole point: stale numbers that look live are
 * the failure this project keeps finding in itself. */
function setStale(on) {
  $("view-live").classList.toggle("stale", on);
}
function markStale() {
  setStale(true);
  $("routesEmpty").hidden = false;
  $("routesEmpty").textContent = "Not connected to a running bot. Start it from the rail.";
  $("routesBody").innerHTML = "";
  for (const id of ["sSlot", "sLag", "sAge", "sPools", "sStale", "sSweep", "sSol"]) $(id).textContent = "—";
  $("wFeedVal").textContent = "—";
  $("wFeedSub").textContent = "no connection to the bot";
  $("wFeedFoot").textContent = "";
  $("wFeedLed").className = "led sm";
  paintBotWell();
}

function onEvent(ev) {
  if (ev.type === "status") return onStatus(ev);
  if (ev.type === "routes") return onRoutes(ev);
  if (ev.type === "poolUpdate") return onPool(ev);
  if (ev.type === "opportunity") return onOpportunity(ev);
  if (ev.type === "execution") return onExecution(ev);
  if (ev.type === "logLine") return onLogLine(ev);
}

function onStatus(e) {
  S.live = e;
  ensureRun(e);
  const edge = e.tradeableEdgeBps;
  const el = $("mEdge");
  if (edge === null || edge === undefined) {
    el.innerHTML = "—";
    el.classList.add("dim");
    $("mEdgeSub").textContent = "nothing deep enough to trade";
  } else {
    el.classList.remove("dim");
    el.innerHTML = bps(edge) + '<span class="unit">bp</span>';
    $("mEdgeSub").textContent = (edge > 0 ? "clears · " : "needs +" + Math.abs(edge).toFixed(2) + " bp · ") + (e.tradeableRoute || "");
  }

  $("sSlot").textContent = String(e.slot ?? "—");
  $("sLag").textContent = String(e.slotLag ?? "—");
  $("sAge").textContent = (e.dataAgeSecs ?? 0) + "s";
  $("sPools").textContent = String(e.poolsTracked ?? "—");
  $("sStale").textContent = String(e.staleExcluded ?? 0);
  $("sSweep").textContent = Math.round((e.sweepUs ?? 0) / 1000) + "ms";
  $("sSol").textContent = e.solPriceUsd ? "$" + e.solPriceUsd.toFixed(2) : "—";
  $("sMode").textContent = (e.mode || "").toUpperCase() + (e.feedStalled ? " · FEED STALLED" : "");
  $("sMode").className = e.feedStalled ? "bad" : "";

  // Feed well and health traces.
  const now = Date.now();
  let rate = null;
  if (S.lastUpdates && e.updates >= S.lastUpdates.n) {
    const dt = (now - S.lastUpdates.t) / 1000;
    if (dt > 0.5) rate = (e.updates - S.lastUpdates.n) / dt;
  }
  S.lastUpdates = { n: e.updates, t: now };
  push(S.traces.lag, e.slotLag ?? 0);
  push(S.traces.age, e.dataAgeSecs ?? 0);
  if (rate !== null) push(S.traces.rate, rate);
  push(S.traces.sweep, (e.sweepUs ?? 0) / 1000);

  const bad = e.feedStalled || (e.dataAgeSecs ?? 0) > 10 || !e.connected;
  $("wFeedLed").className = "led sm " + (bad ? "fail" : "run");
  $("wFeedVal").innerHTML = (e.slotLag ?? 0) + '<span class="unit">slot lag</span>';
  $("wFeedVal").className = "r-val num" + (bad ? " bad" : "");
  $("wFeedSub").textContent = e.feedStalled
    ? "feed stalled — nothing is being recorded"
    : `${(e.dataAgeSecs ?? 0)}s data age · ${rate !== null ? Math.round(rate) + " updates/s" : "measuring…"}`;
  $("wFeedSub").className = "r-sub" + (e.feedStalled ? " bad" : "");
  $("wFeedFoot").textContent =
    `${e.poolsTracked ?? 0} pools · ${e.reconnects ?? 0} reconnects · drift ${e.reconcileDrift ?? 0}/${e.reconcileChecked ?? 0}`;

  paintBotWell();
  paintHealth();
}

function push(arr, v, max = 180) { arr.push(v); if (arr.length > max) arr.shift(); }

function onRoutes(e) {
  S.routes = e.rows || [];
  S.tradeableMin = e.tradeableMinUsd || 0;
  const top = S.routes[0];
  if (top) {
    S.wall.push({ disl: top.dislocationBps, fee: top.feeBps });
    if (S.wall.length > MAX_POINTS) S.wall.shift();
    const fill = clamp(top.dislocationBps / Math.max(top.feeBps, 0.01), 0, 1.4) / 1.4 * 100;
    $("gEdgeFill").style.width = fill + "%";
    $("gEdgeMark").style.left = (100 / 1.4) + "%";
  }
  const body = $("routesBody");
  body.innerHTML = "";
  for (const r of S.routes.slice(0, 10)) {
    const tr = document.createElement("tr");
    const tradeable = r.depthUsd >= S.tradeableMin;
    if (!tradeable) tr.className = "shallow";
    const scale = Math.max(r.feeBps, r.dislocationBps, 0.01) * 1.15;
    tr.innerHTML =
      `<td>${esc(r.route)}</td>` +
      `<td class="venues">${esc(r.venues)}</td>` +
      `<td class="n edge ${r.edgeBps > 0 ? "" : "neg"}">${bps(r.edgeBps)}</td>` +
      `<td><div class="mini"><i style="width:${(r.dislocationBps / scale) * 100}%"></i><s style="left:${(r.feeBps / scale) * 100}%"></s></div></td>` +
      `<td class="n">${fmt(r.dislocationBps)}</td>` +
      `<td class="n">${fmt(r.feeBps)}</td>` +
      `<td class="n">${usdCompact(r.depthUsd)}</td>`;
    body.appendChild(tr);
  }
  $("routesEmpty").hidden = S.routes.length > 0;
  if (!S.routes.length) $("routesEmpty").textContent = "No cycles priced in this sweep.";
  if (S.view === "live") drawWall();
}

function onPool(e) {
  S.pools.set(e.pool, { pair: e.pair, dex: e.dex, price: e.price });
  if (!S.pairs.has(e.pair)) S.pairs.set(e.pair, new Set());
  S.pairs.get(e.pair).add(e.pool);
  if (!S.divPair) pickPair();
}

/* The pair quoted by the most venues, because that is where a two-hop round trip
 * exists at all and therefore where divergence means something. Re-evaluated on every
 * sample; a switch needs strictly more venues than the pair already shown, so ties do
 * not flap and the chart stops clearing itself once the book is populated. */
function pickPair() {
  let best = S.divPair;
  let n = S.divPair ? (S.pairs.get(S.divPair)?.size ?? 0) : 0;
  for (const [pair, set] of S.pairs) if (set.size > n) { n = set.size; best = pair; }
  if (best && best !== S.divPair && n >= 2) { S.divPair = best; S.div.clear(); }
}

/* Sampling on a timer keeps every venue's series on one shared time axis. Plotting
 * per event would compare venues at different instants and manufacture divergence
 * that is really just staleness. */
function sampleDivergence() {
  pickPair();
  if (!S.divPair) return;
  const addrs = [...(S.pairs.get(S.divPair) || [])].filter((a) => S.pools.has(a));
  if (addrs.length < 2) return;
  const prices = addrs.map((a) => S.pools.get(a).price).filter((p) => p > 0);
  if (prices.length < 2) return;
  const mean = prices.reduce((a, b) => a + b, 0) / prices.length;
  for (const a of addrs) {
    const p = S.pools.get(a).price;
    if (!(p > 0)) continue;
    if (!S.div.has(a)) S.div.set(a, []);
    const arr = S.div.get(a);
    arr.push(((p - mean) / mean) * 10000);
    if (arr.length > MAX_POINTS) arr.shift();
  }
  $("divScope").textContent = S.divPair + " · " + addrs.length + " venues";
  if (S.view === "live") drawDivergence();
}

function seriesColours() {
  return [css("--amber"), css("--teal"), css("--violet"), css("--ink-2"),
          "#e39a5b", "#6fb3e0", "#d78bc4", "#b9c46a", "#8fa3b8", "#c7a27c"];
}

function drawDivergence() {
  const cv = $("divChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 44, r: 104, t: 10, b: 18 };
  const series = [...S.div.entries()].filter(([, a]) => a.length > 1);
  if (!series.length) return empty(g, w, h, "waiting for a pair quoted by more than one venue");

  /* Scaled to the 5th–95th percentile, not the extremes. One thin high-fee pool sitting
   * 100 bp off consensus used to set the axis on its own and press every venue that
   * matters into a single flat stripe. Anything beyond is pinned to the edge and drawn
   * dashed, so it still shows as "off the chart" rather than disappearing. */
  const all = [];
  for (const [, a] of series) for (const v of a) all.push(v);
  all.sort((a, b) => a - b);
  const q = (f) => all[Math.min(all.length - 1, Math.max(0, Math.round(f * (all.length - 1))))];
  let lo = Math.min(q(0.05), 0), hi = Math.max(q(0.95), 0);
  const span = Math.max(hi - lo, 0.5);
  lo -= span * 0.15; hi += span * 0.15;
  const X = (i, n) => pad.l + ((w - pad.l - pad.r) * i) / Math.max(n - 1, 1);
  const Y = (v) => h - pad.b - ((clamp(v, lo, hi) - lo) / (hi - lo)) * (h - pad.t - pad.b);
  const off = (v) => v < lo || v > hi;

  const ticks = [0, 1, 2, 3].map((k) => lo + ((hi - lo) * k) / 3);
  grid(g, w, h, pad, ticks.map(Y));
  g.strokeStyle = css("--ink-3"); g.setLineDash([3, 4]); g.lineWidth = 1;
  g.beginPath(); g.moveTo(pad.l, Y(0)); g.lineTo(w - pad.r, Y(0)); g.stroke();
  g.setLineDash([]);
  for (const v of ticks) label(g, v.toFixed(1), pad.l - 8, Y(v));

  const C = seriesColours();
  const tags = [];
  series.forEach(([addr, arr], i) => {
    const c = C[i % C.length];
    const last = arr[arr.length - 1];
    g.strokeStyle = c; g.lineWidth = 1.6; g.lineJoin = "round";
    g.setLineDash(off(last) ? [4, 4] : []); g.beginPath();
    arr.forEach((v, j) => (j ? g.lineTo(X(j, arr.length), Y(v)) : g.moveTo(X(j, arr.length), Y(v))));
    g.stroke(); g.setLineDash([]);
    const y = Y(last);
    g.fillStyle = c; g.beginPath(); g.arc(X(arr.length - 1, arr.length), y, 3, 0, 7); g.fill();
    tags.push({ y, want: y, text: (S.pools.get(addr)?.dex || "?").slice(0, 13) + (off(last) ? ` ${last > 0 ? "+" : ""}${last.toFixed(0)}` : ""), c });
  });

  /* End labels of venues quoting one pair land on top of each other; push them apart to
   * a minimum spacing, down then back up so the group stays inside the plot. */
  const GAP = 13, top = pad.t + 6, bot = h - pad.b - 6;
  tags.sort((a, b) => a.want - b.want);
  for (let i = 1; i < tags.length; i++) if (tags[i].y - tags[i - 1].y < GAP) tags[i].y = tags[i - 1].y + GAP;
  if (tags.length && tags[tags.length - 1].y > bot) {
    tags[tags.length - 1].y = bot;
    for (let i = tags.length - 2; i >= 0; i--) if (tags[i + 1].y - tags[i].y < GAP) tags[i].y = tags[i + 1].y - GAP;
  }
  for (const t of tags) {
    const y = clamp(t.y, top, bot);
    g.strokeStyle = t.c; g.globalAlpha = 0.45; g.lineWidth = 1;
    g.beginPath(); g.moveTo(w - pad.r + 2, t.want); g.lineTo(w - pad.r + 7, y); g.stroke();
    g.globalAlpha = 1;
    label(g, t.text, w - pad.r + 10, y, "left", t.c);
  }
}

function drawWall() {
  const cv = $("feeChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 40, r: 44, t: 10, b: 14 };
  if (S.wall.length < 2) return empty(g, w, h, "waiting for the first sweep");
  let hi = 0;
  for (const d of S.wall) hi = Math.max(hi, d.disl || 0, d.fee || 0);
  hi = Math.max(hi * 1.15, 1);
  const X = (i) => pad.l + ((w - pad.l - pad.r) * i) / Math.max(S.wall.length - 1, 1);
  const Y = (v) => h - pad.b - (v / hi) * (h - pad.t - pad.b);
  const ticks = [0, 1, 2].map((k) => (hi * k) / 2);
  grid(g, w, h, pad, ticks.map(Y));
  for (const v of ticks) label(g, v.toFixed(1), pad.l - 8, Y(v));

  // The space between the two lines is the distance to profitable; shade it.
  g.beginPath();
  S.wall.forEach((d, i) => (i ? g.lineTo(X(i), Y(d.fee || 0)) : g.moveTo(X(i), Y(d.fee || 0))));
  for (let i = S.wall.length - 1; i >= 0; i--) g.lineTo(X(i), Y(S.wall[i].disl || 0));
  g.closePath(); g.fillStyle = css("--amber-soft"); g.globalAlpha = 0.6; g.fill(); g.globalAlpha = 1;

  const line = (key, colour, dash, width) => {
    g.strokeStyle = colour; g.lineWidth = width; g.setLineDash(dash); g.beginPath();
    S.wall.forEach((d, i) => (i ? g.lineTo(X(i), Y(d[key] || 0)) : g.moveTo(X(i), Y(d[key] || 0))));
    g.stroke(); g.setLineDash([]);
  };
  line("fee", css("--ink-2"), [5, 4], 1.4);
  line("disl", css("--amber"), [], 1.8);
  const last = S.wall[S.wall.length - 1];
  label(g, "gap", w - pad.r + 6, Y(last.disl || 0), "left", css("--amber"));
  label(g, "fees", w - pad.r + 6, Y(last.fee || 0) + (Math.abs(Y(last.fee) - Y(last.disl)) < 12 ? 12 : 0), "left", css("--ink-2"));
}

/* ── the funnel: where opportunities die ──────────────────────────────── */

const STAGES = [
  { key: "seen", name: "Surfaced" },
  { key: "positive", name: "Net positive after tip" },
  { key: "attempted", name: "Past the filters" },
  { key: "built", name: "Re-priced & built" },
  { key: "simulated", name: "Simulated clean" },
  { key: "sent", name: "Sent to Jito" },
  { key: "landed", name: "Landed" },
];

/* Every outcome string the bot can publish, mapped to the stage it stopped at and a
 * cause an operator recognises. Order matters: the first match wins. `fault` marks the
 * outcomes that mean something is wrong rather than the market saying no. */
const RULES = [
  [/^net negative after tip/, "positive", "Negative after the tip"],
  [/refused recently/, "attempted", "Cooling down after a refusal"],
  [/stalest leg/, "attempted", "A leg's price is stale"],
  [/slots apart/, "attempted", "Legs priced in different slots"],
  [/attempts\s+went to other cycles/, "attempted", "Sweep's attempt budget used"],
  [/has no encoder/, "attempted", "Venue can't be traded yet"],
  [/wallet cannot start/, "attempted", "Entry token not held"],
  [/round trip costs/, "attempted", "Fees above the ceiling"],
  [/^not attempted/, "attempted", "Filtered before trying"],
  [/^probe/, "probe", "Measured by probe, not traded"],
  [/trading is halted/, "built", "Risk gate halted trading", true],
  [/dry run/, "sent", "Dry run — built, not sent"],
  [/spends \d+ and guarantees only/, "built", "Edge gone when re-priced"],
  [/landing it costs|loses its own fee/, "built", "Edge can't cover fee + tip"],
  [/widest floor this edge allows/, "built", "Edge can't cover the fee"],
  [/could not be re-priced|can carry any size/, "built", "Pool moved before re-pricing"],
  [/starts by spending .* holds/, "built", "Entry token not held"],
  [/no token account/, "built", "No token account (rent)"],
  [/would leave less than/, "built", "Balance too low for fees"],
  [/expected net .* below/, "built", "Below minimum profit"],
  [/last Jito send/, "built", "Jito's one-a-second limit"],
  [/Jito trade can only/, "built", "Not a SOL cycle (Jito)"],
  [/will not fit|not yet re-priced|no encoder/, "built", "Route shape unsupported"],
  [/vanished|learned where pool/, "built", "Pool data refreshed"],
  [/^refused/, "built", "Refused: other"],
  [/^simulation.*(6018|TooLittle|floor was missed|missed its floor)/i, "simulated", "Price moved before simulating"],
  [/^simulation.*simulated balance .* below/, "simulated", "Simulated below the profit floor"],
  [/^simulation/, "simulated", "Simulation rejected the build", true],
  [/submitted and landed/, "landed", "Landed"],
  [/landed, reverted/, "landed", "Landed and reverted", true],
  [/not included/, "landed", "Not included — cost nothing"],
  [/not yet confirmed/, "landed", "Sent; confirmation pending"],
  [/^rpc error|^unexpected/, "built", "Error while attempting", true],
];

function classify(reason) {
  const r = reason || "";
  for (const [re, stage, name, fault] of RULES) if (re.test(r)) return { stage, name, fault: !!fault };
  return { stage: "built", name: "Unrecognised outcome", fault: false };
}

const RUN = { key: null, store: null };

function blankStore(start) {
  return { start, counts: Object.fromEntries(STAGES.map((s) => [s.key, 0])), drops: {}, causes: {},
           realised: 0, tips: 0, lastSend: null, paper: false, probesWon: 0 };
}

/* The bot's own start time, derived from its uptime, identifies the run. A window
 * opened mid-run starts counting then and says so; a reload within the same run picks
 * up the stored counts. */
function ensureRun(status) {
  if (!Number.isFinite(status.uptimeSecs)) return;
  const start = Math.round((Date.now() - status.uptimeSecs * 1000) / 60000) * 60000;
  if (RUN.key && Math.abs(RUN.store.start - start) < 120000) return;
  const key = "funnel:" + start;
  let store = null;
  try { store = JSON.parse(localStorage.getItem(key) || "null"); } catch { store = null; }
  if (!store) {
    store = blankStore(start);
    store.since = Date.now();
    // Keep only the last few runs.
    try {
      const keys = Object.keys(localStorage).filter((k) => k.startsWith("funnel:")).sort();
      while (keys.length > 5) localStorage.removeItem(keys.shift());
    } catch {}
  }
  RUN.key = key; RUN.store = store;
  paintFunnel(); paintReasons(); paintMoneyWell(); paintBotWell();
}

let saveTimer = null;
function saveRun() {
  if (!RUN.key) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => { try { localStorage.setItem(RUN.key, JSON.stringify(RUN.store)); } catch {} }, 800);
}

function drop(stage, name, fault) {
  const st = RUN.store;
  st.drops[stage] = st.drops[stage] || {};
  st.drops[stage][name] = (st.drops[stage][name] || 0) + 1;
  const c = st.causes[name] || (st.causes[name] = { n: 0, stage, fault });
  c.n += 1;
}

function onOpportunity(ev) {
  if (!RUN.store) return;
  const st = RUN.store;
  st.counts.seen += 1;
  if (ev.skippedReason) {
    const c = classify(ev.skippedReason);
    drop("positive", c.name, c.fault);
  } else {
    st.counts.positive += 1;
  }
  saveRun(); paintFunnel(); paintReasons();
}

function onExecution(ev) {
  if (!ev.paper) { recordAttempt(classifyAttempt(ev.reason || ""), ev.tsMs || Date.now(), ev.tipPaidUsd || 0); paintAttempts(); }
  if (!RUN.store) return;
  const st = RUN.store;
  if (ev.paper) {
    st.paper = true;
    st.counts.attempted += 1;
    st.realised += ev.realisedUsd || 0;
    st.tips += ev.tipPaidUsd || 0;
    saveRun(); paintFunnel(); paintReasons(); paintMoneyWell();
    return;
  }
  const c = classify(ev.reason || (ev.landed ? "submitted and landed" : ""));
  const order = STAGES.map((s) => s.key);
  const at = order.indexOf(c.stage);
  if (c.stage === "probe") {
    if (/would have profited/.test(ev.reason || "")) st.probesWon += 1;
    drop("attempted", c.name, false);
  } else if (c.stage === "landed" || c.stage === "sent") {
    for (const k of ["attempted", "built", "simulated"]) st.counts[k] += 1;
    if (c.stage === "sent") {
      drop("sent", c.name, c.fault);
    } else {
      st.counts.sent += 1;
      st.lastSend = { ts: ev.tsMs || Date.now(), sig: ev.signature, outcome: c.name, fault: c.fault };
      if (c.name === "Landed") st.counts.landed += 1; else drop("landed", c.name, c.fault);
    }
  } else if (at >= 0) {
    // Everything before the stage it stopped at was passed.
    for (let i = 2; i < at; i++) st.counts[order[i]] += 1;
    drop(c.stage, c.name, c.fault);
  }
  st.realised += ev.realisedUsd || 0;
  st.tips += ev.tipPaidUsd || 0;
  saveRun(); paintFunnel(); paintReasons(); paintMoneyWell(); paintBotWell();
}

function topDrop(stage) {
  const d = RUN.store?.drops[stage];
  if (!d) return null;
  let best = null;
  for (const [name, n] of Object.entries(d)) if (!best || n > best.n) best = { name, n };
  return best;
}

function paintFunnel() {
  const el = $("funnel");
  const st = RUN.store;
  if (!st) {
    el.innerHTML = '<div class="funnel-foot">Waiting for the bot. The funnel fills from its live event stream once it is running.</div>';
    $("funnelScope").textContent = "";
    return;
  }
  const since = st.since && st.since - st.start > 90000 ? "since you opened this window at " + clock(st.since) : "this run, since " + clock(st.start);
  $("funnelScope").textContent = since;
  const stages = st.paper
    ? [STAGES[0], STAGES[1], { key: "attempted", name: "Taken on paper" }]
    : STAGES;
  const max = Math.max(st.counts.seen, 1);
  el.style.gridTemplateColumns = `repeat(${stages.length}, minmax(0, 1fr))`;
  el.innerHTML = stages.map((s, i) => {
    const n = st.counts[s.key] || 0;
    const h = n ? 14 + Math.sqrt(n / max) * 86 : 0;
    const d = topDrop(s.key);
    const lost = i > 0 ? (st.counts[stages[i - 1].key] || 0) - n : 0;
    const fault = d && st.causes[d.name]?.fault;
    const dropText = i === 0 ? "cycles priced as opportunities"
      : d ? `<b>${int(d.n)}</b> ${esc(d.name.toLowerCase())}`
      : lost > 0 ? `<b>${int(lost)}</b> stopped here` : "none lost here";
    return `<div class="chamber${n ? "" : " zero"}${s.key === "landed" ? " landed" : ""}">
      <div class="well"><div class="fillbar" style="height:${h}%"></div>
        <div class="count">${int(n)}</div><div class="name">${esc(s.name)}</div></div>
      <div class="drop${fault ? " bad" : ""}">${dropText}</div></div>`;
  }).join("") + `<div class="funnel-foot">${funnelVerdict(st)}</div>`;
}

function funnelVerdict(st) {
  const c = st.counts;
  if (!c.seen) {
    const e = S.live?.tradeableEdgeBps;
    return Number.isFinite(e)
      ? `Nothing has cleared its fees yet. The closest tradeable cycle is ${Math.abs(e).toFixed(2)} bp short.`
      : "Nothing has surfaced yet.";
  }
  if (st.paper) return `Demo mode: ${int(c.attempted)} of ${int(c.seen)} would have been taken on paper.`;
  if (c.landed) return `${int(c.landed)} landed of ${int(c.sent)} sent.`;
  if (c.sent) return `${int(c.sent)} sent to Jito; none included yet. A bundle that misses costs nothing.`;
  const worst = STAGES.slice(1).map((s, i) => ({ s, lost: (c[STAGES[i].key] || 0) - (c[s.key] || 0) }))
    .sort((a, b) => b.lost - a.lost)[0];
  return worst && worst.lost > 0
    ? `Most are lost at “${worst.s.name.toLowerCase()}”: ${int(worst.lost)} of ${int(c.seen)}.`
    : "";
}

function paintReasons() {
  const el = $("reasons");
  const st = RUN.store;
  const causes = st ? Object.entries(st.causes).sort((a, b) => b[1].n - a[1].n) : [];
  const landed = st?.counts.landed || 0;
  if (!causes.length && !landed) {
    el.innerHTML = `<div class="empty">No outcomes yet this run. Every refusal, rejection and send will be ranked here as it happens.</div>`;
    return;
  }
  const total = causes.reduce((a, [, c]) => a + c.n, 0) + landed;
  const top = Math.max(landed, ...causes.map(([, c]) => c.n), 1);
  const rows = [];
  if (landed) rows.push(reasonRow("Landed", landed, total, top, "win"));
  for (const [name, c] of causes.slice(0, 9)) rows.push(reasonRow(name, c.n, total, top, c.fault ? "fault" : ""));
  if (causes.length > 9) {
    const rest = causes.slice(9).reduce((a, [, c]) => a + c.n, 0);
    rows.push(reasonRow(`${causes.length - 9} other causes`, rest, total, top, ""));
  }
  if (st.probesWon) rows.push(`<div class="hint">${st.probesWon} filtered cycle${st.probesWon > 1 ? "s" : ""} would have profited when probed.</div>`);
  el.innerHTML = rows.join("");
}

function reasonRow(name, n, total, top, cls) {
  return `<div class="reason ${cls}"><span class="rname" title="${esc(name)}">${esc(name)}</span>
    <span class="rcount">${int(n)} · ${Math.round((n / Math.max(total, 1)) * 100)}%</span>
    <div class="rtrack"><div class="rfill" style="width:${(n / top) * 100}%"></div></div></div>`;
}

/* ── drift and faults, from the log ───────────────────────────────────── */

const LOGA = { drift: new Map(), runStartIdx: 0, gaps: 0, faults: [] };

function parseLogLine(line) {
  const m = line.match(/^(\d{4}-\d\d-\d\dT[\d:.]+)Z?\s+(INFO|WARN|ERROR|DEBUG|TRACE)\s+(\S+?):\s(.*)$/);
  if (!m) return { ts: null, lv: "", target: "", msg: line };
  return { ts: m[1], lv: m[2], target: m[3], msg: m[4] };
}

function isRunStart(msg) { return /^live mode: waiting for the wallet passphrase|^mode: PAPER|^universe: /.test(msg); }

function ingestDrift(msg) {
  const m = msg.match(/^leg drift since detection: (.*)$/);
  if (!m) return;
  for (const part of m[1].split(" · ")) {
    const pm = part.match(/^(.*) ([+-]?\d+(?:\.\d+)?) bps$/);
    if (!pm) continue;
    const v = parseFloat(pm[2]);
    if (!Number.isFinite(v)) continue;
    const arr = LOGA.drift.get(pm[1]) || [];
    arr.push(v);
    if (arr.length > 2000) arr.shift();
    LOGA.drift.set(pm[1], arr);
  }
}

function ingestLog(lines, reset) {
  if (reset) {
    // Only the current run: everything after the last start marker.
    let start = 0;
    for (let i = lines.length - 1; i >= 0; i--) {
      const { msg } = parseLogLine(lines[i]);
      if (/^live mode: waiting for the wallet passphrase|^mode: PAPER/.test(msg)) { start = i; break; }
    }
    LOGA.drift = new Map(); LOGA.gaps = 0;
    ATT.pts = []; ATT.rate = 0; ATT.accounts = 0; ATT.tipsUsd = 0;
    lines = lines.slice(start);
  }
  for (const line of lines) {
    const { msg } = parseLogLine(line);
    if (/^feed silent for over/.test(msg)) LOGA.gaps += 1;
    ingestDrift(msg);
    ingestAttempt(msg, parseLogLine(line).ts);
  }
  paintDrift(); paintHealth(); paintAttempts();
}

/* ── attempts ─────────────────────────────────────────────────────────────
 * Every cycle the bot actually tried — fetched fresh state, re-priced, built a floor —
 * and where the fresh price landed against that floor. The funnel says how many; this
 * says by how much, which is the number that decides whether anything will ever clear.
 * Backfilled from the log for the current run at start, then fed by execution events. */
const ATT = { pts: [], rate: 0, accounts: 0, tipsUsd: 0 };
const ATT_KINDS = {
  loss: { name: "loss on fresh price", tone: "--coral" },
  fee: { name: "cleared price, not fee", tone: "--amber" },
  sim: { name: "failed simulation", tone: "--amber" },
  packet: { name: "cleared, too big for one packet", tone: "--green" },
  miss: { name: "sent, not included", tone: "--violet" },
  revert: { name: "landed, reverted", tone: "--coral" },
  landed: { name: "landed", tone: "--teal" },
};

function classifyAttempt(msg) {
  let m = msg.match(/spends (\d+) and guarantees only (\d+) back/);
  if (m) return { kind: "loss", bps: ((+m[2] - +m[1]) / +m[1]) * 1e4 };
  if (/guarantees \d+ more than it spends, and landing it costs \d+/.test(msg)) return { kind: "fee", bps: null };
  if (/not included/.test(msg)) return { kind: "miss", bps: null };
  if (/landed and reverted|landed, reverted/.test(msg)) return { kind: "revert", bps: null };
  if (/^LANDED |submitted and landed/.test(msg)) return { kind: "landed", bps: null };
  if (/^(refused: |probe not run: )?simulation/.test(msg)) return { kind: "sim", bps: null };
  // Priced clear on fresh state, then too many accounts for one packet: the case an
  // address lookup table would turn into a trade.
  if (/byte limit|address lookup table/.test(msg) && !/^\d+ hops will not fit/.test(msg.replace(/^refused: /, ""))) {
    return { kind: "packet", bps: null };
  }
  if (/the last Jito send was/.test(msg)) return { rate: true };
  // Both the startup open and the rotation confirm with this one line; the rotation's
  // own summary line follows it and would count the same account twice.
  if (/^opened \d+ token account\(s\), confirmed/.test(msg)) return { account: true };
  return null;
}

function recordAttempt(c, t, tipUsd) {
  if (!c) return;
  if (c.rate) { ATT.rate += 1; return; }
  if (c.account) { ATT.accounts += 1; return; }
  ATT.pts.push({ t, kind: c.kind, bps: c.bps });
  if (c.kind === "landed") ATT.tipsUsd += tipUsd || 0;
  if (ATT.pts.length > 5000) ATT.pts.shift();
}

function ingestAttempt(msg, ts) {
  const t = ts ? Date.parse(ts + "Z") : Date.now();
  recordAttempt(classifyAttempt(msg), t, 0);
}

function paintAttempts() {
  if (S.view !== "live") return;
  drawAttempts();
  const n = {};
  for (const p of ATT.pts) n[p.kind] = (n[p.kind] || 0) + 1;
  const sent = (n.miss || 0) + (n.landed || 0) + (n.revert || 0);
  $("attLegend").innerHTML = Object.entries(ATT_KINDS)
    .filter(([k]) => n[k])
    .map(([k, v]) => `<span class="lg"><i style="background:var(${v.tone})"></i>${esc(v.name)} <b>${int(n[k])}</b></span>`)
    .join("") || `<span class="lg muted">No attempt yet this run.</span>`;
  const losses = ATT.pts.filter((p) => p.kind === "loss" && Number.isFinite(p.bps)).map((p) => p.bps);
  const kv = [
    ["Real attempts", int(ATT.pts.length)],
    ["Median miss", losses.length ? fmt(median(losses)) + " bp" : "—"],
    ["Closest miss", losses.length ? fmt(Math.max(...losses)) + " bp" : "—"],
    ["Sent to Jito", int(sent)],
    ["Landed", int(n.landed || 0), n.landed ? "ok" : ""],
    ["Not included, free", int(n.miss || 0)],
    ["Tips paid", ATT.tipsUsd ? "$" + ATT.tipsUsd.toFixed(4) : "$0"],
    ["Turned away by the 1/s limit", int(ATT.rate)],
    ["Token accounts opened", int(ATT.accounts)],
  ];
  $("jitoKv").innerHTML = kv
    .map(([k, v, cls]) => `<div class="kv-row"><span>${esc(k)}</span><b class="num ${cls || ""}">${esc(v)}</b></div>`)
    .join("");
}

function drawAttempts() {
  const cv = $("attChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 44, r: 16, t: 12, b: 20 };
  if (!ATT.pts.length) return empty(g, w, h, "no attempt has reached a fresh re-price yet this run");

  const t0 = Math.min(...ATT.pts.map((q) => q.t));
  const t1 = Math.max(Date.now(), t0 + 60_000);
  const ys = ATT.pts.filter((q) => Number.isFinite(q.bps)).map((q) => q.bps).sort((a, b) => a - b);
  const lo = Math.min(-2, ys.length ? ys[Math.floor(ys.length * 0.05)] * 1.15 : -10);
  const hi = 3;
  const X = (t) => pad.l + ((w - pad.l - pad.r) * (t - t0)) / (t1 - t0);
  const Y = (v) => h - pad.b - ((clamp(v, lo, hi) - lo) / (hi - lo)) * (h - pad.t - pad.b);
  // Labels closer than a line apart overprint; the floor always keeps its own.
  const ticks = [0, lo, lo / 2, hi].filter((v, i, all) => all.slice(0, i).every((u) => Math.abs(Y(u) - Y(v)) >= 14));
  grid(g, w, h, pad, ticks.map(Y));
  for (const v of ticks) label(g, (v > 0 ? "+" : "") + v.toFixed(v === 0 ? 0 : 1), pad.l - 8, Y(v));

  // The floor: fee and tip guaranteed on chain. Above it a trade can be sent.
  g.strokeStyle = css("--teal"); g.globalAlpha = 0.55; g.setLineDash([4, 4]); g.lineWidth = 1;
  g.beginPath(); g.moveTo(pad.l, Y(0)); g.lineTo(w - pad.r, Y(0)); g.stroke();
  g.setLineDash([]); g.globalAlpha = 1;
  label(g, "floor", w - pad.r, Y(0) - 8, "right", css("--teal"));

  // Where a point has no measured distance, it sits just under the floor (refused for
  // something other than price) or above it (sent), so its outcome still reads.
  const yOf = (q) => Number.isFinite(q.bps) ? q.bps
    : q.kind === "miss" || q.kind === "landed" || q.kind === "revert" ? hi * 0.55
    : q.kind === "packet" ? hi * 0.3 : -0.4;
  for (const q of ATT.pts) {
    const big = q.kind === "landed" || q.kind === "miss" || q.kind === "revert" || q.kind === "packet";
    g.fillStyle = css(ATT_KINDS[q.kind].tone);
    g.globalAlpha = big ? 1 : 0.7;
    g.beginPath(); g.arc(X(q.t), Y(yOf(q)), big ? 4.5 : 2.6, 0, 7); g.fill();
  }
  g.globalAlpha = 1;
  const mins = Math.round((t1 - t0) / 60_000);
  label(g, mins >= 120 ? Math.round(mins / 60) + " h ago" : mins + " min ago", pad.l, h - 6, "left");
  label(g, "now", w - pad.r, h - 6, "right");
}

function median(a) {
  if (!a.length) return NaN;
  const s = [...a].sort((x, y) => x - y);
  return s[Math.floor(s.length / 2)];
}

function paintDrift() {
  const el = $("drift");
  const rows = [...LOGA.drift.entries()].filter(([, a]) => a.length).sort((a, b) => b[1].length - a[1].length);
  if (!rows.length) {
    el.innerHTML = `<div class="empty">No re-prices yet this run. Each time a cycle is re-priced, how far each venue's price moved since detection lands here.</div>`;
    return;
  }
  const lim = Math.max(3, Math.min(20, Math.ceil(Math.max(...rows.flatMap(([, a]) => a.map((v) => Math.abs(v)))))));
  el.innerHTML = rows.slice(0, 6).map(([venue, a], i) => {
    const med = median(a);
    const better = a.filter((v) => v > 0).length;
    return `<div class="drift-row"><div class="venue">${esc(venue)}<small>${int(a.length)} re-prices · ${Math.round((better / a.length) * 100)}% better</small></div>
      <canvas data-drift="${i}"></canvas>
      <div class="med">${bps(med)}<small>median bp</small></div></div>`;
  }).join("") + `<div class="drift-axis"><span></span><span><i>−${lim}</i><i>0</i><i>+${lim}</i></span><span></span></div>`;
  rows.slice(0, 6).forEach(([, a], i) => {
    const cv = el.querySelector(`canvas[data-drift="${i}"]`);
    const p = prep(cv); if (!p) return;
    const { g, w, h } = p;
    const X = (v) => ((clamp(v, -lim, lim) + lim) / (2 * lim)) * (w - 8) + 4;
    g.strokeStyle = css("--ink-3"); g.setLineDash([2, 3]); g.lineWidth = 1;
    g.beginPath(); g.moveTo(X(0), 2); g.lineTo(X(0), h - 2); g.stroke(); g.setLineDash([]);
    const recent = a.slice(-400);
    g.fillStyle = css("--violet");
    recent.forEach((v, j) => {
      g.globalAlpha = 0.18 + 0.5 * (j / recent.length);
      const jitter = ((j * 7919) % 17) / 17;
      g.beginPath(); g.arc(X(v), 5 + jitter * (h - 10), 1.8, 0, 7); g.fill();
    });
    g.globalAlpha = 1;
    const m = median(a);
    g.fillStyle = css("--ink");
    g.fillRect(X(m) - 1, 2, 2.5, h - 4);
  });
}

/* ── health traces ────────────────────────────────────────────────────── */

const TRACES = [
  { key: "lag", name: "Slot lag", unit: "", bad: (v) => v > 3, digits: 0 },
  { key: "age", name: "Data age", unit: "s", bad: (v) => v > 5, digits: 0 },
  { key: "rate", name: "Pool updates", unit: "/s", bad: (v) => v < 1, digits: 0 },
  { key: "sweep", name: "Sweep time", unit: "ms", bad: (v) => v > 400, digits: 0 },
];

function paintHealth() {
  const box = $("traces");
  if (!box.children.length) {
    box.innerHTML = TRACES.map((t) => `<div class="well trace" data-t="${t.key}"><div class="t-head">
      <span class="t-name">${t.name}</span><span class="t-val">—</span></div><canvas></canvas></div>`).join("");
  }
  for (const t of TRACES) {
    const cell = box.querySelector(`[data-t="${t.key}"]`);
    const arr = S.traces[t.key];
    const v = arr[arr.length - 1];
    const val = cell.querySelector(".t-val");
    val.textContent = Number.isFinite(v) ? v.toFixed(t.digits) + t.unit : "—";
    val.className = "t-val" + (Number.isFinite(v) && t.bad(v) ? " bad" : "");
    const p = prep(cell.querySelector("canvas")); if (!p) continue;
    const { g, w, h } = p;
    if (arr.length < 2) { empty(g, w, h, "sampling"); continue; }
    const hi = Math.max(...arr, 1) * 1.2;
    const X = (i) => (w * i) / (arr.length - 1);
    const Y = (x) => h - 3 - (x / hi) * (h - 6);
    g.beginPath(); arr.forEach((x, i) => (i ? g.lineTo(X(i), Y(x)) : g.moveTo(X(i), Y(x))));
    g.lineTo(w, h); g.lineTo(0, h); g.closePath();
    g.fillStyle = css("--violet-soft"); g.fill();
    g.strokeStyle = css("--violet"); g.lineWidth = 1.5; g.beginPath();
    arr.forEach((x, i) => (i ? g.lineTo(X(i), Y(x)) : g.moveTo(X(i), Y(x)))); g.stroke();
  }
  const e = S.live || {};
  const c = (name, v, bad, muted) => `<span class="${bad ? "bad" : muted ? "muted" : ""}">${name}<b>${v}</b></span>`;
  $("counters").innerHTML = [
    c("dropped", int(e.dropped ?? NaN), (e.dropped || 0) > 0),
    c("reconnects", int(e.reconnects ?? NaN), (e.reconnects || 0) > 0),
    c("stalls", int(e.stalls ?? NaN), (e.stalls || 0) > 0),
    c("subscribe errors", int(e.subscribeErrors ?? NaN), (e.subscribeErrors || 0) > 0),
    c("feed gaps this run", int(LOGA.gaps), LOGA.gaps > 3),
    c("reconcile drift", `${e.reconcileDrift ?? "—"}/${e.reconcileChecked ?? "—"}`, false),
    c("stale pools excluded", int(e.staleExcluded ?? NaN), (e.staleExcluded || 0) > 0),
    c("RPC credits", "not visible here", false, true),
  ].join("");
}

/* ── readout wells ────────────────────────────────────────────────────── */

function paintBotWell() {
  const s = S.status;
  const led = $("wBotLed");
  if (!s) return;
  const running = s.state === "running" || s.state === "starting";
  led.className = "led sm " + ({ running: "run", starting: "run", failed: "fail", foreign: "warn" }[s.state] || "");
  const up = S.live && S.wsLive ? " · " + dur(S.live.uptimeSecs) : "";
  $("wBotState").textContent = (STATE_LABEL[s.state] || s.state) + (running ? up : "");
  $("wBotState").className = "r-val" + (s.state === "failed" ? " bad" : "");
  const m = window.__mode, d = window.__dry;
  const live = m && m.effective === "live";
  $("wBotSub").textContent = !running ? (STATE_NOTE[s.state] || "")
    : live ? (d && !d.effective ? "LIVE · submitting through Jito" : "LIVE · dry run, nothing sent") : "DEMO · signs nothing";
  const last = RUN.store?.lastSend;
  $("wBotLast").textContent = last ? `last sent ${ago(last.ts)} · ${last.outcome.toLowerCase()}` : "no trade sent this run";
  $("wBotLast").className = "r-foot" + (last?.fault ? " bad" : "");
}

function paintMoneyWell() {
  const st = RUN.store;
  const realised = st ? st.realised : 0;
  const budget = S.limits?.maxDailyLossUsd ?? null;
  $("wMoneyVal").textContent = money(realised);
  const used = budget ? clamp(-Math.min(realised, 0) / budget, 0, 1) : 0;
  $("gBudgetFill").style.width = used * 100 + "%";
  $("gBudgetFill").classList.toggle("bad", used >= 0.8);
  const bal = window.__holdings;
  const solUsd = S.live?.solPriceUsd;
  const balText = bal ? `${parseFloat(bal.sol).toFixed(4)} SOL` + (solUsd ? ` ≈ $${(parseFloat(bal.sol) * solUsd).toFixed(2)}` : "") : "balance not read";
  $("wMoneySub").textContent = budget
    ? `${Math.round(used * 100)}% of the $${budget.toFixed(2)} loss budget · ${balText}`
    : balText;
}

/* ── history ──────────────────────────────────────────────────────────── */

let historyInFlight = false;

async function loadHistory(path) {
  if (historyInFlight) return;
  historyInFlight = true;
  if (!S.history) {
    // A long live ledger takes seconds to aggregate. Say so in every panel rather than
    // leaving five empty frames that read as broken.
    $("pnlNote").textContent = "reading the ledger…";
    const wait = `<div class="loading"><i></i><i></i><i></i><span>Reading the ledger — a long run takes a few seconds.</span></div>`;
    for (const id of ["ladder", "contest", "race"]) $(id).innerHTML = wait;
  }
  try {
    S.history = path ? await invoke("read_history_at", { path }) : await invoke("read_history");
    S.viewingArchive = path || null;
  } catch (e) {
    S.history = { available: false, reason: String(e) };
  } finally {
    historyInFlight = false;
  }
  paintHistory();
}

function paintHistory() {
  const H = S.history;
  const cells = ["hHours", "hOpps", "hTaken", "hNet", "hMedian"];
  const banner = $("archiveBanner");
  banner.hidden = !S.viewingArchive;
  if (S.viewingArchive) {
    banner.innerHTML = `Showing an archived run, read-only: <code>${esc(S.viewingArchive.split(/[\\/]/).pop())}</code>
      <button class="key" id="btnBackLive" type="button">Back to the live ledger</button>`;
    $("btnBackLive").onclick = () => { S.viewingArchive = null; loadHistory(null); };
  }
  if (!H || !H.available) {
    for (const c of cells) { $(c).textContent = "—"; $(c).classList.add("dim"); }
    $("hWindow").textContent = H ? (H.reason || "") : "";
    $("pnlNote").textContent = "no ledger yet — start the bot to begin one";
    return;
  }
  for (const c of cells) $(c).classList.remove("dim");
  const L = H.ladder || {};
  const eps = H.episodes || [];
  const curve = H.curve || [];
  const last = curve[curve.length - 1];

  $("hHours").innerHTML = fmt(H.hoursObserved, 1) + '<span class="unit">h</span>';
  $("hWindow").textContent = (H.firstAt || "").slice(5, 16) + " → " + (H.lastAt || "").slice(5, 16);
  $("hOpps").textContent = last ? int(last.episodes) : "—";
  $("hTaken").textContent = last ? int(last.taken) : "—";
  $("hTakenSub").textContent = last && last.episodes ? ((last.taken / last.episodes) * 100).toFixed(1) + "% of what was seen" : "";
  $("hNet").textContent = money(L.realisedUsd ?? 0);
  const pies = eps.map((e) => e.pieUsd).filter((v) => v > 0).sort((a, b) => a - b);
  $("hMedian").textContent = pies.length ? money(pies[Math.floor(pies.length / 2)]) : "—";

  const perHour = H.hoursObserved > 0 ? (L.realisedUsd ?? 0) / H.hoursObserved : 0;
  $("pnlNote").textContent = `${money(perHour)}/h · ${money(perHour * 24)}/day at this rate`;

  paintLadder(L);
  paintRace(H.race);
  paintContest(H);
  if (S.view === "history") { drawPnl(curve); drawScatter(eps); }
}

function barRow(name, value, top, cls) {
  const pct = clamp((value / Math.max(top, 1e-9)) * 100, 0, 100);
  return `<div class="bar-row"><div class="bar-lab">${name}</div>
    <div class="bar-track"><div class="bar-fill ${cls || ""}" style="width:${pct}%"></div></div>
    <div class="bar-val">${money(value)}</div></div>`;
}

function paintLadder(L) {
  const el = $("ladder");
  const rungs = L.rungs || [];
  if (!rungs.length) { el.innerHTML = '<div class="empty">No ladder measured yet.</div>'; return; }
  const ceiling = Math.max(L.atOptimalUsd || 0, ...rungs.map((r) => r[1]), 1e-9);
  el.innerHTML = rungs.map(([book, paid]) => barRow("$" + book.toLocaleString("en-US"), paid, ceiling)).join("")
    + barRow("unlimited", L.atOptimalUsd || 0, ceiling, "faint")
    + barRow("actual", L.realisedUsd || 0, ceiling, "teal");
}

function paintRace(R) {
  const el = $("race");
  if (!R || !R.rungs || !R.rungs.length || !R.declinedEpisodes) {
    el.innerHTML = '<div class="empty">Nothing refused as contested yet — this run has no race to price.</div>';
    return;
  }
  const top = Math.max(...R.rungs.map((r) => r[1]), 1e-9);
  el.innerHTML = R.rungs.map(([p, got]) => barRow(p === 0 ? "now" : (p * 100).toFixed(0) + "% win", got, top, p === 0 ? "faint" : "")).join("")
    + `<div class="note-row">${R.declinedEpisodes} episodes worth ${money(R.declinedNetUsd)} net refused for being contested`
    + (R.declinedUnprofitableEpisodes ? `; a further ${R.declinedUnprofitableEpisodes} were already negative and are rightly refused` : "")
    + `.</div>`;
}

function paintContest(H) {
  const el = $("contest");
  const c = H.contest || {};
  if (!H.contestHasEvidence) {
    el.innerHTML = `<div class="empty">Not enough evidence yet — ${c.contestedEpisodes ?? 0} declined and
      ${c.uncontestedEpisodes ?? 0} not. The comparison needs at least 20 of each before it says anything,
      and a rate computed from fewer would look like a finding.</div>`;
    return;
  }
  const a = (H.contestSurvivalRate ?? 0) * 100;
  const b = (H.uncontestedSurvivalRate ?? 0) * 100;
  el.innerHTML = `<table class="kv"><tbody>
      <tr><td>Declined as contested</td><td class="n">${int(c.contestedEpisodes ?? 0)}</td></tr>
      <tr><td>…still there a slot later</td><td class="n">${a.toFixed(1)}%</td></tr>
      <tr><td>Everything else</td><td class="n">${int(c.uncontestedEpisodes ?? 0)}</td></tr>
      <tr><td>…still there a slot later</td><td class="n">${b.toFixed(1)}%</td></tr>
      <tr><td>Value declined</td><td class="n">${money(c.declinedUsd ?? 0)}</td></tr>
    </tbody></table>
    <p class="prose">${a < b
      ? `Declined opportunities vanish about ${(b / Math.max(a, 0.01)).toFixed(1)}× faster, which is what losing a race looks like. It is not proof: large gaps also close fast for purely mechanical reasons, and this test cannot separate the two.`
      : `Declined opportunities survive at least as often as the rest — so the classifier is not detecting a race, and is more likely keying on size.`}</p>`;
}

function drawPnl(curve) {
  const cv = $("pnlChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 58, r: 14, t: 12, b: 16 };
  if (curve.length < 2) return empty(g, w, h, "no history yet");
  const vals = curve.map((c) => c.realisedUsd);
  const lo = Math.min(0, ...vals), hi = Math.max(...vals, 1e-6);
  const span = (hi - lo) * 1.12 || 1e-6;
  const X = (i) => pad.l + ((w - pad.l - pad.r) * i) / (curve.length - 1);
  const Y = (v) => h - pad.b - ((v - lo) / span) * (h - pad.t - pad.b);
  const ticks = [0, 1, 2, 3].map((k) => lo + (span * k) / 3);
  grid(g, w, h, pad, ticks.map(Y));
  for (const v of ticks) label(g, money(v), pad.l - 8, Y(v));

  g.beginPath();
  vals.forEach((v, i) => (i ? g.lineTo(X(i), Y(v)) : g.moveTo(X(i), Y(v))));
  g.lineTo(X(vals.length - 1), Y(lo)); g.lineTo(X(0), Y(lo)); g.closePath();
  const grad = g.createLinearGradient(0, pad.t, 0, h - pad.b);
  grad.addColorStop(0, css("--teal-soft")); grad.addColorStop(1, "transparent");
  g.fillStyle = grad; g.fill();
  g.strokeStyle = css("--teal"); g.lineWidth = 1.8; g.beginPath();
  vals.forEach((v, i) => (i ? g.lineTo(X(i), Y(v)) : g.moveTo(X(i), Y(v)))); g.stroke();
  g.fillStyle = css("--teal");
  g.beginPath(); g.arc(X(vals.length - 1), Y(vals[vals.length - 1]), 3.5, 0, 7); g.fill();
}

function drawScatter(eps) {
  const cv = $("scatter"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 58, r: 14, t: 12, b: 28 };
  const pts = eps.filter((e) => e.pieUsd > 0);
  if (!pts.length) return empty(g, w, h, "no episodes yet");
  // Lifetime is in whole slots and frequently zero, so it is shifted by one before the
  // log. Dropping the zeros instead would delete the population that matters most.
  const lx = (e) => Math.log10(e.lifetimeSlots + 1);
  const ly = (e) => Math.log10(e.pieUsd);
  const xs = pts.map(lx), ys = pts.map(ly);
  const x1 = Math.max(...xs, 0.4), y0 = Math.min(...ys), y1 = Math.max(...ys);
  const X = (v) => pad.l + (v / Math.max(x1, 1e-9)) * (w - pad.l - pad.r);
  const Y = (v) => h - pad.b - ((v - y0) / Math.max(y1 - y0, 1e-9)) * (h - pad.t - pad.b);
  const yt = [];
  for (let d = Math.ceil(y0); d <= Math.floor(y1); d++) yt.push(d);
  grid(g, w, h, pad, yt.map(Y));
  for (const d of yt) label(g, "$" + Math.pow(10, d).toPrecision(1), pad.l - 8, Y(d));
  for (let s = 0; s <= Math.floor(x1) + 1; s++) {
    const x = X(s);
    if (x > w - pad.r - 16) break;
    label(g, String(Math.pow(10, s) - (s === 0 ? 1 : 0)), x, h - pad.b + 11, "center");
  }
  label(g, "slots survived →", pad.l, h - 5, "left");
  for (const e of pts) {
    g.fillStyle = e.taken ? css("--teal") : e.contested ? css("--amber") : css("--ink-3");
    g.globalAlpha = e.taken ? 0.9 : 0.4;
    g.beginPath(); g.arc(X(lx(e)), Y(ly(e)), e.taken ? 2.8 : 1.8, 0, 7); g.fill();
  }
  g.globalAlpha = 1;
}

/* ── control ──────────────────────────────────────────────────────────── */

const STATE_LABEL = {
  running: "Running", starting: "Starting…", stopped: "Stopped",
  foreign: "Port held elsewhere", failed: "Died unexpectedly",
};
const STATE_NOTE = {
  running: "Recording to the ledger.",
  starting: "Waiting for it to bind the port.",
  stopped: "Nothing running. History is still readable.",
  foreign: "Another process holds port 8787. Not started by this app, so it will not be stopped by it.",
  failed: "The process exited on its own. The log says why.",
};

async function refreshStatus() {
  try {
    S.status = await invoke("bot_status");
  } catch {
    return; // a failed probe is not a state change; keep the last known one
  }
  const s = S.status;
  $("dot").className = "led " + ({ running: "run", starting: "run", foreign: "foreign", failed: "fail" }[s.state] || "");
  $("stateLabel").textContent = STATE_LABEL[s.state] || s.state;
  $("stateNote").textContent = !s.botExePresent ? "No cb-bot.exe found. Build it before starting." : (STATE_NOTE[s.state] || "");
  $("btnStart").disabled = s.state !== "stopped" || !s.botExePresent;
  // A foreign process was not started here and must never be killed from here.
  $("btnStop").disabled = !(s.state === "running" || s.state === "starting");
  // Archiving stops the bot first; stopping a process we did not start is not ours to do.
  $("btnArchive").disabled = !s.ledgerPresent || s.state === "foreign";
  $("railPath").textContent = s.root || "";
  $("railPath").title = s.root || "";
  $("sLedger").textContent = s.ledgerPresent ? "cryptobot.db" : "none";
  paintBotWell();
}

async function showFailure(msg) {
  let lines = [];
  try { lines = await invoke("read_log", { lines: 40 }); } catch { /* log may not exist */ }
  LOGV.banner = msg;
  LOGV.lines = lines;
  setView("log");
}

/* ── views ────────────────────────────────────────────────────────────── */

function setView(v) {
  S.view = v;
  hideTip();
  for (const b of document.querySelectorAll("nav button")) {
    const on = b.dataset.view === v;
    b.classList.toggle("on", on);
    if (on) b.setAttribute("aria-current", "page"); else b.removeAttribute("aria-current");
  }
  for (const sec of document.querySelectorAll(".view")) sec.hidden = sec.id !== "view-" + v;
  if (v === "history") loadHistory(S.viewingArchive);
  if (v === "runs") loadRuns();
  if (v === "log") loadLog();
  if (v === "config") loadConfig();
  requestAnimationFrame(drawAll);
}

function drawAll() {
  if (S.view === "live") { drawDivergence(); drawWall(); paintDrift(); paintHealth(); paintAttempts(); }
  if (S.view === "history" && S.history && S.history.available) { drawPnl(S.history.curve || []); drawScatter(S.history.episodes || []); }
}

/* ── config ───────────────────────────────────────────────────────────── */

async function loadConfig() {
  try {
    const p = await invoke("read_config");
    $("fCapital").value = p.capitalUsd;
    $("fBuffer").value = p.feeBufferUsd;
    $("fMinTrade").value = p.minTradeUsd;
    $("fHops").value = p.maxHops;
    $("fSlippage").value = p.slippageTenthBps;
    $("fPriority").value = p.priorityMicroLamports;
    // Blank when it is the public default, so the placeholder shows through and the
    // field reads as "not set" rather than as a choice someone made.
    $("fRpcHttp").value = p.rpcHttpUrl === PUBLIC_HTTP ? "" : p.rpcHttpUrl;
    $("fRpcWs").value = p.rpcWsUrl === PUBLIC_WS ? "" : p.rpcWsUrl;
  } catch (e) {
    $("saveResult").textContent = "Could not read config.toml: " + e;
  }
  try { $("fAutostart").checked = await invoke("get_autostart"); } catch { /* not registrable */ }
  $("fAutorestart").checked = await invoke("get_auto_restart");
  try { $("fRoot").textContent = await invoke("get_root"); } catch { $("fRoot").textContent = "unknown"; }
  await paintWallet();
  await paintLimits();
  await paintMode();
  await paintDryRun();
}

// ── submission ──────────────────────────────────────────────────────────────
//
// Deliberately not a field in the parameters form. That form writes six values at
// once, and a form which can arm real spending as a side effect of changing the
// capital is a form that will eventually do exactly that.

async function paintDryRun() {
  let d;
  try { d = await invoke("read_dry_run"); }
  catch (e) { $("dryState").textContent = "Could not read dry_run: " + e; return; }
  window.__dry = d;
  const lines = [];
  if (d.envOverride) {
    lines.push(`<b>CRYPTOBOT_DRY_RUN=${esc(d.envOverride)} is set and overrides the file.</b> `
      + `The file says <code>${d.dryRun}</code>; the bot will use <code>${d.effective}</code>.`);
  }
  lines.push(d.effective
    ? "<b>Dry run.</b> Transactions are built, signed and simulated against live state, and none are submitted."
    : "<b>Real transactions will be submitted.</b> Each goes to Jito as a bundle of one: a missed floor is dropped and costs nothing, and every floor guarantees the fee and tip on chain — but money can move from here.");
  $("dryState").innerHTML = lines.join("<br><br>");
  $("btnAllowSpend").disabled = !d.effective;
  $("btnDryRun").disabled = d.effective;
  $("spendConfirmWrap").hidden = !d.effective;
  // The rail reads the same flag; painted before it arrived, it says dry run is on.
  paintRail();
  paintBotWell();
}

$("btnAllowSpend").onclick = async () => {
  $("dryResult").textContent = "Applying…";
  try {
    await invoke("set_dry_run", { dryRun: false, confirm: $("fSpendConfirm").value });
    $("fSpendConfirm").value = "";
    $("dryResult").textContent = "Real transactions are now allowed.";
  } catch (e) {
    $("dryResult").textContent = "" + e;
  }
  await paintDryRun();
};

// Never asks. Making the safe direction cheap is the whole point of asking on the other one.
$("btnDryRun").onclick = async () => {
  $("dryResult").textContent = "Applying…";
  try {
    await invoke("set_dry_run", { dryRun: true, confirm: "" });
    $("dryResult").textContent = "Back to dry run. Nothing will be submitted.";
  } catch (e) {
    $("dryResult").textContent = "" + e;
  }
  await paintDryRun();
};

// ── mode ────────────────────────────────────────────────────────────────────
//
// Three things that can genuinely disagree are shown: what the file says, what the bot
// will actually run as once the environment is applied, and whether this build can
// execute at all.

async function paintMode() {
  let m;
  try { m = await invoke("read_mode"); }
  catch (e) { $("modeState").textContent = "Could not read mode: " + e; return; }
  window.__mode = m;
  $("modeDemo").checked = m.effective === "paper";
  $("modeLive").checked = m.effective === "live";
  const lines = [];
  if (m.envOverride) {
    lines.push(`<b>CRYPTOBOT_MODE=${esc(m.envOverride)} is set in the environment and overrides the file.</b> `
      + `The file says <code>${esc(m.mode)}</code>; the bot will run as <code>${esc(m.effective)}</code>. `
      + `This application cannot change that — unset the variable and restart it.`);
  }
  if (!m.executionImplemented) {
    lines.push("<b>Live execution is not built in this binary.</b> Demo is the whole of what works here.");
  } else if (m.effective === "live") {
    lines.push("<b>Live is armed.</b> Whether anything is actually submitted depends on Submission above: "
      + "while dry run is on, the bot builds, signs and simulates real transactions and sends none.");
  }
  lines.push(m.allowLiveSet
    ? "<code>CRYPTOBOT_ALLOW_LIVE=1</code> is set — the outside half of the guard is open."
    : "<code>CRYPTOBOT_ALLOW_LIVE</code> is not set, so Live cannot be armed and the bot would refuse to "
      + "start against a live config. That half of the guard lives outside this application, which "
      + "deliberately does not set it for you. In PowerShell: <code>setx CRYPTOBOT_ALLOW_LIVE 1</code>, "
      + "then close and reopen this application — a process inherits its environment at start.");
  $("modeState").innerHTML = lines.join("<br><br>");
  paintRail();
  paintBotWell();
}

function syncModeConfirm() {
  $("modeConfirmWrap").hidden = !$("modeLive").checked;
  paintLiveAccount();
}
$("modeDemo").onchange = syncModeConfirm;
$("modeLive").onchange = syncModeConfirm;

$("btnSetMode").onclick = async () => {
  const mode = $("modeLive").checked ? "live" : "paper";
  const confirm = $("fModeConfirm").value;
  $("modeResult").textContent = "Applying…";
  try {
    const r = await invoke("set_mode", { mode, confirm });
    const bits = [`Mode is now ${r.mode}.`];
    if (r.archived) bits.push(`Previous run archived as ${r.archived}.`);
    if (r.restartError) bits.push(`The bot did not restart: ${r.restartError}`);
    else if (r.restarted) bits.push("The bot restarted.");
    $("modeResult").textContent = bits.join(" ");
  } catch (e) {
    $("modeResult").textContent = "" + e;
  } finally {
    $("fModeConfirm").value = "";
    await paintMode();
    syncModeConfirm();
  }
};

// ── risk limits ─────────────────────────────────────────────────────────────
//
// Saved separately from the trading parameters and without archiving the run: limits
// bound what may be signed, they do not change how anything is measured.

const LIMIT_FIELDS = {
  fMaxPos: "maxPositionUsd",
  fMaxLoss: "maxDailyLossUsd",
  fMinNet: "minNetProfitUsd",
  fMaxFails: "maxConsecutiveFailures",
  fHaltCooldown: "haltCooldownSecs",
};

async function paintLimits() {
  try {
    const l = await invoke("read_limits");
    for (const [id, key] of Object.entries(LIMIT_FIELDS)) $(id).value = l[key];
    window.__limits = l;
    S.limits = l;
    paintMoneyWell();
  } catch (e) {
    $("limitsResult").textContent = "Could not read limits: " + e;
  }
}

$("btnSaveLimits").onclick = async () => {
  // Carry through the fields the form does not show, so saving does not silently reset
  // them to defaults the operator never chose.
  const limits = Object.assign({}, window.__limits || {});
  for (const [id, key] of Object.entries(LIMIT_FIELDS)) limits[key] = Number($(id).value);
  try {
    await invoke("save_limits", { limits });
    $("limitsResult").textContent = "Limits saved. They apply to the next trade considered.";
    await paintLimits();
  } catch (e) {
    $("limitsResult").textContent = "" + e;
  }
};

// ── wallet ──────────────────────────────────────────────────────────────────
//
// The key is in the clear in exactly one place — the value of #fSecret — and for as
// long as it takes to hand it to the backend. The field is cleared on success and on
// failure alike: a rejected paste left sitting in a form is still a secret in a form.

function clearSecretFields() {
  for (const id of ["fSecret", "fPass", "fUnlockPass"]) {
    const el = $(id);
    if (el) el.value = "";
  }
}

async function paintWallet() {
  let s;
  try { s = await invoke("wallet_status"); }
  catch { $("walletState").textContent = "Could not read wallet state."; return; }
  window.__walletUnlocked = !!s.unlocked;
  const setup = $("walletSetup"), unlock = $("walletUnlock");
  if (!s.configured) {
    $("walletState").textContent = "No key configured. The bot cannot trade without one.";
    setup.hidden = false; unlock.hidden = true;
    $("accountPanel").hidden = true;
    window.__address = null;
    window.__holdings = null;
    paintLiveAccount();
    paintRailWallet();
    return;
  }
  setup.hidden = true; unlock.hidden = false;
  $("walletState").innerHTML = s.unlocked
    ? "Unlocked for this session."
    : "Key present but locked. Unlock it before this address can sign anything.";
  // Seeing what an address holds never requires the ability to spend from it.
  $("accountPanel").hidden = false;
  $("accAddress").textContent = s.pubkey || "—";
  window.__address = s.pubkey || null;
  if (window.__address && !window.__balancesFetched) {
    window.__balancesFetched = true;
    paintBalances(true);
  }
  paintRailWallet();
}

// Balances are fetched on demand rather than on a timer: the endpoint is shared with
// the bot that is actually working, and a balance does not change on its own.
async function paintBalances(quiet) {
  const holdings = $("accBalances"), verdict = $("accReadiness");
  if (!window.__address) { verdict.textContent = ""; return; }
  if (!quiet) { holdings.textContent = "reading…"; $("railBal").classList.add("reading"); }
  let h;
  try { h = await invoke("wallet_balances"); }
  catch (e) {
    holdings.textContent = "could not read balances";
    verdict.className = "acct-verdict";
    verdict.textContent = "" + e;
    return;
  }
  window.__holdings = h;
  const rows = [`<div><span class="amt">${esc(h.sol)}</span> <span class="sym">SOL</span></div>`];
  for (const t of h.tokens) {
    // An unnamed mint is shown by its address rather than a guessed ticker.
    const lab = t.symbol ? `<span class="sym">${esc(t.symbol)}</span>` : `<span class="mint">${esc(t.mint)}</span>`;
    rows.push(`<div><span class="amt">${esc(t.amount)}</span> ${lab}</div>`);
  }
  holdings.innerHTML = rows.join("");
  verdict.className = "acct-verdict " + (h.readiness.canTrade ? "ok" : "no");
  verdict.innerHTML = (h.readiness.canTrade ? "<b>Can trade.</b> " : "<b>Cannot trade.</b> ") + esc(h.readiness.reason);
  $("accSource").textContent = "from " + h.rpc;
  paintLiveAccount();
  paintRailWallet();
  paintMoneyWell();
}

$("btnRefreshBalances").onclick = () => paintBalances(false);

$("btnCopyAddr").onclick = async () => {
  if (!window.__address) return;
  try {
    await navigator.clipboard.writeText(window.__address);
    $("accSource").textContent = "address copied";
  } catch {
    $("accSource").textContent = "could not reach the clipboard";
  }
};

// What Live would actually sign with, shown the moment Live is selected: the address
// and what it holds are the two facts that decision needs.
function paintLiveAccount() {
  const box = $("liveAccount");
  if (!$("modeLive").checked) { box.hidden = true; return; }
  box.hidden = false;
  const addr = window.__address;
  if (!addr) {
    box.innerHTML = "<b>No key is configured.</b> Live has nothing to sign with. Import one under Wallet below.";
    return;
  }
  const h = window.__holdings;
  const bits = [`<b>Live would sign with</b> <code>${esc(addr)}</code>`];
  if (!h) {
    bits.push("Balances not read yet — press <b>Refresh balances</b> under Wallet before arming it.");
  } else {
    const held = [`${h.sol} SOL`].concat(h.tokens.filter((t) => parseFloat(t.amount) > 0)
      .map((t) => `${t.amount} ${t.symbol || t.mint.slice(0, 6) + "…"}`));
    bits.push("Holding " + esc(held.join(" · ")));
    if (!h.readiness.canTrade) bits.push("<b>This address cannot trade.</b> " + esc(h.readiness.reason));
  }
  if (!window.__walletUnlocked) {
    bits.push("<b>The key is locked.</b> Unlock it under Wallet — a locked key cannot sign.");
  }
  box.innerHTML = bits.join("<br>");
}

$("btnImport").onclick = async () => {
  const secret = $("fSecret").value, passphrase = $("fPass").value;
  $("walletResult").textContent = "Encrypting…";
  try {
    await invoke("wallet_import", { secret, passphrase });
    $("walletResult").textContent = "Key encrypted and saved. Unlock it to use it.";
  } catch (e) {
    $("walletResult").textContent = "" + e;
  } finally {
    clearSecretFields();
    await paintWallet();
  }
};

$("btnUnlock").onclick = async () => {
  $("walletResult").textContent = "Unlocking…";
  try {
    await invoke("wallet_unlock", { passphrase: $("fUnlockPass").value });
    $("walletResult").textContent = "Unlocked for this session.";
  } catch (e) {
    $("walletResult").textContent = "" + e;
  } finally {
    clearSecretFields();
    await paintWallet();
  }
};

$("btnForget").onclick = async () => {
  if (!confirm("Delete the encrypted key from this machine?\n\nThis cannot be undone here — you would need the original key to import it again.")) return;
  try {
    await invoke("wallet_forget");
    $("walletResult").textContent = "Key removed.";
  } catch (e) {
    $("walletResult").textContent = "" + e;
  }
  await paintWallet();
};

$("fAutostart").onchange = async (e) => {
  try { await invoke("set_autostart", { on: e.target.checked }); }
  catch (err) { $("saveResult").textContent = "Could not change autostart: " + err; e.target.checked = !e.target.checked; }
};
$("fAutorestart").onchange = (e) => invoke("set_auto_restart", { on: e.target.checked });

// Provider shortcuts insert a URL *shape*, not a working endpoint — the key has to come
// from that provider's dashboard. Appended rather than replacing: the point is several.
for (const b of document.querySelectorAll("#rpcProviders button")) {
  b.onclick = () => {
    const box = $("fRpcHttp");
    const lines = box.value.split("\n").map((l) => l.trim()).filter(Boolean);
    const url = b.dataset.rpc;
    if (!lines.includes(url)) lines.push(url);
    box.value = lines.join("\n") + "\n";
    box.focus();
    const at = box.value.indexOf("YOUR_KEY");
    if (at >= 0) box.setSelectionRange(at, at + "YOUR_KEY".length);
  };
}

$("btnSave").onclick = async () => {
  const params = {
    capitalUsd: parseFloat($("fCapital").value),
    feeBufferUsd: parseFloat($("fBuffer").value),
    minTradeUsd: parseFloat($("fMinTrade").value),
    maxHops: parseInt($("fHops").value, 10),
    slippageTenthBps: parseInt($("fSlippage").value, 10),
    priorityMicroLamports: parseInt($("fPriority").value, 10),
    rpcHttpUrl: $("fRpcHttp").value.trim(),
    rpcWsUrl: $("fRpcWs").value.trim(),
  };
  $("saveResult").textContent = "Saving…";
  try {
    const r = await invoke("save_config", { params, restart: true });
    const bits = ["Saved."];
    bits.push(r.archived ? `Archived ${r.archived}.` : "Nothing to archive.");
    if (r.wasRunning) bits.push(r.restarted ? "Restarted." : "Restart FAILED: " + (r.restartError || "?"));
    else bits.push("The bot was not running, so nothing was restarted.");
    $("saveResult").textContent = bits.join(" ");
    refreshStatus();
  } catch (e) {
    $("saveResult").textContent = "Refused: " + e;
  }
};

/* ── runs ─────────────────────────────────────────────────────────────── */

function runWhen(name) {
  const m = name.match(/(\d{4})(\d\d)(\d\d)-(\d\d)(\d\d)(\d\d)/);
  if (!m) return name;
  const d = new Date(`${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:${m[6]}`);
  return "Filed " + d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

async function loadRuns() {
  let runs = [];
  try { runs = await invoke("read_archives"); } catch (e) {
    $("runsNote").textContent = "could not list archives: " + e;
    return;
  }
  const body = $("runsBody");
  body.innerHTML = "";
  $("runsEmpty").hidden = runs.length > 0;
  for (const r of runs) {
    const row = document.createElement("div");
    row.className = "well run" + (S.viewingArchive === r.path ? " viewing" : "");
    row.innerHTML = `<div><div class="when">${esc(runWhen(r.name))}</div><div class="file">${esc(r.name)}</div></div>
      <div class="size">${(r.bytes / 1048576).toFixed(1)} MB</div>`;
    const btn = document.createElement("button");
    btn.className = "key";
    btn.type = "button";
    btn.textContent = S.viewingArchive === r.path ? "Showing in History" : "Open in History";
    btn.onclick = () => { loadHistory(r.path); setView("history"); };
    row.appendChild(btn);
    body.appendChild(row);
  }
  $("runsNote").textContent = S.viewingArchive ? "History is showing an archived run" : `${runs.length} archived`;
}

/* ── log ──────────────────────────────────────────────────────────────── */

const LOGV = { lines: [], filter: "all", query: "", paused: false, banner: null };

function lineKind(p) {
  const m = p.msg;
  if (/^(submitted |LANDED|sent to Jito)|was not included|landed and reverted/.test(m)) return m.includes("reverted") ? "fault" : "trade";
  if (p.lv === "ERROR" && !/^(LIVE ARMED|mode: LIVE|a filter held back)/.test(m)) return "fault";
  if (/panicked|halted:|Jito refused|rpc error/.test(m)) return "fault";
  if (/^(refused|rejected|simulation|probe|not attempted)/.test(m)) return "refused";
  if (/^leg drift/.test(m)) return "drift";
  if (p.lv === "WARN") return "warn";
  return "info";
}

function passes(kind, p) {
  const f = LOGV.filter;
  if (f === "trade" && kind !== "trade") return false;
  if (f === "refused" && kind !== "refused") return false;
  if (f === "warn" && p.lv !== "WARN") return false;
  if (f === "error" && kind !== "fault") return false;
  if (LOGV.query && !p.msg.toLowerCase().includes(LOGV.query)) return false;
  return true;
}

function renderLog() {
  const body = $("logBody");
  const q = LOGV.query;
  const out = [];
  if (LOGV.banner) out.push(`<div class="ln k-fault"><span></span><span></span><span class="msg raw">${esc(LOGV.banner)}</span></div>`);
  let prev = null, rep = 0;
  const flush = () => {
    if (!prev) return;
    const { p, kind } = prev;
    let msg = esc(p.msg);
    if (q) msg = msg.replace(new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi"), (x) => `<mark>${x}</mark>`);
    out.push(`<div class="ln k-${kind}"><span class="ts">${esc(p.ts ? p.ts.slice(11, 19) : "")}</span>` +
      `<span class="lv ${p.lv}">${esc(p.lv)}</span><span class="msg">${msg}${rep > 1 ? `<span class="rep">×${rep}</span>` : ""}</span></div>`);
  };
  for (const line of LOGV.lines) {
    const p = parseLogLine(line);
    const kind = lineKind(p);
    if (!passes(kind, p)) continue;
    // A refusal can repeat every sweep; collapse exact repeats into a counter.
    if (prev && prev.p.msg === p.msg) { rep += 1; continue; }
    flush();
    prev = { p, kind }; rep = 1;
  }
  flush();
  body.innerHTML = out.length ? out.join("") : `<div class="empty">${LOGV.lines.length ? "Nothing matches this filter." : "No log yet."}</div>`;
  if (!LOGV.paused) body.scrollTop = body.scrollHeight;
}

async function loadLog() {
  let lines = [];
  try { lines = await invoke("read_log", { lines: 1500 }); } catch { /* no log yet */ }
  LOGV.lines = lines;
  renderLog();
}

// Lines pushed the instant they are written, over the same connection as everything
// else. The file remains the source of truth; this is a live echo on top of it.
function onLogLine(ev) {
  const { msg } = parseLogLine(ev.line);
  if (/^feed silent for over/.test(msg)) LOGA.gaps += 1;
  if (/^leg drift/.test(msg)) { ingestDrift(msg); if (S.view === "live") paintDrift(); }
  LOGV.lines.push(ev.line);
  if (LOGV.lines.length > 3000) LOGV.lines.splice(0, LOGV.lines.length - 3000);
  if (S.view === "log" && !LOGV.paused) renderLog();
}

for (const b of document.querySelectorAll("#logFilters .chip")) {
  b.onclick = () => {
    LOGV.filter = b.dataset.f;
    for (const c of document.querySelectorAll("#logFilters .chip")) c.classList.toggle("on", c === b);
    renderLog();
  };
}
$("logSearch").oninput = (e) => { LOGV.query = e.target.value.trim().toLowerCase(); renderLog(); };
$("logPause").onclick = () => {
  LOGV.paused = !LOGV.paused;
  $("logPause").setAttribute("aria-pressed", String(LOGV.paused));
  $("logPause").title = LOGV.paused ? "Resume following" : "Pause following";
  if (!LOGV.paused) renderLog();
};

/* ── boot ─────────────────────────────────────────────────────────────── */

$("btnStart").onclick = async () => {
  $("btnStart").disabled = true;
  try { await invoke("bot_start"); }
  catch (e) { await showFailure(String(e)); }
  refreshStatus();
};
$("btnStop").onclick = async () => {
  $("btnStop").disabled = true;
  try { await invoke("bot_stop"); } catch (e) { await showFailure(String(e)); }
  refreshStatus();
};
$("btnArchive").onclick = async () => {
  // Asked before doing, because it ends the run in progress. Nothing here deletes
  // anything: an operator who hesitates should hesitate over "the run stops".
  const running = S.status && (S.status.state === "running" || S.status.state === "starting");
  const ask = running
    ? "Stop the bot, file this run in the archive, and start a fresh one?\n\nThe current ledger stays readable under History."
    : "File this run in the archive and begin an empty ledger?\n\nThe current ledger stays readable under History.";
  if (!confirm(ask)) return;
  $("btnArchive").disabled = true;
  try {
    const r = await invoke("archive_run", { restart: running });
    if (r && r.restartError) await showFailure("Archived, but the bot did not restart: " + r.restartError);
  } catch (e) {
    await showFailure(String(e));
  }
  refreshStatus();
};
for (const b of document.querySelectorAll("nav button")) b.onclick = () => setView(b.dataset.view);

$("themeBtn").onclick = () => {
  const next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
  document.documentElement.setAttribute("data-theme", next);
  try { localStorage.setItem("theme", next); } catch {}
  requestAnimationFrame(drawAll);
};
try {
  const saved = localStorage.getItem("theme");
  if (saved) document.documentElement.setAttribute("data-theme", saved);
} catch {}

markStale();
paintFunnel();
paintReasons();
paintDrift();
paintHealth();
refreshStatus();
loadHistory(null);
connect();
// The rail is visible on every view, so its state cannot wait for Parameters to open.
paintMode();
paintWallet();
paintLimits();
paintDryRun();
invoke("read_log", { lines: 20000 }).then((lines) => ingestLog(lines, true)).catch(() => {});
setInterval(refreshStatus, 2000);
setInterval(sampleDivergence, 1000);
setInterval(() => { if (S.view === "history") loadHistory(S.viewingArchive); }, 20000);
// A slow fallback resync for the Log view; live lines arrive over the WebSocket.
setInterval(() => { if (S.view === "log" && !LOGV.paused) loadLog(); }, 8000);
setInterval(paintBotWell, 15000);
window.addEventListener("resize", () => requestAnimationFrame(drawAll));

// ---------------------------------------------------------------------------
// The rail's mode and wallet panel: the same commands the Parameters tab uses,
// rendered next to the Start button, because whether a run will sign anything and
// what it would sign with belong beside the control that starts it. The typed LIVE
// confirmation is kept: the rail is the convenient place to arm live trading, which
// is exactly why it must not also be the easy place to do it by accident.
// ---------------------------------------------------------------------------

function paintRail() {
  const m = window.__mode;
  const badge = $("railModeBadge");
  if (!m) { badge.textContent = "…"; return; }
  const live = m.effective === "live";
  badge.textContent = live ? "LIVE" : "DEMO";
  badge.className = "badge " + (live ? "badge-live" : "badge-demo");
  $("segDemo").classList.toggle("on", !live);
  $("segLive").classList.toggle("on", live);
  const note = $("railArmedNote");
  if (live) {
    note.hidden = false;
    const d = window.__dry;
    note.innerHTML = d && !d.effective
      ? "Armed and <b>submitting</b> through Jito."
      : "Armed. Nothing is submitted while dry run is on — see Parameters.";
  } else if (!m.allowLiveSet) {
    note.hidden = false;
    note.innerHTML = "<code>CRYPTOBOT_ALLOW_LIVE</code> is not set, so Live cannot be armed.";
  } else {
    note.hidden = true;
  }
}

function paintRailWallet() {
  const addr = window.__address;
  $("railAddr").textContent = addr ? addr.slice(0, 6) + "…" + addr.slice(-6) : "no key — import one under Parameters";
  $("railAddr").title = addr ? "Copy " + addr : "";
  const h = window.__holdings;
  const bal = $("railBal"), ready = $("railReady");
  bal.classList.remove("reading");
  if (!h) { bal.textContent = "—"; ready.textContent = ""; ready.className = "rail-ready"; return; }
  const held = h.tokens.filter((t) => parseFloat(t.amount) > 0);
  const empty = h.tokens.length - held.length;
  const rows = [`<span class="big">${esc(parseFloat(h.sol).toFixed(4))} <span class="sym">SOL</span></span>`];
  for (const t of held) {
    const lab = t.symbol || t.mint.slice(0, 4) + "…" + t.mint.slice(-4);
    rows.push(`<div>${esc(t.amount)} <span class="sym">${esc(lab)}</span></div>`);
  }
  if (empty) rows.push(`<div class="more">+${empty} empty token account${empty > 1 ? "s" : ""}</div>`);
  bal.innerHTML = rows.join("");
  ready.className = "rail-ready " + (h.readiness.canTrade ? "ok" : "no");
  ready.textContent = h.readiness.canTrade ? "Can trade" : "Cannot trade — " + h.readiness.reason;
}

$("railAddr").onclick = async () => {
  if (!window.__address) return;
  try {
    await navigator.clipboard.writeText(window.__address);
    $("railAddr").textContent = "copied";
    setTimeout(paintRailWallet, 900);
  } catch { /* a rail with no clipboard is not worth an error box */ }
};

$("railRefresh").onclick = () => paintBalances(false);

function railShowArm(show) {
  $("railArm").hidden = !show;
  if (show) { $("railConfirm").value = ""; $("railConfirm").focus(); }
}

$("segDemo").onclick = async () => {
  railShowArm(false);
  if (window.__mode && window.__mode.effective === "paper") return;
  await applyMode("paper", "");
};

// Selecting Live only *offers* to arm. Nothing is written until the word is typed and
// Arm live is pressed, so a stray click on a narrow rail costs nothing.
$("segLive").onclick = () => {
  if (window.__mode && window.__mode.effective === "live") return;
  railShowArm(true);
};
$("railCancel").onclick = () => railShowArm(false);
$("railApply").onclick = async () => { await applyMode("live", $("railConfirm").value); };

/// One path for both controls, so the rail and the Parameters tab cannot drift.
async function applyMode(mode, confirm) {
  const note = $("railArmedNote");
  note.hidden = false;
  note.textContent = "Applying…";
  try {
    const r = await invoke("set_mode", { mode, confirm });
    railShowArm(false);
    await paintMode();
    paintRail();
    const bits = [`Mode is now ${r.mode}.`];
    if (r.archived) bits.push(`Previous run archived as ${r.archived}.`);
    if (r.restartError) bits.push(`The bot did not restart: ${r.restartError}`);
    else if (r.restarted) bits.push("The bot restarted.");
    $("modeResult").textContent = bits.join(" ");
  } catch (e) {
    // Shown in the rail, where the button was pressed: a refusal that only appears on
    // another tab reads as nothing having happened.
    note.hidden = false;
    note.textContent = "" + e;
  }
}

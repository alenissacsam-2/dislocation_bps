/* cryptobot-desk — window logic.
 *
 * Three independent data paths, deliberately not unified:
 *   1. live telemetry  — WebSocket to the bot, works only while it runs
 *   2. history         — Tauri command reading SQLite, works with the bot stopped
 *   3. control         — Tauri command driving the child process
 * Keeping them separate is why a dead bot degrades path 1 alone: the window still
 * opens, history still renders, and the controls still work.
 */

const invoke = window.__TAURI__.core.invoke;

const S = {
  status: null,
  history: null,
  view: "live",
  ws: null,
  wsLive: false,
  // pool address -> { pair, dex, price }
  pools: new Map(),
  // pair -> Set(pool address)
  pairs: new Map(),
  // pool address -> [bps deviation from consensus, ...]
  div: new Map(),
  divPair: null,
  // [{ disl, fee }, ...]
  wall: [],
  routes: [],
  viewingArchive: null,
};

const MAX_POINTS = 240;
const $ = (id) => document.getElementById(id);
const fmt = (n, d = 2) => (Number.isFinite(n) ? n.toFixed(d) : "—");
const money = (n) => (Number.isFinite(n) ? "$" + n.toFixed(n < 1 ? 4 : 2) : "—");

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

const css = (name) =>
  getComputedStyle(document.documentElement).getPropertyValue(name).trim();

function axes(g, w, h, pad) {
  g.strokeStyle = css("--line");
  g.lineWidth = 1;
  g.beginPath();
  g.moveTo(pad.l, pad.t);
  g.lineTo(pad.l, h - pad.b);
  g.lineTo(w - pad.r, h - pad.b);
  g.stroke();
}

function label(g, text, x, y, align = "right", color = null) {
  g.fillStyle = color || css("--faint");
  g.font = '10px "IBM Plex Mono", Consolas, monospace';
  g.textAlign = align;
  g.textBaseline = "middle";
  g.fillText(text, x, y);
}

/* ── live telemetry ───────────────────────────────────────────────────── */

function connect() {
  try {
    S.ws = new WebSocket("ws://127.0.0.1:8787/api/stream");
  } catch {
    setTimeout(connect, 3000);
    return;
  }
  S.ws.onopen = () => { S.wsLive = true; };
  S.ws.onmessage = (m) => {
    try { onEvent(JSON.parse(m.data)); } catch { /* a malformed frame is not fatal */ }
  };
  S.ws.onclose = () => { S.wsLive = false; markStale(); setTimeout(connect, 2500); };
  S.ws.onerror = () => { try { S.ws.close(); } catch {} };
}

/* A disconnected feed must never leave the last values on screen looking current.
 * Dimming them is the whole point: stale numbers that look live are the failure this
 * project keeps finding in itself. */
function markStale() {
  for (const id of ["mEdge", "mDisl", "mFee", "mFloor", "mPools"]) {
    $(id).classList.add("dim");
  }
  $("routesEmpty").hidden = false;
  $("routesEmpty").textContent = "Not connected to a running instrument.";
  $("routesBody").innerHTML = "";
  for (const id of ["sSlot", "sLag", "sAge", "sPools", "sStale", "sSweep"]) {
    $(id).textContent = "—";
  }
}

function onEvent(ev) {
  if (ev.type === "status") return onStatus(ev);
  if (ev.type === "routes") return onRoutes(ev);
  if (ev.type === "poolUpdate") return onPool(ev);
}

function onStatus(e) {
  const edge = e.tradeableEdgeBps;
  const el = $("mEdge");
  if (edge === null || edge === undefined) {
    el.textContent = "—";
    el.className = "v dim";
    $("mEdgeSub").textContent = "nothing deep enough to trade";
  } else {
    el.textContent = fmt(edge) + " bps";
    el.className = "v " + (edge > 0 ? "pos" : "neg");
    $("mEdgeSub").textContent = e.tradeableRoute || "";
  }
  $("mFloor").textContent = fmt(e.cheapestRoundTripBps) + " bps";
  $("mFloor").classList.remove("dim");
  $("mPools").textContent = String(e.poolsTracked ?? "—");
  $("mPools").classList.remove("dim");
  $("mPoolsSub").textContent =
    `${e.venues ?? 0} venues · ${e.duplicatePairs ?? 0} pairs quoted twice`;

  $("sSlot").textContent = String(e.slot ?? "—");
  $("sLag").textContent = String(e.slotLag ?? "—");
  $("sAge").textContent = (e.dataAgeSecs ?? 0) + "s";
  $("sPools").textContent = String(e.poolsTracked ?? "—");
  $("sStale").textContent = String(e.staleExcluded ?? 0);
  $("sSweep").textContent = Math.round((e.sweepUs ?? 0) / 1000) + "ms";
  $("sMode").textContent = (e.mode || "").toUpperCase() + (e.feedStalled ? " · FEED STALLED" : "");
}

function onRoutes(e) {
  S.routes = e.rows || [];
  const top = S.routes[0];
  if (top) {
    $("mDisl").textContent = fmt(top.dislocationBps) + " bps";
    $("mFee").textContent = fmt(top.feeBps) + " bps";
    $("mDisl").classList.remove("dim");
    $("mFee").classList.remove("dim");
    S.wall.push({ disl: top.dislocationBps, fee: top.feeBps });
    if (S.wall.length > MAX_POINTS) S.wall.shift();
  }
  const min = e.tradeableMinUsd || 0;
  const body = $("routesBody");
  body.innerHTML = "";
  for (const r of S.routes.slice(0, 12)) {
    const tr = document.createElement("tr");
    const tradeable = r.depthUsd >= min;
    tr.innerHTML =
      `<td>${esc(r.route)}</td>` +
      `<td style="color:var(--muted)">${esc(r.venues)}</td>` +
      `<td class="n" style="color:${r.edgeBps > 0 ? "var(--pos)" : "var(--neg)"}">${fmt(r.edgeBps)}</td>` +
      `<td class="n">${fmt(r.dislocationBps)}</td>` +
      `<td class="n">${fmt(r.feeBps)}</td>` +
      `<td class="n" style="color:${tradeable ? "var(--ink)" : "var(--faint)"}">${
        Number.isFinite(r.depthUsd) ? "$" + Math.round(r.depthUsd).toLocaleString() : "—"}</td>`;
    body.appendChild(tr);
  }
  $("routesEmpty").hidden = S.routes.length > 0;
  if (!S.routes.length) $("routesEmpty").textContent = "No cycles priced in this sweep.";
  drawWall();
}

function onPool(e) {
  S.pools.set(e.pool, { pair: e.pair, dex: e.dex, price: e.price });
  if (!S.pairs.has(e.pair)) S.pairs.set(e.pair, new Set());
  S.pairs.get(e.pair).add(e.pool);
  if (!S.divPair) pickPair();
}

/* The pair quoted by the most venues, because that is where a two-hop round trip
 * exists at all and therefore where divergence means something. */
function pickPair() {
  let best = null, n = 0;
  for (const [pair, set] of S.pairs) if (set.size > n) { n = set.size; best = pair; }
  if (best && best !== S.divPair && n >= 2) { S.divPair = best; S.div.clear(); }
}

/* Sampling on a timer rather than per-event keeps every venue's series on one shared
 * time axis. Events arrive per-pool and unevenly; plotting them as they land would
 * compare venues at different instants and manufacture divergence that is really just
 * staleness. */
function sampleDivergence() {
  if (!S.divPair) { pickPair(); return; }
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
  drawDivergence();
}

const SERIES = ["#FFC24B", "#5FBF8A", "#7AA7E0", "#E0574A", "#C08BE0", "#4FC4C4",
                "#E09A4A", "#96C24B", "#E07AA7", "#8A9BB5"];

function drawDivergence() {
  const cv = $("divChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 46, r: 96, t: 12, b: 20 };

  const series = [...S.div.entries()].filter(([, a]) => a.length > 1);
  if (!series.length) {
    label(g, "waiting for a pair quoted by more than one venue", w / 2, h / 2, "center");
    return;
  }
  let lo = Infinity, hi = -Infinity;
  for (const [, a] of series) for (const v of a) { if (v < lo) lo = v; if (v > hi) hi = v; }
  const span = Math.max(hi - lo, 0.5);
  lo -= span * 0.15; hi += span * 0.15;

  const X = (i, n) => pad.l + ((w - pad.l - pad.r) * i) / Math.max(n - 1, 1);
  const Y = (v) => h - pad.b - ((v - lo) / (hi - lo)) * (h - pad.t - pad.b);

  // zero line: the consensus itself
  g.strokeStyle = css("--line"); g.setLineDash([3, 3]); g.lineWidth = 1;
  g.beginPath(); g.moveTo(pad.l, Y(0)); g.lineTo(w - pad.r, Y(0)); g.stroke();
  g.setLineDash([]);
  axes(g, w, h, pad);
  // The axis title used to sit at pad.t and collided with the topmost tick value.
  // The panel note already says what the unit is, so the ticks carry it alone.
  for (let k = 0; k <= 3; k++) {
    const v = lo + ((hi - lo) * k) / 3;
    label(g, v.toFixed(1), pad.l - 6, Y(v));
  }

  const tags = [];
  series.forEach(([addr, arr], i) => {
    const c = SERIES[i % SERIES.length];
    g.strokeStyle = c; g.lineWidth = 1.4; g.beginPath();
    arr.forEach((v, j) => (j ? g.lineTo(X(j, arr.length), Y(v)) : g.moveTo(X(j, arr.length), Y(v))));
    g.stroke();
    const y = Y(arr[arr.length - 1]);
    g.fillStyle = c; g.beginPath(); g.arc(X(arr.length - 1, arr.length), y, 2.4, 0, 7); g.fill();
    tags.push({ y, want: y, text: (S.pools.get(addr)?.dex || "?").slice(0, 13), c });
  });

  /* Venues quoting one pair sit within a few bps of each other by definition — that is
   * the entire point of the chart — so their end labels land on top of one another and
   * render as an unreadable stack. Push them apart to a minimum spacing, sweeping down
   * then back up so the group stays inside the plot instead of walking off the bottom. */
  const GAP = 12, top = pad.t + 6, bot = h - pad.b - 6;
  tags.sort((a, b) => a.want - b.want);
  for (let i = 1; i < tags.length; i++) {
    if (tags[i].y - tags[i - 1].y < GAP) tags[i].y = tags[i - 1].y + GAP;
  }
  if (tags.length && tags[tags.length - 1].y > bot) {
    tags[tags.length - 1].y = bot;
    for (let i = tags.length - 2; i >= 0; i--) {
      if (tags[i + 1].y - tags[i].y < GAP) tags[i].y = tags[i + 1].y - GAP;
    }
  }
  for (const t of tags) {
    const y = Math.max(top, Math.min(bot, t.y));
    // A leader line, because a label pushed away from its series is otherwise a lie
    // about which line it belongs to.
    g.strokeStyle = t.c; g.globalAlpha = 0.45; g.lineWidth = 1;
    g.beginPath(); g.moveTo(w - pad.r + 1, t.want); g.lineTo(w - pad.r + 5, y); g.stroke();
    g.globalAlpha = 1;
    label(g, t.text, w - pad.r + 8, y, "left", t.c);
  }
  label(g, S.divPair || "", pad.l + 4, h - pad.b + 11, "left");
}

function drawWall() {
  const cv = $("feeChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 38, r: 10, t: 12, b: 18 };
  if (S.wall.length < 2) { label(g, "waiting", w / 2, h / 2, "center"); return; }
  let hi = 0;
  for (const d of S.wall) hi = Math.max(hi, d.disl || 0, d.fee || 0);
  hi = Math.max(hi * 1.15, 1);
  const X = (i) => pad.l + ((w - pad.l - pad.r) * i) / Math.max(S.wall.length - 1, 1);
  const Y = (v) => h - pad.b - (v / hi) * (h - pad.t - pad.b);
  axes(g, w, h, pad);
  for (let k = 0; k <= 2; k++) label(g, ((hi * k) / 2).toFixed(1), pad.l - 6, Y((hi * k) / 2));

  const line = (key, colour, dash) => {
    g.strokeStyle = colour; g.lineWidth = 1.5; g.setLineDash(dash);
    g.beginPath();
    S.wall.forEach((d, i) => (i ? g.lineTo(X(i), Y(d[key] || 0)) : g.moveTo(X(i), Y(d[key] || 0))));
    g.stroke(); g.setLineDash([]);
  };
  line("disl", css("--accent"), []);
  line("fee", css("--neg"), [4, 3]);
  label(g, "gap", w - 14, Y(S.wall[S.wall.length - 1].disl || 0), "right", css("--accent"));
  label(g, "fees", w - 14, Y(S.wall[S.wall.length - 1].fee || 0), "right", css("--neg"));
}

/* ── history ──────────────────────────────────────────────────────────── */

let historyInFlight = false;

async function loadHistory(path) {
  // A gigabyte-scale ledger takes real time to read. Overlapping reads would queue up
  // behind each other on the blocking pool and never catch up.
  if (historyInFlight) return;
  historyInFlight = true;
  if (!S.history) $("pnlNote").textContent = "Reading the ledger…";
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
  if (!H || !H.available) {
    for (const c of cells) { $(c).textContent = "—"; $(c).className = "v dim"; }
    $("hWindow").textContent = H ? (H.reason || "") : "";
    $("pnlNote").textContent = "No ledger to read yet. Start the instrument to begin one.";
    return;
  }
  const L = H.ladder || {};
  const eps = H.episodes || [];
  const curve = H.curve || [];
  const last = curve[curve.length - 1];

  $("hHours").textContent = fmt(H.hoursObserved, 2) + " h";
  $("hHours").className = "v";
  $("hWindow").textContent = (H.firstAt || "").slice(5, 16) + " → " + (H.lastAt || "").slice(5, 16);

  $("hOpps").textContent = last ? last.episodes.toLocaleString() : "—";
  $("hOpps").className = "v";
  $("hTaken").textContent = last ? last.taken.toLocaleString() : "—";
  $("hTaken").className = "v";
  $("hTakenSub").textContent = last && last.episodes
    ? ((last.taken / last.episodes) * 100).toFixed(1) + "% of what was seen" : "";

  $("hNet").textContent = money(L.realisedUsd ?? 0);
  $("hNet").className = "v " + ((L.realisedUsd ?? 0) > 0 ? "pos" : "");

  const pies = eps.map((e) => e.pieUsd).filter((v) => v > 0).sort((a, b) => a - b);
  $("hMedian").textContent = pies.length ? money(pies[Math.floor(pies.length / 2)]) : "—";
  $("hMedian").className = "v";

  const perHour = H.hoursObserved > 0 ? (L.realisedUsd ?? 0) / H.hoursObserved : 0;
  $("pnlNote").textContent =
    `${money(L.realisedUsd ?? 0)} over ${fmt(H.hoursObserved, 2)} hours — ` +
    `${money(perHour)} an hour, ${money(perHour * 24)} a day at this rate.` +
    (S.viewingArchive ? "  (archived run)" : "");

  paintLadder(L);
  paintContest(H);
  drawPnl(curve);
  drawScatter(eps);
}

function paintLadder(L) {
  const el = $("ladder");
  el.innerHTML = "";
  const rungs = L.rungs || [];
  if (!rungs.length) { el.innerHTML = '<div class="empty">No ladder measured yet.</div>'; return; }
  const ceiling = Math.max(L.atOptimalUsd || 0, ...rungs.map((r) => r[1]), 1e-9);
  const row = (name, value, accent) => {
    const d = document.createElement("div");
    d.className = "bar-row";
    const pct = Math.max(0, Math.min(100, (value / ceiling) * 100));
    d.innerHTML =
      `<div class="bar-lab">${name}</div>` +
      `<div class="bar-track"><div class="bar-fill" style="width:${pct}%${
        accent ? "" : ";background:var(--faint)"}"></div></div>` +
      `<div class="bar-val">${money(value)}</div>`;
    el.appendChild(d);
  };
  for (const [book, paid] of rungs) row("$" + book.toLocaleString(), paid, true);
  row("unlimited", L.atOptimalUsd || 0, false);
  row("actual", L.realisedUsd || 0, true);
}

function paintContest(H) {
  const el = $("contest");
  const c = H.contest || {};
  if (!H.contestHasEvidence) {
    el.innerHTML =
      `<p class="note">Not enough evidence yet — ${c.contestedEpisodes ?? 0} declined and ` +
      `${c.uncontestedEpisodes ?? 0} not. The comparison needs at least 20 of each before ` +
      `it says anything, and a rate computed from fewer would look like a finding.</p>`;
    return;
  }
  const a = (H.contestSurvivalRate ?? 0) * 100;
  const b = (H.uncontestedSurvivalRate ?? 0) * 100;
  el.innerHTML =
    `<table><tbody>
      <tr><td>Declined as contested</td><td class="n">${(c.contestedEpisodes ?? 0).toLocaleString()}</td></tr>
      <tr><td>…still there a slot later</td><td class="n">${a.toFixed(1)}%</td></tr>
      <tr><td>Everything else</td><td class="n">${(c.uncontestedEpisodes ?? 0).toLocaleString()}</td></tr>
      <tr><td>…still there a slot later</td><td class="n">${b.toFixed(1)}%</td></tr>
      <tr><td>Value declined</td><td class="n">${money(c.declinedUsd ?? 0)}</td></tr>
    </tbody></table>
    <p class="note">${
      a < b
        ? `Declined opportunities vanish about ${(b / Math.max(a, 0.01)).toFixed(1)}× faster, which is
           what losing a race looks like. It is not proof: large gaps also close fast for purely
           mechanical reasons, and this test cannot separate the two.`
        : `Declined opportunities survive at least as often as the rest — so the classifier is not
           detecting a race, and is more likely keying on size.`
    }</p>`;
}

function drawPnl(curve) {
  const cv = $("pnlChart"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 52, r: 12, t: 12, b: 18 };
  if (curve.length < 2) { label(g, "no history yet", w / 2, h / 2, "center"); return; }
  const hi = Math.max(...curve.map((c) => c.realisedUsd), 1e-6) * 1.12;
  const X = (i) => pad.l + ((w - pad.l - pad.r) * i) / (curve.length - 1);
  const Y = (v) => h - pad.b - (v / hi) * (h - pad.t - pad.b);
  axes(g, w, h, pad);
  for (let k = 0; k <= 3; k++) label(g, "$" + ((hi * k) / 3).toFixed(2), pad.l - 6, Y((hi * k) / 3));

  g.beginPath();
  curve.forEach((c, i) => (i ? g.lineTo(X(i), Y(c.realisedUsd)) : g.moveTo(X(i), Y(c.realisedUsd))));
  g.lineTo(X(curve.length - 1), h - pad.b); g.lineTo(X(0), h - pad.b); g.closePath();
  const grad = g.createLinearGradient(0, pad.t, 0, h - pad.b);
  grad.addColorStop(0, css("--accent-soft")); grad.addColorStop(1, "transparent");
  g.fillStyle = grad; g.fill();

  g.strokeStyle = css("--accent"); g.lineWidth = 1.6; g.beginPath();
  curve.forEach((c, i) => (i ? g.lineTo(X(i), Y(c.realisedUsd)) : g.moveTo(X(i), Y(c.realisedUsd))));
  g.stroke();
  const lastY = Y(curve[curve.length - 1].realisedUsd);
  g.fillStyle = css("--accent");
  g.beginPath(); g.arc(X(curve.length - 1), lastY, 3, 0, 7); g.fill();
}

function drawScatter(eps) {
  const cv = $("scatter"); if (!cv) return;
  const p = prep(cv); if (!p) return;
  const { g, w, h } = p;
  const pad = { l: 52, r: 14, t: 12, b: 26 };
  const pts = eps.filter((e) => e.pieUsd > 0);
  if (!pts.length) { label(g, "no episodes yet", w / 2, h / 2, "center"); return; }

  // Lifetime is in whole slots and is frequently zero — gone before the next slot —
  // so it is shifted by one before the log. Dropping the zeros instead would delete
  // exactly the population that matters most.
  const lx = (e) => Math.log10(e.lifetimeSlots + 1);
  const ly = (e) => Math.log10(e.pieUsd);
  const xs = pts.map(lx), ys = pts.map(ly);
  const x0 = 0, x1 = Math.max(...xs, 0.4);
  const y0 = Math.min(...ys), y1 = Math.max(...ys);
  const X = (v) => pad.l + ((v - x0) / Math.max(x1 - x0, 1e-9)) * (w - pad.l - pad.r);
  const Y = (v) => h - pad.b - ((v - y0) / Math.max(y1 - y0, 1e-9)) * (h - pad.t - pad.b);
  axes(g, w, h, pad);

  for (let d = Math.ceil(y0); d <= Math.floor(y1); d++) {
    label(g, "$" + Math.pow(10, d).toPrecision(1), pad.l - 6, Y(d));
  }
  for (let s = 0; s <= Math.floor(x1 * 10) / 10 + 1; s++) {
    const v = Math.log10(Math.pow(10, s));
    const x = X(v);
    if (x > w - pad.r - 16) break;
    label(g, String(Math.pow(10, s) - (s === 0 ? 1 : 0)), x, h - pad.b + 10, "center");
  }
  label(g, "slots survived →", pad.l, h - 6, "left");

  for (const e of pts) {
    g.fillStyle = e.taken ? css("--pos") : e.contested ? css("--accent") : css("--faint");
    g.globalAlpha = e.taken ? 0.9 : 0.42;
    g.beginPath(); g.arc(X(lx(e)), Y(ly(e)), e.taken ? 2.6 : 1.7, 0, 7); g.fill();
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
  stopped: "Nothing running. History below is still readable.",
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
  $("dot").className = "dot " + ({ running: "run", starting: "run", foreign: "foreign", failed: "fail" }[s.state] || "");
  $("stateLabel").textContent = STATE_LABEL[s.state] || s.state;
  $("stateNote").textContent = !s.botExePresent
    ? "No cb-bot.exe found. Build it before starting."
    : (STATE_NOTE[s.state] || "");
  $("btnStart").disabled = s.state !== "stopped" || !s.botExePresent;
  // A foreign process was not started here and must never be killed from here.
  $("btnStop").disabled = !(s.state === "running" || s.state === "starting");
  $("railPath").textContent = s.root || "";
  $("sLedger").textContent = s.ledgerPresent ? "cryptobot.db" : "none";
}

async function showFailure(msg) {
  let lines = [];
  try { lines = await invoke("read_log", { lines: 40 }); } catch { /* log may not exist */ }
  $("logBody").innerHTML =
    `<b>${esc(msg)}</b>\n\n` + (lines.length ? esc(lines.join("\n")) : "(log is empty)");
  setView("log");
}

function esc(s) {
  return String(s ?? "").replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
}

/* ── views ────────────────────────────────────────────────────────────── */

function setView(v) {
  S.view = v;
  for (const b of document.querySelectorAll("nav button")) {
    b.classList.toggle("on", b.dataset.view === v);
  }
  for (const sec of document.querySelectorAll(".view")) {
    sec.hidden = sec.id !== "view-" + v;
  }
  if (v === "history") { loadHistory(S.viewingArchive); }
  if (v === "runs") loadRuns();
  if (v === "log") loadLog();
  if (v === "config") loadConfig();
  requestAnimationFrame(drawAll);
}

function drawAll() { drawDivergence(); drawWall(); if (S.history) paintHistory(); }

/* ── config ───────────────────────────────────────────────────────────── */

async function loadConfig() {
  try {
    const p = await invoke("read_config");
    $("fCapital").value = p.capitalUsd;
    $("fBuffer").value = p.feeBufferUsd;
    $("fMinTrade").value = p.minTradeUsd;
    $("fHops").value = p.maxHops;
  } catch (e) {
    $("saveResult").textContent = "Could not read config.toml: " + e;
  }
  try { $("fAutostart").checked = await invoke("get_autostart"); } catch { /* not registrable */ }
  $("fAutorestart").checked = await invoke("get_auto_restart");
}

$("fAutostart").onchange = async (e) => {
  try { await invoke("set_autostart", { on: e.target.checked }); }
  catch (err) { $("saveResult").textContent = "Could not change autostart: " + err;
                e.target.checked = !e.target.checked; }
};
$("fAutorestart").onchange = (e) => invoke("set_auto_restart", { on: e.target.checked });

$("btnSave").onclick = async () => {
  const params = {
    capitalUsd: parseFloat($("fCapital").value),
    feeBufferUsd: parseFloat($("fBuffer").value),
    minTradeUsd: parseFloat($("fMinTrade").value),
    maxHops: parseInt($("fHops").value, 10),
  };
  $("saveResult").textContent = "Saving…";
  try {
    const r = await invoke("save_config", { params, restart: true });
    const bits = ["Saved."];
    bits.push(r.archived ? `Archived ${r.archived}.` : "Nothing to archive.");
    if (r.wasRunning) bits.push(r.restarted ? "Restarted." : "Restart FAILED: " + (r.restartError || "?"));
    else bits.push("The instrument was not running, so nothing was restarted.");
    $("saveResult").textContent = bits.join(" ");
    refreshStatus();
  } catch (e) {
    $("saveResult").textContent = "Refused: " + e;
  }
};

/* ── runs ─────────────────────────────────────────────────────────────── */

async function loadRuns() {
  let runs = [];
  try { runs = await invoke("read_archives"); } catch (e) {
    $("runsNote").textContent = "Could not list archives: " + e;
    return;
  }
  const body = $("runsBody");
  body.innerHTML = "";
  $("runsEmpty").hidden = runs.length > 0;
  for (const r of runs) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td class="num">${esc(r.name)}</td>` +
      `<td class="n">${(r.bytes / 1048576).toFixed(1)} MB</td><td></td>`;
    const btn = document.createElement("button");
    btn.textContent = "Open read-only";
    btn.onclick = () => { loadHistory(r.path); setView("history"); };
    tr.lastChild.appendChild(btn);
    body.appendChild(tr);
  }
  $("runsNote").textContent = S.viewingArchive
    ? "History is currently showing an archived run, not the live ledger."
    : "";
}

/* ── log ──────────────────────────────────────────────────────────────── */

async function loadLog() {
  let lines = [];
  try { lines = await invoke("read_log", { lines: 400 }); } catch { /* no log yet */ }
  $("logBody").textContent = lines.length ? lines.join("\n") : "No log yet.";
  $("logBody").scrollTop = $("logBody").scrollHeight;
}

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
for (const b of document.querySelectorAll("nav button")) {
  b.onclick = () => setView(b.dataset.view);
}
$("themeBtn").onclick = () => {
  const now = document.documentElement.getAttribute("data-theme");
  const next = now === "dark" ? "light" : now === "light" ? "dark"
    : (matchMedia("(prefers-color-scheme: dark)").matches ? "light" : "dark");
  document.documentElement.setAttribute("data-theme", next);
  localStorage.setItem("theme", next);
  requestAnimationFrame(drawAll);
};
const saved = localStorage.getItem("theme");
if (saved) document.documentElement.setAttribute("data-theme", saved);

markStale();
refreshStatus();
loadHistory(null);
connect();
setInterval(refreshStatus, 2000);
setInterval(sampleDivergence, 1000);
setInterval(() => { if (S.view === "history") loadHistory(S.viewingArchive); }, 20000);
window.addEventListener("resize", () => requestAnimationFrame(drawAll));

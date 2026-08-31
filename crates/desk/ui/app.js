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

// The endpoints used when nothing is configured. Kept in step with
// cb_desk::config::PUBLIC_HTTP / PUBLIC_WS.
const PUBLIC_HTTP = "https://api.mainnet-beta.solana.com";
const PUBLIC_WS = "wss://api.mainnet-beta.solana.com";

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
  paintRace(H.race);
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

function paintRace(R) {
  const el = $("race");
  el.innerHTML = "";
  if (!R || !R.rungs || !R.rungs.length || !R.declinedEpisodes) {
    el.innerHTML =
      '<div class="empty">Nothing refused as contested yet — the run has no race to price.</div>';
    return;
  }
  const top = Math.max(...R.rungs.map((r) => r[1]), 1e-9);
  for (const [p, got] of R.rungs) {
    const d = document.createElement("div");
    d.className = "bar-row";
    const now = p === 0;
    d.innerHTML =
      `<div class="bar-lab">${now ? "now" : (p * 100).toFixed(0) + "%"}</div>` +
      `<div class="bar-track"><div class="bar-fill" style="width:${
        Math.max(0, Math.min(100, (got / top) * 100))}%${now ? ";background:var(--faint)" : ""}"></div></div>` +
      `<div class="bar-val">${money(got)}</div>`;
    el.appendChild(d);
  }
  const note = document.createElement("div");
  note.className = "bar-lab";
  note.style.marginTop = "4px";
  note.textContent =
    `${R.declinedEpisodes} episodes worth ${money(R.declinedNetUsd)} net refused for being contested` +
    (R.declinedUnprofitableEpisodes
      ? `; a further ${R.declinedUnprofitableEpisodes} were already negative and are rightly refused`
      : "");
  el.appendChild(note);
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
    $("fSlippage").value = p.slippageBps;
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
  // Worth stating rather than leaving to be inferred: an installed copy records to a
  // folder the operator never chose and would otherwise have no way to find.
  try { $("fRoot").textContent = await invoke("get_root"); }
  catch { $("fRoot").textContent = "unknown"; }
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
    lines.push(`<b>CRYPTOBOT_DRY_RUN=${d.envOverride} is set and overrides the file.</b> `
      + `The file says <code>${d.dryRun}</code>; the bot will use <code>${d.effective}</code>.`);
  }
  lines.push(d.effective
    ? "<b>Dry run.</b> Transactions are built, signed and simulated against live state, "
      + "and none are submitted."
    : "<b>Real transactions will be submitted.</b> Every one is still simulated first and "
      + "abandoned unless the simulated balance clears the profit floor — but money can "
      + "move from here.");
  $("dryState").innerHTML = lines.join("<br><br>");

  $("btnAllowSpend").disabled = !d.effective;
  $("btnDryRun").disabled = d.effective;
  $("spendConfirmWrap").hidden = !d.effective;
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

// Never asks. Making the safe direction cheap is the whole point of asking on the
// other one.
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
// The control shows three things that can disagree, because in this system they
// genuinely can: what the file says, what the bot will actually run as once the
// environment is applied over it, and whether this build can execute at all.
// Showing only the first would tell the operator the opposite of the truth in the one
// case where it matters.

async function paintMode() {
  let m;
  try { m = await invoke("read_mode"); }
  catch (e) { $("modeState").textContent = "Could not read mode: " + e; return; }
  window.__mode = m;

  $("modeDemo").checked = m.effective === "paper";
  $("modeLive").checked = m.effective === "live";

  const lines = [];
  if (m.envOverride) {
    lines.push(`<b>CRYPTOBOT_MODE=${m.envOverride} is set in the environment and overrides the file.</b> `
      + `The file says <code>${m.mode}</code>; the bot will run as <code>${m.effective}</code>. `
      + `This application cannot change that — unset the variable and restart it.`);
  }
  if (!m.executionImplemented) {
    lines.push("<b>Live execution is not built in this binary.</b> Demo is the whole of what "
      + "works here.");
  } else if (m.effective === "live") {
    lines.push("<b>Live is armed.</b> Whether anything is actually submitted depends on "
      + "<code>dry_run</code> in <code>config.toml</code>: while it is true the bot builds, "
      + "signs and simulates real transactions and sends none. That is a second, separate "
      + "decision from this switch.");
  }
  lines.push(m.allowLiveSet
    ? "<code>CRYPTOBOT_ALLOW_LIVE=1</code> is set — the outside half of the guard is open."
    : "<code>CRYPTOBOT_ALLOW_LIVE</code> is not set, so Live cannot be armed and the bot "
      + "would refuse to start against a live config. That is the half of the guard that "
      + "lives outside this application, and it deliberately does not set it for you. "
      + "In PowerShell:<br><br><code>setx CRYPTOBOT_ALLOW_LIVE 1</code><br><br>"
      + "then close and reopen this application — a process inherits its environment at "
      + "start, so a variable set after launch is invisible to it.");
  $("modeState").innerHTML = lines.join("<br><br>");
  paintRail();
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
// Saved separately from the trading parameters, and deliberately without archiving the
// run: limits bound what may be signed, they do not change how anything is measured.

const LIMIT_FIELDS = {
  fMaxPos:   "maxPositionUsd",
  fMaxLoss:  "maxDailyLossUsd",
  fMinNet:   "minNetProfitUsd",
  fMaxFails: "maxConsecutiveFailures",
};

async function paintLimits() {
  try {
    const l = await invoke("read_limits");
    for (const [id, key] of Object.entries(LIMIT_FIELDS)) $(id).value = l[key];
    window.__limits = l;
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
// long as it takes to hand it to the backend. Everything here exists to keep that
// window short: the field is cleared on success and on failure alike, because a
// rejected paste left sitting in a form is still a secret sitting in a form.

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

  // The address is shown whether or not the key is unlocked. Seeing what an address
  // holds should never require the ability to spend from it, and the public key sits in
  // the clear beside the ciphertext precisely so this works without a passphrase.
  $("accountPanel").hidden = false;
  $("accAddress").textContent = s.pubkey || "—";
  window.__address = s.pubkey || null;

  // Fetch once when the panel first appears, then only on request. Automatic on open
  // because an operator asking "what do I hold" should not have to press anything;
  // once because the endpoint is shared with the instrument and a balance does not
  // change on its own.
  if (window.__address && !window.__balancesFetched) {
    window.__balancesFetched = true;
    paintBalances(true);
  }
  paintRailWallet();
}

// Balances are fetched on demand rather than on a timer. A poll would put this app on
// somebody's RPC quota for a number that only changes when the operator does something,
// and the endpoint is shared with the instrument that is actually working.
async function paintBalances(quiet) {
  const holdings = $("accBalances"), verdict = $("accReadiness");
  if (!window.__address) { verdict.textContent = ""; return; }
  if (!quiet) holdings.textContent = "reading…";
  let h;
  try { h = await invoke("wallet_balances"); }
  catch (e) {
    holdings.textContent = "could not read balances";
    verdict.className = "acct-verdict";
    verdict.textContent = "" + e;
    return;
  }
  window.__holdings = h;

  const rows = [`<div><span class="amt">${h.sol}</span> <span class="sym">SOL</span></div>`];
  for (const t of h.tokens) {
    // An unnamed mint is shown by its address rather than a guessed ticker. Two tokens
    // called USD-something that are not the same token is a normal Tuesday on Solana.
    const label = t.symbol
      ? `<span class="sym">${t.symbol}</span>`
      : `<span class="mint">${t.mint}</span>`;
    rows.push(`<div><span class="amt">${t.amount}</span> ${label}</div>`);
  }
  holdings.innerHTML = rows.join("");

  verdict.className = "acct-verdict " + (h.readiness.canTrade ? "ok" : "no");
  verdict.innerHTML = (h.readiness.canTrade ? "<b>Can trade.</b> " : "<b>Cannot trade.</b> ")
    + h.readiness.reason;
  $("accSource").textContent = "from " + h.rpc;

  // Both panels quote these numbers, so keep all three in step.
  paintLiveAccount();
  paintRailWallet();
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

// What Live would actually sign with, shown at the moment Live is selected rather than
// after it is applied. The operator is being asked to arm real money; the address and
// what it holds are the two facts that decision needs, and making them look them up
// somewhere else is how the wrong wallet gets armed.
function paintLiveAccount() {
  const box = $("liveAccount");
  if (!$("modeLive").checked) { box.hidden = true; return; }
  box.hidden = false;
  box.className = "warn";

  const addr = window.__address;
  if (!addr) {
    box.innerHTML = "<b>No key is configured.</b> Live has nothing to sign with. "
      + "Import one under Wallet below.";
    return;
  }

  const h = window.__holdings;
  const bits = [`<b>Live would sign with:</b><br><code>${addr}</code>`];

  if (!h) {
    bits.push("Balances not read yet — press <b>Refresh balances</b> under Wallet to see "
      + "what this address holds before arming it.");
  } else {
    const held = [`${h.sol} SOL`]
      .concat(h.tokens.map(t => `${t.amount} ${t.symbol || t.mint.slice(0, 6) + "…"}`));
    bits.push("Holding " + held.join(" · "));
    if (!h.readiness.canTrade) bits.push("<b>This address cannot trade.</b> " + h.readiness.reason);
  }

  const unlocked = window.__walletUnlocked;
  if (!unlocked) {
    bits.push("<b>The key is locked.</b> Unlock it under Wallet — a locked key cannot sign, "
      + "so Live would run without being able to do anything.");
  }
  box.innerHTML = bits.join("<br><br>");
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
  catch (err) { $("saveResult").textContent = "Could not change autostart: " + err;
                e.target.checked = !e.target.checked; }
};
$("fAutorestart").onchange = (e) => invoke("set_auto_restart", { on: e.target.checked });

// Provider shortcuts. They insert a URL *shape*, not a working endpoint — the key has
// to come from that provider's dashboard, which is the only authority on the exact
// form. Appended rather than replacing, because the point of the list is having more
// than one.
for (const b of document.querySelectorAll("#rpcProviders button")) {
  b.onclick = () => {
    const box = $("fRpcHttp");
    const lines = box.value.split("\n").map(l => l.trim()).filter(Boolean);
    const url = b.dataset.rpc;
    if (!lines.includes(url)) lines.push(url);
    box.value = lines.join("\n") + "\n";
    box.focus();
    // Land the caret on the placeholder so the key can be typed straight over it.
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
    slippageBps: parseInt($("fSlippage").value, 10),
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
// The rail is visible on every view, so its state cannot wait for the Parameters tab
// to be opened. `paintWallet` fetches balances once on first paint; both are also
// repainted whenever the Parameters tab reloads, from the same two commands.
paintMode();
paintWallet();
setInterval(refreshStatus, 2000);
setInterval(sampleDivergence, 1000);
setInterval(() => { if (S.view === "history") loadHistory(S.viewingArchive); }, 20000);
window.addEventListener("resize", () => requestAnimationFrame(drawAll));

// ---------------------------------------------------------------------------
// The rail's mode and wallet panel.
//
// The same three commands the Parameters tab uses — read_mode, set_mode,
// wallet_balances — rendered next to the Start button, because whether a run will
// sign anything and what it would sign with are the two facts that belong beside
// the control that starts it. Both places repaint from the same state, so they
// cannot disagree.
//
// The typed LIVE confirmation is kept here rather than simplified into a click.
// The rail is the *convenient* place to arm live trading, which is exactly why it
// must not also be the easy place to do it by accident.
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

  // What the operator most needs to know once armed is not that it is armed — the
  // badge says that — but whether anything can actually leave the machine.
  const note = $("railArmedNote");
  if (live) {
    note.hidden = false;
    note.innerHTML = "Armed. Whether anything is <b>submitted</b> depends on "
      + "<code>dry_run</code> in config.toml.";
  } else if (!m.allowLiveSet) {
    note.hidden = false;
    note.innerHTML = "<code>CRYPTOBOT_ALLOW_LIVE</code> is not set, so Live cannot be armed.";
  } else {
    note.hidden = true;
  }
}

function paintRailWallet() {
  const addr = window.__address;
  $("railAddr").textContent = addr || "no key — import one under Parameters";

  const h = window.__holdings;
  const bal = $("railBal"), ready = $("railReady");
  if (!h) { bal.textContent = "—"; ready.textContent = ""; ready.className = "rail-ready"; return; }

  const rows = [`<div>${h.sol} <span class="sym">SOL</span></div>`];
  for (const t of h.tokens) {
    const label = t.symbol || t.mint.slice(0, 4) + "…" + t.mint.slice(-4);
    rows.push(`<div>${t.amount} <span class="sym">${label}</span></div>`);
  }
  bal.innerHTML = rows.join("");

  ready.className = "rail-ready " + (h.readiness.canTrade ? "ok" : "no");
  ready.textContent = h.readiness.canTrade
    ? "Can trade."
    : "Cannot trade — " + h.readiness.reason;
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

// Selecting Live only *offers* to arm. Nothing is written until the word is typed
// and Arm live is pressed, so a stray click on a narrow rail costs nothing.
$("segLive").onclick = () => {
  if (window.__mode && window.__mode.effective === "live") return;
  railShowArm(true);
};

$("railCancel").onclick = () => railShowArm(false);

$("railApply").onclick = async () => {
  await applyMode("live", $("railConfirm").value);
};

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
    if ($("modeResult")) {
      const bits = [`Mode is now ${r.mode}.`];
      if (r.archived) bits.push(`Previous run archived as ${r.archived}.`);
      if (r.restartError) bits.push(`The bot did not restart: ${r.restartError}`);
      else if (r.restarted) bits.push("The bot restarted.");
      $("modeResult").textContent = bits.join(" ");
    }
  } catch (e) {
    // Shown in the rail, where the button was pressed. A refusal that only appears
    // on another tab reads as nothing having happened.
    note.hidden = false;
    note.className = "rail-hint";
    note.textContent = "" + e;
  }
}

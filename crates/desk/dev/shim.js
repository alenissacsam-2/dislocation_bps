/* Stand-in for the Tauri command bridge, for designing the desk UI in a browser.
 *
 * Read-only by construction: history comes from the running bot's /api/equity, the log
 * from the harness's masked tail of cb-bot.log, and everything else from fixtures that
 * mirror the real return shapes. Commands that would change anything resolve without
 * doing it and say so in the console. Nothing here can start, stop, sign or spend. */
(() => {
  const FIX = {
    bot_status: { state: "running", botExePresent: true, ledgerPresent: true, root: "D:\\Dev\\Quant\\cryptobot" },
    read_config: {
      capitalUsd: 12.4, feeBufferUsd: 0.8, minTradeUsd: 5, maxHops: 3, slippageTenthBps: 1,
      priorityMicroLamports: 8000, rpcHttpUrl: "https://mainnet.helius-rpc.com/?api-key=***",
      rpcWsUrl: "wss://mainnet.helius-rpc.com/?api-key=***",
    },
    read_limits: {
      maxPositionUsd: 13, maxDailyLossUsd: 1, minNetProfitUsd: 0.0002, maxSlippageBps: 30,
      maxConsecutiveFailures: 6, maxDailyTrades: 500, haltCooldownSecs: 600,
    },
    read_mode: { mode: "live", effective: "live", envOverride: null, allowLiveSet: true, executionImplemented: true },
    read_dry_run: { dryRun: false, effective: false, envOverride: null },
    wallet_status: { configured: true, pubkey: "9AGJ73LtgxMoBJza1JStPui4kTCkTtuA7eTHLErDu9dw", unlocked: true },
    wallet_balances: {
      address: "9AGJ73LtgxMoBJza1JStPui4kTCkTtuA7eTHLErDu9dw", lamports: 121585463, sol: "0.121585463",
      tokens: [
        { mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", symbol: "USDC", amount: "0.007904", decimals: 6 },
        { mint: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", symbol: "USDT", amount: "0", decimals: 6 },
        { mint: "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4", symbol: "", amount: "0", decimals: 6 },
        { mint: "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", symbol: "", amount: "0", decimals: 6 },
      ],
      readiness: { canTrade: true, reason: "Enough SOL for fees and rent.", shortByLamports: 0 },
      rpc: "mainnet.helius-rpc.com",
    },
    get_autostart: false,
    get_auto_restart: false,
    get_root: "D:\\Dev\\Quant\\cryptobot",
    read_archives: [
      { name: "cryptobot-20260912-074245.db", path: "archive/cryptobot-20260912-074245.db", bytes: 24879104 },
      { name: "cryptobot-20260912-022414.db", path: "archive/cryptobot-20260912-022414.db", bytes: 82264064 },
      { name: "cryptobot-20260911-132059.db", path: "archive/cryptobot-20260911-132059.db", bytes: 51200000 },
    ],
  };
  const WRITES = new Set([
    "bot_start", "bot_stop", "archive_run", "save_config", "set_dry_run", "set_mode",
    "save_limits", "wallet_import", "wallet_unlock", "wallet_forget", "set_autostart", "set_auto_restart",
  ]);
  async function invoke(cmd, args) {
    if (WRITES.has(cmd)) {
      console.info("[shim] would run", cmd, "— the harness changes nothing");
      if (cmd === "set_mode") return { mode: args.mode, archived: null, restarted: false };
      if (cmd === "save_config") return { archived: null, wasRunning: true, restarted: true };
      return null;
    }
    if ((cmd === "read_history" || cmd === "read_history_at") && !(cmd in FIX)) {
      const r = await fetch("http://127.0.0.1:8787/api/equity");
      return r.json();
    }
    if (cmd === "read_log") {
      const r = await fetch("/dev/log?lines=" + (args?.lines || 400));
      return r.json();
    }
    if (cmd in FIX) return structuredClone(FIX[cmd]);
    throw new Error("shim: no fixture for " + cmd);
  }
  window.__TAURI__ = { core: { invoke } };

  // #demo — a synthetic bot, for designing every panel with the real bot stopped.
  // Everything below is invented and says so in the window title; nothing here reads a
  // wallet or reaches the chain. Shapes mirror cb_server's events exactly.
  const DEMO = new URLSearchParams(location.hash.slice(1)).has("demo");
  if (DEMO) {
    document.title = "cryptobot (demo data)";
    const started = Date.now() - 3.2 * 3600e3;
    let slot = 450_330_000, id = 1;
    const venues = [
      ["p1", "RAY-CL 1bp"], ["p2", "ORCA 2bp"], ["p3", "ORCA 4bp"], ["p4", "RAY-CL 4bp"],
      ["p5", "MET-DL 3.1bp"], ["p6", "RAY-V4 25bp"],
    ];
    const bias = { p1: 0, p2: 0.4, p3: -0.6, p4: 0.9, p5: -1.4, p6: 6.5 };
    const routes = [
      ["SOL → USDC → SOL", "RAY-CL 1bp · ORCA 2bp", 3.0, 19],
      ["SOL → USDT → SOL", "ORCA 2bp · RAY-CL 1bp", 3.0, 24],
      ["SOL → JitoSOL → SOL", "ORCA 1bp · RAY-CL 1bp", 2.0, 60000],
      ["SOL → USDC → USDT → SOL", "RAY-CL 1bp · ORCA 1bp · RAY-CL 2bp", 4.0, 12],
      ["SOL → JUP → SOL", "ORCA 30bp · RAY-CL 25bp", 55.0, 8],
      ["SOL → GRASS → SOL", "RAY-CL 25bp · ORCA 30bp", 55.0, 6],
      ["SOL → BONK → USDC → SOL", "ORCA 30bp · RAY-CL 25bp · RAY-CL 1bp", 56.0, 5],
      ["USDC → USDS → USDC", "RAY-CL 1bp · ORCA 1bp", 2.0, 5800],
    ];
    const reasons = [
      [0.55, () => `refused: this route spends ${47_000_000 + (Math.random() * 3e7) | 0} and guarantees only ${47_000_000 - (Math.random() * 2e4) | 0} back — signing it would authorise a loss`],
      [0.12, () => "not attempted — refused recently and nothing has changed"],
      [0.08, () => `refused: this route guarantees ${4000 + (Math.random() * 3000) | 0} more than it spends, and landing it costs 6000 — the floor would let through a trade that loses its own fee`],
      [0.08, () => "not attempted — the stalest leg is 4 slots behind the feed's own head, over the 2 this will trade on; on fresh state this class is worth 0.37 bps against a 3.00 bps fee"],
      [0.06, () => "refused: the wallet holds no token account for 7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr, and opening one costs 1488440 lamports of rent out of the same balance this trade's profit is measured in — the bot opens one itself once attempts keep asking for this mint"],
      [0.05, () => "sent to Jito; not included, cost nothing"],
      [0.04, () => "simulation: TooLittleOutputReceived"],
      [0.02, () => "submitted and landed"],
    ];
    const pick = () => { let r = Math.random(); for (const [p, f] of reasons) { if ((r -= p) < 0) return f(); } return reasons[0][1](); };

    class FakeSocket {
      constructor() {
        setTimeout(() => this.onopen && this.onopen(), 50);
        const send = (ev) => this.onmessage && this.onmessage({ data: JSON.stringify(ev) });
        this.timers = [
          setInterval(() => {
            slot += 5;
            send({ type: "status", mode: "live", connected: true, slot, slotLag: Math.random() < 0.9 ? 0 : 1,
              poolsTracked: 151, solPriceUsd: 114.9 + Math.sin(Date.now() / 6e4), uptimeSecs: (Date.now() - started) / 1e3 | 0,
              updates: 0, dropped: 1247, reconnects: 2, stalls: 0, dataAgeSecs: 0, tradeableEdgeBps: -1.2 - Math.random(),
              tradeableRoute: routes[0][0], tradeableMinUsd: 5, staleExcluded: 0, feedStalled: false,
              cheapestRoundTripBps: 2, sweepUs: 50_000 + Math.random() * 40_000, subscribed: 217, subscribeErrors: 0,
              reconcileDrift: 5, reconcileChecked: 151, venues: 6, duplicatePairs: 42 });
          }, 2000),
          setInterval(() => {
            send({ type: "routes", tradeableMinUsd: 5, rows: routes.map(([route, v, fee, depth], i) => {
              const disl = fee - 0.8 - i * 0.3 - Math.random() * 1.5;
              return { route, venues: v, hops: route.split("→").length - 1, edgeBps: disl - fee, dislocationBps: disl, feeBps: fee, depthUsd: depth, slot };
            }) });
            for (const [pool, dex] of venues) {
              send({ type: "poolUpdate", pool, pair: "SOL/USDC", dex, price: 114.9 * (1 + (bias[pool] + Math.random() * 0.6) / 1e4) });
            }
          }, 1000),
          setInterval(() => {
            for (let k = 0; k < 4; k++) {
              const neg = Math.random() < 0.97;
              send({ type: "opportunity", id: id, route: routes[k % routes.length][0], skippedReason: neg ? "net negative after tip" : null, tsMs: Date.now() });
              if (!neg) send({ type: "execution", id, opportunityId: id, paper: false, landed: false, realisedUsd: 0, tipPaidUsd: 0, latencyMs: 300, signature: null, reason: pick(), tsMs: Date.now() });
              id += 1;
            }
          }, 700),
        ];
      }
      close() { this.timers.forEach(clearInterval); this.onclose && this.onclose(); }
      send() {}
    }
    window.WebSocket = FakeSocket;

    // History: a plausible 30-hour paper ledger.
    const curve = [], episodes = [];
    let net = 0, eps = 0, taken = 0;
    for (let i = 0; i < 400; i++) {
      eps += 3 + (Math.random() * 6 | 0);
      const t = Math.random() < 0.3;
      if (t) { taken += 1; net += Math.random() * 0.004 - 0.0006; }
      curve.push({ at: new Date(started - 27 * 3600e3 + i * 270e3).toISOString(), realisedUsd: net,
        atCapitalUsd: [net * 3, net * 9, net * 14], atOptimalUsd: net * 20, episodes: eps, taken });
    }
    for (let i = 0; i < 900; i++) {
      const life = Math.exp(Math.random() * 6);
      episodes.push({ lifetimeSlots: life, pieUsd: Math.exp(-3 - Math.log(life) * 0.6 + (Math.random() - 0.5) * 2) / 50,
        taken: Math.random() < 0.25, contested: Math.random() < 0.1 });
    }
    FIX.read_history = {
      available: true, hoursObserved: 30.1, firstAt: curve[0].at, lastAt: curve[curve.length - 1].at, curve, episodes,
      ladder: { rungs: [[100, net * 3], [1000, net * 9], [10000, net * 14]], atOptimalUsd: net * 20, realisedUsd: net },
      race: { rungs: [[0, 0], [0.1, 0.004], [0.25, 0.011], [0.5, 0.022]], declinedEpisodes: 61, declinedNetUsd: 0.031, declinedUnprofitableEpisodes: 12 },
      contest: { contestedEpisodes: 61, uncontestedEpisodes: 840, declinedUsd: 0.031 }, contestHasEvidence: true,
      contestSurvivalRate: 0.279, uncontestedSurvivalRate: 0.153,
    };
    FIX.read_history_at = FIX.read_history;
  }

  // #view=history&theme=light, so a headless browser can capture any view in either
  // theme without clicking.
  const want = new URLSearchParams(location.hash.slice(1));
  if (want.get("theme")) document.documentElement.setAttribute("data-theme", want.get("theme"));
  if (want.get("view")) {
    addEventListener("load", () => setTimeout(() => {
      document.querySelector(`[data-view="${want.get("view")}"]`)?.click();
    }, 300));
  }
})();

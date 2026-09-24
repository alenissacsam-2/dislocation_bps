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
    if (cmd === "read_history" || cmd === "read_history_at") {
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

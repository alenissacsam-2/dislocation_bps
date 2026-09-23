# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

(A Tauri 2 desktop window on Windows rendering plain HTML/CSS/JS from `crates/desk/ui`;
its design language is the web's, not a native OS's.)

## Users

One operator: the owner of the wallet. The window sits open on a screen while the bot
runs, and is glanced at — "is it alive, is anything wrong, did anything trade" should be
answerable in about two seconds. Deeper review (history, archived runs, the log) happens
occasionally and on purpose.

## Product Purpose

cryptobot-desk supervises `cb-bot`, a Solana arbitrage bot that watches AMM pools, prices
every cycle it can see, and — in Live mode — sends trades as Jito bundles of one. The
desk starts and stops the bot, arms Live and real spending behind typed confirmations,
holds the encrypted wallet key, edits parameters and risk limits, and shows what the bot
is seeing and doing.

Success is the operator understanding, at a glance, whether the bot is healthy, where
opportunities die in the pipeline, and what money has moved.

## Positioning

An instrument that measures honestly rather than a trading dashboard that sells hope: it
shows why trades do *not* happen as prominently as when they do, and it never lets a
stale number look live.

## Operating Context

- Desktop window, default 1480×940, minimum 1120×720; dark window chrome by default.
- Data paths: a WebSocket to the bot at `127.0.0.1:8787/api/stream` (status, routes,
  pool updates, opportunities, executions, log lines) that exists only while the bot
  runs; Tauri commands reading the SQLite ledger (history) that work with the bot
  stopped; Tauri commands that drive the child process, config, wallet and limits.
- The bot is usually live with a $12 wallet and a $1 daily loss budget; trades are rare,
  refusals are constant, and most sessions are long and quiet.

## Capabilities and Constraints

- Every existing control and its safety behaviour must survive a redesign: typed `LIVE`
  and `SPEND` confirmations, archive-before-change semantics, the wallet key only ever
  in the password field until handed to the backend, a foreign process never stopped.
- Live data must visibly go stale (dimmed) when the connection drops.
- Analyses the operator asked for: the trade funnel (seen → re-priced → simulated →
  sent → landed, and where each died), why trades fail (refusal reasons, per-venue leg
  drift), money over time (balance, realised P&L, fees and tips, loss budget used), and
  feed and RPC health. The operator delegated further analysis choices to the builder.
- Helius credit usage is not exposed by any API the desk can reach; it must not be
  displayed as if measured.
- CSP allows only `self`, inline styles, data: images, and the local bot API; no remote
  fonts or scripts. Fonts are bundled in `crates/desk/ui/fonts`.

## Brand Commitments

- Name: cryptobot (lower-case), with the gauge-arc mark in the rail.
- The operator asked for a modern look "like claymorphism" — soft, dimensional, tactile
  surfaces — while keeping the screen uncluttered.
- Long explanatory text stays available but is tucked behind an info affordance, shown on
  hover or click, never deleted.

## Evidence on Hand

- Real telemetry and ledgers only; no synthetic numbers may be presented as measured.
- `docs/research/`, `HANDOVER.md` and the ledger archives under `archive/` hold the
  measured history the History view reads.

## Product Principles

1. Honest over hopeful: a refusal is information, and stale data never looks live.
2. Glanceable first, depth on demand: status and anything wrong in two seconds,
   explanation one hover away.
3. Danger is deliberate: arming Live and allowing spending stay two separate, typed acts.
4. Every number names its unit and its source.

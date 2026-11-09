//! Axum routes: a WebSocket event stream, a health endpoint, and a ledger read.
//!
//! # Why there is no longer a UI here
//!
//! This server used to serve `dashboard/dist` as well, and that was the arrangement's
//! defect: the interface was served by the process it observed, so a stopped bot had
//! no interface at all — not a degraded one, none. `cryptobot-desk` inverts that. The
//! server keeps the three endpoints and stops pretending to be a window.

use crate::events::{Event, EventBus};
use axum::{extract::Request, middleware::Next};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::broadcast::error::RecvError;

/// Detections this far apart in slots belong to different episodes. Matches the value
/// the report uses, so the dashboard's P&L and `cb-bot --report` cannot disagree.
const EPISODE_GAP_SLOTS: u64 = 5;

/// Points the equity endpoint will return. Enough to draw a run of days without
/// shipping a row per detection to a browser.
const CURVE_POINTS: usize = 600;

/// Opportunities returned for the value-against-lifetime scatter. Kept by value, so a
/// long run still ships the points that decide the question rather than its dust.
const SCATTER_POINTS: usize = 1500;

#[derive(Clone)]
pub struct AppState {
    pub bus: EventBus,
    pub mode: String,
    /// Ledger to read history from. `None` runs the dashboard live-only: the stream
    /// still works, and the P&L panel says it has no history rather than drawing a
    /// flat line that looks like a run which earned nothing.
    pub ledger_path: Option<String>,
}

/// Serve the API on `addr`.
///
/// # Errors
/// If the address cannot be bound, or the server stops with an error.
pub async fn serve(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/stream", get(ws_handler))
        .route("/api/equity", get(equity))
        .layer(axum::middleware::from_fn(host_guard))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("api listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "ok": true,
        "mode": state.mode,
        "watchers": state.bus.receiver_count(),
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> axum::response::Response {
    // Browsers skip CORS for WebSockets, so any open website could read this stream.
    let origin = headers.get(axum::http::header::ORIGIN).map(|v| v.to_str().unwrap_or(""));
    if !origin_allowed(origin) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| push_events(socket, state.bus)).into_response()
}

/// Refuse any request whose Host is not loopback or the desk webview: a DNS-rebound page
/// reaches us under its own hostname, and CORS does not stop it reading plain GETs.
async fn host_guard(req: Request, next: Next) -> axum::response::Response {
    let host = req.headers().get(axum::http::header::HOST).and_then(|v| v.to_str().ok());
    if host_allowed(host) { next.run(req).await } else { axum::http::StatusCode::FORBIDDEN.into_response() }
}

fn host_allowed(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let name = host.strip_prefix("[::1]").map_or_else(|| host.split(':').next().unwrap_or(""), |_| "[::1]");
    matches!(name, "localhost" | "127.0.0.1" | "[::1]" | "tauri.localhost")
}

/// No Origin = not a browser. Otherwise only loopback pages and the desk webview
/// (`http://tauri.localhost` on Windows, `tauri://localhost` elsewhere).
fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let Some((scheme, rest)) = origin.split_once("://") else { return false };
    let host = rest.strip_prefix("[::1]").map_or_else(
        || rest.split(':').next().unwrap_or(""),
        |_| "[::1]",
    );
    matches!(scheme, "http" | "https" | "tauri")
        && matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "tauri.localhost")
}

/// The run's cumulative P&L, and what the same opportunities would have paid larger
/// books.
///
/// # Why this is a REST read of the ledger rather than a running total on the socket
///
/// A curve accumulated in the browser starts at zero every time someone opens the page,
/// so an overnight run would render as a flat line beginning at the moment you looked
/// at it — a chart that says the opposite of what happened. The ledger already holds
/// the history; reading it back means the P&L survives a reload, a reconnect, and the
/// bot itself restarting.
///
/// SQLite work happens on a blocking thread. `Connection` is neither `Send` nor cheap
/// to hold open across an await, and the alternative — sharing the writer's connection
/// — would let a browser refresh contend with the trading loop's inserts.
async fn equity(State(state): State<AppState>) -> impl IntoResponse {
    let Some(path) = state.ledger_path.clone() else {
        return Json(serde_json::json!({
            "available": false,
            "reason": "this run is not recording to a ledger",
        }));
    };

    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        // Read-only: the dashboard must never be able to touch the measurement, and a
        // browser hitting refresh must never create a ledger file where none existed.
        let ledger = cb_ledger::Ledger::open_read_only(&path)?;
        let curve = ledger.equity_curve(EPISODE_GAP_SLOTS, CURVE_POINTS)?;
        let ladder = ledger.capital_ladder(EPISODE_GAP_SLOTS)?;
        let summary = ledger.summary()?;
        let contest = ledger.contest_audit(EPISODE_GAP_SLOTS)?;
        let scatter = ledger.episode_scatter(EPISODE_GAP_SLOTS, SCATTER_POINTS)?;
        Ok(serde_json::json!({
            "available": true,
            "curve": curve,
            "ladder": ladder,
            "contest": contest,
            "episodes": scatter,
            "contestSurvivalRate": contest.contested_survival_rate(),
            "uncontestedSurvivalRate": contest.uncontested_survival_rate(),
            "contestHasEvidence": contest.has_enough_evidence(),
            "hoursObserved": ledger.hours_observed()?,
            "samples": summary.samples,
            "firstAt": summary.first_at,
            "lastAt": summary.last_at,
        }))
    })
    .await;

    match read {
        Ok(Ok(v)) => Json(v),
        Ok(Err(e)) => {
            tracing::warn!("equity read failed: {e:#}");
            Json(serde_json::json!({ "available": false, "reason": e.to_string() }))
        }
        Err(e) => {
            tracing::warn!("equity task failed: {e:#}");
            Json(serde_json::json!({ "available": false, "reason": "read task failed" }))
        }
    }
}

/// How often a browser is sent pool prices. The bus carries one `PoolUpdate` per account
/// write, several per pool per slot; the window samples prices once a second.
const POOL_FLUSH: std::time::Duration = std::time::Duration::from_millis(250);

/// Hold a `PoolUpdate` back as its pool's latest price; hand anything else straight back.
fn coalesce(pending: &mut HashMap<String, Event>, event: Event) -> Option<Event> {
    if let Event::PoolUpdate { pool, .. } = &event {
        pending.insert(pool.clone(), event);
        return None;
    }
    Some(event)
}

/// Relay bus events to one browser. Pool updates go out at most once per pool per
/// [`POOL_FLUSH`], latest price only; every other event goes out as it happens.
///
/// On `Lagged` we deliberately continue rather than disconnect: a client that missed
/// events during a burst should catch up on current state, not be dropped. Losing
/// history is acceptable; losing the connection is not.
async fn push_events(mut socket: WebSocket, bus: EventBus) {
    let mut rx = bus.subscribe();
    let mut pools: HashMap<String, Event> = HashMap::new();
    let mut flush = tokio::time::interval(POOL_FLUSH);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let out: Vec<Event> = tokio::select! {
            received = rx.recv() => match received {
                Ok(event) => coalesce(&mut pools, event).into_iter().collect(),
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!("dashboard lagged, skipped {skipped} events");
                    vec![Event::Resync { skipped }]
                }
                Err(RecvError::Closed) => break,
            },
            _ = flush.tick() => pools.drain().map(|(_, e)| e).collect(),
        };
        for event in out {
            let Ok(text) = serde_json::to_string(&event) else { continue };
            if socket.send(Message::Text(text.into())).await.is_err() {
                return; // browser went away
            }
        }
    }
}

/// Wire a bus into a server for a run that is recording — so the dashboard can show its history.
#[must_use]
pub fn state_with_ledger(
    bus: EventBus,
    mode: impl Into<String>,
    ledger_path: impl Into<String>,
) -> AppState {
    AppState { bus, mode: mode.into(), ledger_path: Some(ledger_path.into()) }
}

#[cfg(test)]
mod tests {
    use super::{coalesce, host_allowed, origin_allowed, Event};

    fn pool(pool: &str, price: f64) -> Event {
        Event::PoolUpdate {
            pool: pool.into(), dex: String::new(), pair: String::new(),
            price, reserve_a: 0.0, reserve_b: 0.0, slot: 0, ts_ms: 0,
        }
    }

    #[test]
    fn pool_updates_keep_only_the_latest_per_pool_and_everything_else_passes() {
        let mut pending = std::collections::HashMap::new();
        assert!(coalesce(&mut pending, pool("a", 1.0)).is_none());
        assert!(coalesce(&mut pending, pool("a", 2.0)).is_none());
        assert!(coalesce(&mut pending, pool("b", 3.0)).is_none());
        assert_eq!(pending.len(), 2);
        assert!(matches!(pending["a"], Event::PoolUpdate { price, .. } if price == 2.0));
        let log = Event::LogLine { line: "x".into(), ts_ms: 1 };
        assert!(matches!(coalesce(&mut pending, log), Some(Event::LogLine { .. })));
    }

    #[test]
    fn only_loopback_hosts_are_served() {
        for ok in ["127.0.0.1:8787", "localhost", "localhost:8787", "[::1]:8787", "tauri.localhost"] {
            assert!(host_allowed(Some(ok)), "{ok}");
        }
        for bad in [None, Some("evil.com"), Some("evil.com:8787"), Some("127.0.0.1.evil.com"), Some("")] {
            assert!(!host_allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_local_origins_may_open_the_stream() {
        for ok in [None, Some("http://tauri.localhost"), Some("tauri://localhost"),
            Some("http://127.0.0.1:5178"), Some("http://localhost:8787"), Some("http://[::1]:80")]
        {
            assert!(origin_allowed(ok), "{ok:?}");
        }
        for bad in [Some("https://evil.com"), Some("http://localhost.evil.com"),
            Some("http://127.0.0.1.evil.com"), Some("null"), Some("")]
        {
            assert!(!origin_allowed(bad), "{bad:?}");
        }
    }
}

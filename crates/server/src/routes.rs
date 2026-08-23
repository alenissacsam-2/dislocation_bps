//! Axum routes: a WebSocket event stream, a health endpoint, and the static dashboard.

use crate::events::{Event, EventBus};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::net::SocketAddr;
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::{ServeDir, ServeFile};

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

/// Serve the dashboard and event stream on `addr`.
///
/// `static_dir` is the built frontend. If it is missing the API still works, which
/// keeps the bot runnable headless.
pub async fn serve(addr: SocketAddr, state: AppState, static_dir: &str) -> anyhow::Result<()> {
    let index = format!("{static_dir}/index.html");
    let spa = ServeDir::new(static_dir).fallback(ServeFile::new(index));

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/stream", get(ws_handler))
        .route("/api/equity", get(equity))
        .fallback_service(spa)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("dashboard listening on http://{addr}");
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

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| push_events(socket, state.bus))
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

/// Relay bus events to one browser.
///
/// On `Lagged` we deliberately continue rather than disconnect: a client that missed
/// events during a burst should catch up on current state, not be dropped. Losing
/// history is acceptable; losing the connection is not.
async fn push_events(mut socket: WebSocket, bus: EventBus) {
    let mut rx = bus.subscribe();
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Ok(text) = serde_json::to_string(&event) else {
                    continue;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break; // browser went away
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::debug!("dashboard lagged, skipped {skipped} events");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Convenience for wiring a bus into a server without a full bot.
#[must_use]
pub fn state(bus: EventBus, mode: impl Into<String>) -> AppState {
    AppState { bus, mode: mode.into(), ledger_path: None }
}

/// The same, for a run that is recording — so the dashboard can show its history.
#[must_use]
pub fn state_with_ledger(
    bus: EventBus,
    mode: impl Into<String>,
    ledger_path: impl Into<String>,
) -> AppState {
    AppState { bus, mode: mode.into(), ledger_path: Some(ledger_path.into()) }
}

#[allow(unused_imports)]
use Event as _EventInDocs;

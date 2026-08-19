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

#[derive(Clone)]
pub struct AppState {
    pub bus: EventBus,
    pub mode: String,
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
    AppState { bus, mode: mode.into() }
}

#[allow(unused_imports)]
use Event as _EventInDocs;

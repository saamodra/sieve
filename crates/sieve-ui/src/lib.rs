//! HTTP API, live WebSocket feed, and embedded web UI.

mod api;
mod assets;
mod auth;
mod model;
mod store;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::{Router, middleware};
use sieve_core::{FlowSender, UiConfig};
use tokio::{net::TcpListener, sync::broadcast};
use tokio_util::sync::CancellationToken;

use api::{ApiState, WsEvent};
use model::FlowSummary;
use store::{DEFAULT_MAX_FLOWS, FlowStore};

const EVENT_CAPACITY: usize = 4096;

/// Serves the API and UI on `config.listen` until `shutdown` is cancelled.
/// `extra_routes` (device setup pages) are merged into the app.
///
/// When `config.listen` is not a loopback address a non-empty `config.token`
/// is required, and every route except `/setup/*` and `/proxy.pac` demands it.
pub async fn serve(
    config: UiConfig,
    flows: FlowSender,
    extra_routes: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    let app = build(&config, &flows, extra_routes, &shutdown)?;
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the web UI to {}", config.listen))?;
    tracing::info!("web UI listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("web UI server failed")
}

/// Builds the app and starts feeding the store from `flows`.
fn build(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
) -> Result<Router> {
    let token = config.token.as_deref().filter(|t| !t.is_empty());
    let protect = !config.listen.ip().is_loopback();
    if protect && token.is_none() {
        bail!(
            "the web UI is bound to {}, which is not a loopback address, but no ui.token is \
             configured; set `token` under [ui] in config.toml or bind to 127.0.0.1",
            config.listen
        );
    }

    let store = Arc::new(FlowStore::new(DEFAULT_MAX_FLOWS));
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    tokio::spawn(ingest(
        flows.subscribe(),
        store.clone(),
        events.clone(),
        shutdown.clone(),
    ));

    let mut app = api::router(ApiState { store, events })
        .merge(extra_routes)
        .fallback(assets::serve_asset);
    if let (true, Some(token)) = (protect, token) {
        app = app.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            auth::require_token,
        ));
    }
    Ok(app)
}

/// Copies flows from the proxy channel into the store, then announces them.
async fn ingest(
    mut rx: sieve_core::flow::FlowReceiver,
    store: Arc<FlowStore>,
    events: broadcast::Sender<WsEvent>,
    shutdown: CancellationToken,
) {
    loop {
        let flow = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = rx.recv() => match r {
                Ok(flow) => flow,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("web UI fell behind the proxy and dropped {n} flow updates");
                    let _ = events.send(WsEvent::Resync);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        let summary = Box::new(FlowSummary::from(flow.as_ref()));
        store.upsert(flow);
        // No WebSocket subscribers is fine.
        let _ = events.send(WsEvent::Flow { flow: summary });
    }
}

#[cfg(test)]
mod tests;

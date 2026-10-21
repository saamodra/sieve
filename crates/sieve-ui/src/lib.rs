//! HTTP API, live WebSocket feed, and embedded web UI.

mod api;
mod assets;
mod auth;
mod diff;
mod har_import;
mod model;
mod store;

use std::sync::{Arc, atomic::AtomicU64};

use anyhow::{Context, Result, bail};
use axum::{Router, middleware};
use sieve_core::{
    FlowSender, UiConfig,
    config::DEFAULT_MAX_BODY_BYTES,
    proxy::replay::Replayer,
    rules::{RuleEngine, breakpoint::BreakpointEvent},
};
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
/// On loopback every route instead refuses a Host or Origin other than
/// `localhost`, `127.0.0.1` or `[::1]` on the bound port.
pub async fn serve(
    config: UiConfig,
    flows: FlowSender,
    extra_routes: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    serve_with_rules(config, flows, extra_routes, shutdown, None).await
}

/// Like [`serve`], and with `rules` the UI can edit rules and answer
/// breakpoints.
pub async fn serve_with_rules(
    config: UiConfig,
    flows: FlowSender,
    extra_routes: Router,
    shutdown: CancellationToken,
    rules: Option<Arc<RuleEngine>>,
) -> Result<()> {
    let replayer = Replayer::new(&[], DEFAULT_MAX_BODY_BYTES)?;
    serve_with_replayer(config, flows, extra_routes, shutdown, rules, replayer).await
}

/// Like [`serve_with_rules`], replaying requests with `replayer`, so replays
/// trust the proxy's extra upstream roots and honour its body limit.
pub async fn serve_with_replayer(
    config: UiConfig,
    flows: FlowSender,
    extra_routes: Router,
    shutdown: CancellationToken,
    rules: Option<Arc<RuleEngine>>,
    replayer: Replayer,
) -> Result<()> {
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the web UI to {}", config.listen))?;
    // The Host and Origin checks need the real port when the config says 0.
    let config = UiConfig {
        listen: listener.local_addr()?,
        ..config
    };
    let app = build_with_replayer(&config, &flows, extra_routes, &shutdown, rules, replayer)?;
    tracing::info!("web UI listening on http://{}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("web UI server failed")
}

/// Builds the app and starts feeding the store from `flows`.
#[cfg(test)]
fn build(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
) -> Result<Router> {
    build_with_rules(config, flows, extra_routes, shutdown, None)
}

#[cfg(test)]
fn build_with_rules(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
    rules: Option<Arc<RuleEngine>>,
) -> Result<Router> {
    let replayer = Replayer::new(&[], DEFAULT_MAX_BODY_BYTES)?;
    build_with_replayer(config, flows, extra_routes, shutdown, rules, replayer)
}

fn build_with_replayer(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
    rules: Option<Arc<RuleEngine>>,
    replayer: Replayer,
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

    if let Some(engine) = &rules {
        tokio::spawn(forward_breakpoints(
            engine.breakpoints().subscribe(),
            events.clone(),
            shutdown.clone(),
        ));
    }

    let mut app = api::router(ApiState {
        store,
        events,
        rules,
        replayer,
        flows: flows.clone(),
        next_id: Arc::new(AtomicU64::new(api::SYNTHETIC_ID_BASE)),
    })
    .merge(extra_routes)
    .fallback(assets::serve_asset);
    if let (true, Some(token)) = (protect, token) {
        app = app.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            auth::require_token,
        ));
    } else {
        app = app.layer(middleware::from_fn_with_state(
            config.listen.port(),
            auth::require_local,
        ));
    }
    Ok(app)
}

/// Relays breakpoint events to WebSocket subscribers.
async fn forward_breakpoints(
    mut rx: broadcast::Receiver<BreakpointEvent>,
    events: broadcast::Sender<WsEvent>,
    shutdown: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = rx.recv() => match r {
                Ok(BreakpointEvent::Held { request }) => WsEvent::Breakpoint { request },
                Ok(BreakpointEvent::Released { id, outcome }) => {
                    WsEvent::BreakpointReleased { id, outcome }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => WsEvent::Resync,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        let _ = events.send(event);
    }
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
mod rules_tests;
#[cfg(test)]
mod tests;

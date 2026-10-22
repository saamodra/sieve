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

/// What [`serve_with_options`] adds to the plain UI.
#[derive(Default)]
pub struct ServeOptions {
    /// With rules the UI can edit them and answer breakpoints.
    pub rules: Option<Arc<RuleEngine>>,
    /// Replays requests, so replays trust the proxy's extra upstream roots and
    /// honour its body limit. Defaults to a replayer with neither.
    pub replayer: Option<Replayer>,
}

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
    serve_with_options(
        config,
        flows,
        extra_routes,
        shutdown,
        ServeOptions::default(),
    )
    .await
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
    let options = ServeOptions {
        rules,
        ..ServeOptions::default()
    };
    serve_with_options(config, flows, extra_routes, shutdown, options).await
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
    let options = ServeOptions {
        rules,
        replayer: Some(replayer),
    };
    serve_with_options(config, flows, extra_routes, shutdown, options).await
}

/// The single implementation behind [`serve`] and its variants.
pub async fn serve_with_options(
    config: UiConfig,
    flows: FlowSender,
    extra_routes: Router,
    shutdown: CancellationToken,
    options: ServeOptions,
) -> Result<()> {
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the web UI to {}", config.listen))?;
    // The Host and Origin checks need the real port when the config says 0.
    let config = UiConfig {
        listen: listener.local_addr()?,
        ..config
    };
    let app = build_with_options(&config, &flows, extra_routes, &shutdown, options)?;
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
    build_with_options(
        config,
        flows,
        extra_routes,
        shutdown,
        ServeOptions::default(),
    )
}

#[cfg(test)]
fn build_with_rules(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
    rules: Option<Arc<RuleEngine>>,
) -> Result<Router> {
    let options = ServeOptions {
        rules,
        ..ServeOptions::default()
    };
    build_with_options(config, flows, extra_routes, shutdown, options)
}

#[cfg(test)]
fn build_with_replayer(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
    rules: Option<Arc<RuleEngine>>,
    replayer: Replayer,
) -> Result<Router> {
    let options = ServeOptions {
        rules,
        replayer: Some(replayer),
    };
    build_with_options(config, flows, extra_routes, shutdown, options)
}

fn build_with_options(
    config: &UiConfig,
    flows: &FlowSender,
    extra_routes: Router,
    shutdown: &CancellationToken,
    options: ServeOptions,
) -> Result<Router> {
    let ServeOptions { rules, replayer } = options;
    let replayer = match replayer {
        Some(replayer) => replayer,
        None => Replayer::new(&[], DEFAULT_MAX_BODY_BYTES)?,
    };
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
                    // The skipped messages may include a flow's final version,
                    // which would leave it pending forever. Make that visible,
                    // then have clients re-read the store.
                    let marked = store.mark_lost_pending();
                    if !marked.is_empty() {
                        tracing::warn!("{} pending flows marked as lost", marked.len());
                    }
                    let _ = events.send(WsEvent::Resync);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        // Keep the per-message work minimal: the channel is bounded, so every
        // microsecond spent here is headroom against losing a final update.
        let summary =
            (events.receiver_count() > 0).then(|| Box::new(FlowSummary::from(flow.as_ref())));
        store.upsert(flow);
        if let Some(flow) = summary {
            let _ = events.send(WsEvent::Flow { flow });
        }
    }
}

#[cfg(test)]
mod rules_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod ingest_tests {
    use std::time::Duration;

    use sieve_core::flow;

    use super::*;
    use crate::store::{LOST_UPDATE_ERROR, tests::flow as test_flow};

    /// Waits until `done` holds, so the tests do not depend on how fast the
    /// ingest task is scheduled on a loaded machine.
    async fn wait_for(mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ingest did not catch up within 10s");
    }

    fn pending_ids(store: &FlowStore) -> Vec<u64> {
        let (_, page) = store.list(&Default::default(), 0, usize::MAX);
        page.iter()
            .filter(|s| s.status.is_none() && s.error.is_none())
            .map(|s| s.id)
            .collect()
    }

    /// Spawns `ingest` on a fresh store; the task cannot run until the test
    /// yields, so everything sent before then is a burst.
    fn start(tx: &FlowSender, shutdown: &CancellationToken) -> Arc<FlowStore> {
        let store = Arc::new(FlowStore::new(DEFAULT_MAX_FLOWS));
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        tokio::spawn(ingest(
            tx.subscribe(),
            store.clone(),
            events,
            shutdown.clone(),
        ));
        store
    }

    #[tokio::test]
    async fn burst_larger_than_old_capacity_keeps_every_final_version() {
        // Two messages per flow, comfortably more than the old 4096 slots.
        const FLOWS: u64 = 5_000;
        let tx = flow::channel();
        let shutdown = CancellationToken::new();
        let store = start(&tx, &shutdown);
        for id in 1..=FLOWS {
            tx.send(test_flow(id, None)).unwrap();
            tx.send(test_flow(id, Some(200))).unwrap();
        }
        wait_for(|| store.get(FLOWS).is_some_and(|f| f.response.is_some())).await;
        let (total, _) = store.list(&Default::default(), 0, 0);
        assert_eq!(total, FLOWS as usize);
        assert!(pending_ids(&store).is_empty());
        assert!((1..=FLOWS).all(|id| store.get(id).unwrap().response.is_some()));
        assert!((1..=FLOWS).all(|id| store.get(id).unwrap().error.is_none()));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn lagging_feed_does_not_leave_flows_pending_forever() {
        // A channel small enough to overflow, standing in for a burst bigger
        // than any capacity.
        let (tx, _) = broadcast::channel(4);
        let shutdown = CancellationToken::new();
        let store = start(&tx, &shutdown);
        for id in 1..=4 {
            tx.send(test_flow(id, None)).unwrap();
        }
        wait_for(|| pending_ids(&store).len() == 4).await;
        assert_eq!(pending_ids(&store), [1, 2, 3, 4]);

        // The final versions are overwritten before ingest sees them.
        for id in 1..=4 {
            tx.send(test_flow(id, Some(200))).unwrap();
        }
        for id in 100..104 {
            tx.send(test_flow(id, Some(200))).unwrap();
        }
        wait_for(|| store.get(103).is_some()).await;

        assert!(pending_ids(&store).is_empty());
        for id in 1..=4 {
            let flow = store.get(id).unwrap();
            assert_eq!(flow.error.as_deref(), Some(LOST_UPDATE_ERROR));
        }
        assert!((100..104).all(|id| store.get(id).unwrap().error.is_none()));
        shutdown.cancel();
    }
}

//! HTTP API, live WebSocket feed, and embedded web UI. Implemented in phase 3.

use anyhow::Result;
use axum::Router;
use sieve_core::{FlowSender, UiConfig};
use tokio_util::sync::CancellationToken;

/// Serves the API and UI on `config.listen` until `shutdown` is cancelled.
/// `extra_routes` (device setup pages) are merged into the app.
pub async fn serve(
    _config: UiConfig,
    _flows: FlowSender,
    _extra_routes: Router,
    shutdown: CancellationToken,
) -> Result<()> {
    tracing::warn!("web UI is not implemented yet (phase 3)");
    shutdown.cancelled().await;
    Ok(())
}

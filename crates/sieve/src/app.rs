//! Wiring shared by every subcommand: config loading and the proxy + UI session.

use std::sync::Arc;

use anyhow::Result;
use sieve_core::{CertAuthority, Config, Paths, flow, proxy};
use sieve_device::ios_device::{self, SetupContext};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct Context {
    pub paths: Paths,
    pub config: Config,
}

impl Context {
    pub fn load() -> Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let config = Config::load(&paths)?;
        Ok(Self { paths, config })
    }

    pub fn load_ca(&self) -> Result<Arc<CertAuthority>> {
        Ok(Arc::new(CertAuthority::load_or_create(&self.paths)?))
    }
}

/// Runs the proxy and the web UI until Ctrl+C or until `shutdown` is cancelled.
/// Callers that change device state run their teardown after this returns.
pub async fn run_session(
    ctx: &Context,
    ca: Arc<CertAuthority>,
    shutdown: CancellationToken,
) -> Result<()> {
    let flows = flow::channel();
    tokio::spawn(log_flows(flows.subscribe()));

    let on_ctrl_c = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("shutting down");
            on_ctrl_c.cancel();
        }
    });

    let setup_routes = ios_device::routes(SetupContext {
        ca: ca.clone(),
        proxy_addr: ctx.config.proxy.listen,
        flows: flows.clone(),
    });

    info!(
        proxy = %ctx.config.proxy.listen,
        ui = %format!("http://{}", ctx.config.ui.listen),
        "sieve running",
    );
    let result = tokio::try_join!(
        proxy::run(
            ctx.config.proxy.clone(),
            ca,
            flows.clone(),
            shutdown.clone()
        ),
        sieve_ui::serve(ctx.config.ui.clone(), flows, setup_routes, shutdown.clone()),
    );
    shutdown.cancel();
    result.map(|_| ())
}

async fn log_flows(mut rx: flow::FlowReceiver) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(f) => match (&f.response, &f.error) {
                (_, Some(err)) => warn!("{} {} -> {err}", f.request.method, f.request.url),
                (Some(res), None) => {
                    info!("{} {} -> {}", f.request.method, f.request.url, res.status)
                }
                (None, None) => {}
            },
            Err(RecvError::Lagged(n)) => warn!("flow log skipped {n} flows"),
            Err(RecvError::Closed) => break,
        }
    }
}

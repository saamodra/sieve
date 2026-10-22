//! Wiring shared by every subcommand: config loading and the proxy + UI session.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context as _, Result};
use sieve_core::{
    CertAuthority, Config, Paths, flow, proxy, rules::RuleEngine, script::ScriptEngine,
};
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

/// Fails if `addr` can't be bound, so device setup can refuse before it
/// changes anything rather than route a device to a proxy that won't start.
pub fn ensure_port_free(addr: SocketAddr) -> Result<()> {
    std::net::TcpListener::bind(addr)
        .map(drop)
        .with_context(|| format!("{addr} is not available; is another sieve already running?"))
}

/// Runs the proxy and the web UI until Ctrl+C, SIGTERM, or SIGHUP, or until
/// `shutdown` is cancelled. Callers that change device state run their
/// teardown after this returns.
pub async fn run_session(
    ctx: &Context,
    ca: Arc<CertAuthority>,
    shutdown: CancellationToken,
) -> Result<()> {
    let flows = flow::channel();
    tokio::spawn(log_flows(flows.subscribe()));

    let on_signal = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutting down");
        on_signal.cancel();
    });

    let setup_routes = ios_device::routes(SetupContext {
        ca: ca.clone(),
        proxy_addr: ctx.config.proxy.listen,
        flows: flows.clone(),
    });

    let rules = RuleEngine::new(Some(ctx.paths.rules_file()));
    if let Err(e) = rules.reload() {
        warn!("ignoring the rules file: {e:#}");
    }
    // Held for the session: dropping the watcher stops live reloads.
    let _rules_watcher = match rules.watch() {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            warn!("not watching the rules file for changes: {e:#}");
            None
        }
    };

    let scripts = ScriptEngine::for_dir(ctx.paths.scripts_dir());
    match scripts.reload() {
        Ok(0) => {}
        Ok(n) => info!(n, "loaded scripting plugins"),
        Err(e) => warn!("ignoring the scripts directory: {e:#}"),
    }
    // Held for the session, like the rules watcher. This also creates the
    // scripts directory, so scripts added to a fresh install load live.
    let _scripts_watcher = match scripts.watch() {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            warn!("not watching the scripts directory for changes: {e:#}");
            None
        }
    };
    let replayer = proxy::replay::Replayer::from_config(&ctx.config.proxy)
        .context("preparing the replay client")?;

    info!(
        proxy = %ctx.config.proxy.listen,
        ui = %format!("http://{}", ctx.config.ui.listen),
        "sieve running",
    );
    let result = tokio::try_join!(
        proxy::run_with_options(
            ctx.config.proxy.clone(),
            ca,
            flows.clone(),
            shutdown.clone(),
            proxy::RunOptions {
                rules: Some(rules.clone()),
                scripts: Some(scripts),
            },
        ),
        sieve_ui::serve_with_options(
            ctx.config.ui.clone(),
            flows,
            setup_routes,
            shutdown.clone(),
            sieve_ui::ServeOptions {
                rules: Some(rules),
                replayer: Some(replayer),
            },
        ),
    );
    shutdown.cancel();
    result.map(|_| ())
}

/// Ctrl+C, plus SIGTERM (`kill`) and SIGHUP (the terminal closing) on Unix.
/// Without the last two, sieve dies on the spot and skips device teardown,
/// leaving the Mac or the phone pointed at a proxy that is gone.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(mut term), Ok(mut hup)) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                    _ = hup.recv() => {}
                }
            }
            _ => {
                warn!("could not listen for SIGTERM/SIGHUP; only Ctrl+C stops sieve cleanly");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
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

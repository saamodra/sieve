use std::net::SocketAddr;

use anyhow::Result;
use clap::Args;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::{self, Context};

#[derive(Args)]
pub struct StartArgs {
    /// Proxy listen address [default: from config, 127.0.0.1:8080].
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Web UI listen address [default: from config, 127.0.0.1:8081].
    #[arg(long)]
    ui_listen: Option<SocketAddr>,
    /// Tunnel this host without interception. Repeatable; `*.example.com`
    /// matches every subdomain.
    #[arg(long = "passthrough", value_name = "HOST")]
    passthrough_hosts: Vec<String>,
    /// Keep at most this many bytes of each body [default: from config, 5 MiB].
    #[arg(long, value_name = "BYTES")]
    max_body_bytes: Option<usize>,
}

pub async fn run(mut ctx: Context, args: StartArgs) -> Result<()> {
    if let Some(listen) = args.listen {
        ctx.config.proxy.listen = listen;
    }
    if let Some(listen) = args.ui_listen {
        ctx.config.ui.listen = listen;
    }
    if let Some(max) = args.max_body_bytes {
        ctx.config.proxy.max_body_bytes = max;
    }
    ctx.config
        .proxy
        .passthrough_hosts
        .extend(args.passthrough_hosts);

    let ca = ctx.load_ca()?;
    let ca_path = ctx.paths.ca_cert();
    info!("CA certificate: {}", ca_path.display());
    info!(
        "try: curl -x http://{} --cacert \"{}\" https://example.com",
        ctx.config.proxy.listen,
        ca_path.display()
    );
    app::run_session(&ctx, ca, CancellationToken::new()).await
}

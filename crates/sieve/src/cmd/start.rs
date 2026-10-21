use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use clap::Args;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::{self, Context};

#[derive(Args)]
pub struct StartArgs {
    /// Proxy listen address [default: from config, 127.0.0.1:8730].
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Web UI listen address [default: from config, 127.0.0.1:8731].
    #[arg(long)]
    ui_listen: Option<SocketAddr>,
    /// Tunnel this host without interception. Repeatable; `*.example.com`
    /// matches every subdomain.
    #[arg(long = "passthrough", value_name = "HOST")]
    passthrough_hosts: Vec<String>,
    /// Keep at most this many bytes of each body [default: from config, 5 MiB].
    #[arg(long, value_name = "BYTES")]
    max_body_bytes: Option<usize>,
    /// Trust the roots in this PEM file when verifying upstream servers, on
    /// top of the built-in ones. Repeatable.
    #[arg(long = "upstream-ca", value_name = "PEM")]
    upstream_ca_files: Vec<PathBuf>,
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

    ctx.config
        .proxy
        .upstream_ca_files
        .extend(args.upstream_ca_files);

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

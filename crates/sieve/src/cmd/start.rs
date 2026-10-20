use std::net::SocketAddr;

use anyhow::Result;
use clap::Args;
use tokio_util::sync::CancellationToken;

use crate::app::{self, Context};

#[derive(Args)]
pub struct StartArgs {
    /// Proxy listen address [default: from config, 127.0.0.1:8080].
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Web UI listen address [default: from config, 127.0.0.1:8081].
    #[arg(long)]
    ui_listen: Option<SocketAddr>,
    /// Tunnel this host without interception. Repeatable.
    #[arg(long = "passthrough", value_name = "HOST")]
    passthrough_hosts: Vec<String>,
}

pub async fn run(mut ctx: Context, args: StartArgs) -> Result<()> {
    if let Some(listen) = args.listen {
        ctx.config.proxy.listen = listen;
    }
    if let Some(listen) = args.ui_listen {
        ctx.config.ui.listen = listen;
    }
    ctx.config
        .proxy
        .passthrough_hosts
        .extend(args.passthrough_hosts);

    let ca = ctx.load_ca()?;
    app::run_session(&ctx, ca, CancellationToken::new()).await
}

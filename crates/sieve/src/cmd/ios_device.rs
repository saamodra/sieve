use std::net::SocketAddr;

use anyhow::Result;
use clap::{Args, Subcommand};
use sieve_device::ios_device;
use tokio_util::sync::CancellationToken;

use crate::app::{self, Context};

#[derive(Args)]
pub struct IosDeviceArgs {
    #[command(subcommand)]
    command: IosDeviceCommand,
}

#[derive(Subcommand)]
enum IosDeviceCommand {
    /// Listen on the LAN and show a QR code for the iPhone setup page.
    Setup,
}

pub async fn run(ctx: Context, args: IosDeviceArgs) -> Result<()> {
    match args.command {
        IosDeviceCommand::Setup => setup(ctx).await,
    }
}

async fn setup(mut ctx: Context) -> Result<()> {
    let ip = ios_device::lan_ipv4()?;
    let ui_port = ctx.config.ui.listen.port();
    let proxy_port = ctx.config.proxy.listen.port();
    let token = ios_device::random_token();

    // Explicit LAN opt-in: everything on the Wi-Fi network can reach these ports.
    // Bound to the LAN address rather than 0.0.0.0, and the proxy refuses
    // loopback targets from other machines, so the Mac's loopback-only
    // services stay private.
    ctx.config.proxy.listen = SocketAddr::from((ip, proxy_port));
    ctx.config.ui.listen = SocketAddr::from((ip, ui_port));
    ctx.config.ui.token = Some(token.clone());

    let setup_url = format!("http://{ip}:{ui_port}/setup/ios");
    println!("{}", ios_device::render_qr(&setup_url)?);
    println!("Scan with the iPhone camera, or open: {setup_url}");
    println!("UI (token required for the API): http://{ip}:{ui_port}/?token={token}");
    println!("Proxy and UI are listening on your local network until you press Ctrl+C.");

    let ca = ctx.load_ca()?;
    app::run_session(&ctx, ca, CancellationToken::new()).await
}

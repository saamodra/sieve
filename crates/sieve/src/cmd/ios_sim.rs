use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use sieve_device::ios_sim::{self, SystemRunner};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::{Context, ensure_port_free, run_session};

/// Matches every process inside a Simulator runtime or a Simulator device,
/// and nothing that runs on the Mac itself.
const SIMULATOR_PATH: &str = "/CoreSimulator/";

#[derive(Args)]
pub struct IosSimArgs {
    #[command(subcommand)]
    command: IosSimCommand,
}

#[derive(Subcommand)]
enum IosSimCommand {
    /// Trust the CA in booted simulators, set the Mac proxy, and run the proxy.
    /// Only Simulator traffic is intercepted; the Mac's own apps are tunneled
    /// untouched.
    Setup {
        /// Also intercept the Mac's own apps. They fail TLS unless the sieve CA
        /// is trusted in the Mac keychain.
        #[arg(long)]
        intercept_mac: bool,
    },
    /// Restore the Mac's previous proxy settings.
    Teardown,
}

pub async fn run(ctx: Context, args: IosSimArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("ios-sim needs macOS: the iOS Simulator and `networksetup` only exist there");
    }
    match args.command {
        IosSimCommand::Setup { intercept_mac } => setup(ctx, intercept_mac).await,
        IosSimCommand::Teardown => teardown(&ctx),
    }
}

async fn setup(mut ctx: Context, intercept_mac: bool) -> Result<()> {
    ensure_port_free(ctx.config.proxy.listen)?;
    ensure_port_free(ctx.config.ui.listen)?;
    if !intercept_mac {
        ctx.config.proxy.intercept_processes = vec![SIMULATOR_PATH.to_string()];
    }
    let runner = SystemRunner;
    let sims = ios_sim::booted_simulators(&runner)?;
    if sims.is_empty() {
        bail!("no booted iOS Simulator; boot one first (e.g. `xcrun simctl boot <udid>`)");
    }
    let ca = ctx.load_ca()?;
    let ca_pem = ctx.paths.ca_cert();
    for sim in &sims {
        ios_sim::install_ca(&runner, &sim.udid, &ca_pem)?;
        info!(udid = %sim.udid, "CA installed in {}", sim.name);
    }
    if intercept_mac {
        info!("intercepting the Mac's own traffic too (--intercept-mac)");
    } else {
        info!("intercepting Simulator traffic only; the Mac's own apps are tunneled untouched");
    }
    let port = ctx.config.proxy.listen.port();
    ios_sim::run_guarded(
        &runner,
        &ctx.paths.state_dir(),
        port,
        run_session(&ctx, ca, CancellationToken::new()),
    )
    .await
}

fn teardown(ctx: &Context) -> Result<()> {
    match ios_sim::find_leftover(&ctx.paths.state_dir())? {
        Some(left) => {
            ios_sim::restore(&SystemRunner, &left.path)?;
            println!("Restored proxy settings for \"{}\".", left.state.service);
        }
        None => println!("Nothing to restore."),
    }
    Ok(())
}

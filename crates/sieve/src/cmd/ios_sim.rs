use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use sieve_device::ios_sim::{self, SystemRunner};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::{Context, run_session};

#[derive(Args)]
pub struct IosSimArgs {
    #[command(subcommand)]
    command: IosSimCommand,
}

#[derive(Subcommand)]
enum IosSimCommand {
    /// Trust the CA in booted simulators, set the Mac proxy, and run the proxy.
    Setup,
    /// Restore the Mac's previous proxy settings.
    Teardown,
}

pub async fn run(ctx: Context, args: IosSimArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("ios-sim needs macOS: the iOS Simulator and `networksetup` only exist there");
    }
    match args.command {
        IosSimCommand::Setup => setup(ctx).await,
        IosSimCommand::Teardown => teardown(&ctx),
    }
}

async fn setup(ctx: Context) -> Result<()> {
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

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::app::Context;

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

pub async fn run(_ctx: Context, args: IosSimArgs) -> Result<()> {
    match args.command {
        IosSimCommand::Setup | IosSimCommand::Teardown => bail!("not implemented yet (phase 2)"),
    }
}

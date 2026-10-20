use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::app::Context;

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

pub async fn run(_ctx: Context, args: IosDeviceArgs) -> Result<()> {
    match args.command {
        IosDeviceCommand::Setup => bail!("not implemented yet (phase 4)"),
    }
}

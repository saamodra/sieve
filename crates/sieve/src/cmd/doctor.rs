use anyhow::{Result, bail};
use clap::Args;

use crate::app::Context;

#[derive(Args)]
pub struct DoctorArgs {
    /// Restore system proxy settings saved by a setup that did not tear down.
    #[arg(long)]
    restore: bool,
}

pub async fn run(_ctx: Context, _args: DoctorArgs) -> Result<()> {
    bail!("not implemented yet (phase 2)")
}

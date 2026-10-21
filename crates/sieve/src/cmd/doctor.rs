use anyhow::Result;
use clap::Args;
use sieve_device::ios_sim::{self, SystemRunner};

use crate::app::Context;

#[derive(Args)]
pub struct DoctorArgs {
    /// Restore system proxy settings saved by a setup that did not tear down.
    #[arg(long)]
    restore: bool,
}

pub async fn run(ctx: Context, args: DoctorArgs) -> Result<()> {
    let Some(left) = ios_sim::find_leftover(&ctx.paths.state_dir())? else {
        println!("OK: no leftover proxy state.");
        return Ok(());
    };
    let s = &left.state;
    println!(
        "Leftover Mac proxy state for \"{}\" ({}): web {}:{} ({}), secure web {}:{} ({}).",
        s.service,
        left.path.display(),
        s.web.server,
        s.web.port,
        if s.web.enabled { "was on" } else { "was off" },
        s.secure_web.server,
        s.secure_web.port,
        if s.secure_web.enabled {
            "was on"
        } else {
            "was off"
        },
    );
    if args.restore {
        ios_sim::restore(&SystemRunner, &left.path)?;
        println!("Restored and removed the state file.");
    } else {
        println!("Run `sieve doctor --restore` to put the old settings back.");
    }
    Ok(())
}

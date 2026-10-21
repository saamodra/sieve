use anyhow::Result;
use clap::Args;
use sieve_device::{
    android::{Adb, mode1},
    ios_sim::{self, SystemRunner},
};

use crate::app::Context;

#[derive(Args)]
pub struct DoctorArgs {
    /// Restore proxy settings saved by a setup that did not tear down.
    #[arg(long)]
    restore: bool,
}

pub async fn run(ctx: Context, args: DoctorArgs) -> Result<()> {
    let state_dir = ctx.paths.state_dir();
    let mac = ios_sim::find_leftover(&state_dir)?;
    let android = mode1::find_leftovers(&state_dir)?;
    if mac.is_none() && android.is_empty() {
        println!("OK: no leftover proxy state.");
        return Ok(());
    }

    if let Some(left) = &mac {
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
        }
    }

    let adb = Adb::default();
    for left in &android {
        let s = &left.state;
        println!(
            "Leftover Android proxy state for device {} ({}): http_proxy was {}.",
            s.serial,
            left.path.display(),
            s.http_proxy.as_deref().unwrap_or("not set"),
        );
        if !args.restore {
            continue;
        }
        if mode1::select_device(&adb, Some(&s.serial)).await.is_err() {
            println!(
                "Skipped: device {} is not connected. Connect it and run `sieve doctor --restore` again; the state file is kept.",
                s.serial
            );
            continue;
        }
        mode1::restore_proxy(&adb, &state_dir, &s.serial).await?;
        println!("Restored and removed the state file.");
    }

    if !args.restore {
        println!("Run `sieve doctor --restore` to put the old settings back.");
    }
    Ok(())
}

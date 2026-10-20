use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use sieve_device::android::{Adb, system_ca};

use crate::app::Context;

#[derive(Args)]
pub struct AndroidArgs {
    /// Device serial, as listed by `adb devices`.
    #[arg(long, global = true)]
    serial: Option<String>,
    #[command(subcommand)]
    command: AndroidCommand,
}

#[derive(Subcommand)]
enum AndroidCommand {
    /// Route the device through sieve (mode 1, no root) and run the proxy.
    Setup,
    /// Undo `setup`: clear http_proxy and remove adb reverse.
    Teardown,
    /// Print the network_security_config.xml snippet for debug builds.
    Nsc,
    /// Inject the CA into the system store (mode 2, emulator or root).
    InjectCa,
}

pub async fn run(ctx: Context, args: AndroidArgs) -> Result<()> {
    let adb = Adb::default();
    let serial = args.serial.as_deref();
    match args.command {
        AndroidCommand::Setup | AndroidCommand::Teardown | AndroidCommand::Nsc => {
            bail!("not implemented yet (phase 2)")
        }
        AndroidCommand::InjectCa => {
            let ca = ctx.load_ca()?;
            system_ca::inject(&adb, serial, ca.cert_pem(), &ca.subject_hash_old()?).await
        }
    }
}

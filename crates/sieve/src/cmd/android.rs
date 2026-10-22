use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use sieve_device::android::{Adb, frida, mode1, system_ca, vpn};
use tokio_util::sync::CancellationToken;

use crate::app::{Context, ensure_port_free, run_session};

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
    /// Undo `setup`: restore the previous http_proxy and remove adb reverse.
    Teardown,
    /// Print the network_security_config.xml snippet for debug builds.
    Nsc,
    /// Check that the device is routed to a running sieve proxy.
    Health,
    /// Inject the CA into the system store (mode 2, emulator or root).
    InjectCa,
    /// Push and start frida-server (needs root), then spawn <package> with
    /// sieve's certificate-pinning unpinning script attached.
    Frida {
        /// Package id of the app to spawn.
        package: String,
        /// Path to a frida-server binary for the device's ABI, downloaded
        /// from https://github.com/frida/frida/releases. Its version must
        /// match the `frida` CLI installed on this machine.
        #[arg(long)]
        frida_server: PathBuf,
    },
    /// Route all TCP through sieve with the VPN companion app, and run the proxy.
    Vpn,
}

pub async fn run(ctx: Context, args: AndroidArgs) -> Result<()> {
    let adb = Adb::default();
    let serial = args.serial.as_deref();
    let port = ctx.config.proxy.listen.port();
    match args.command {
        AndroidCommand::Setup => {
            ensure_port_free(ctx.config.proxy.listen)?;
            ensure_port_free(ctx.config.ui.listen)?;
            let device = mode1::select_device(&adb, serial).await?;
            let ca = ctx.load_ca()?;
            let state_dir = ctx.paths.state_dir();
            mode1::setup(&adb, &state_dir, &device, port, ca.cert_pem()).await?;
            println!("Device {device} now proxies through 127.0.0.1:{port}.\n");
            println!("{}\n", mode1::install_steps());
            mode1::with_teardown(
                &adb,
                &state_dir,
                &device,
                port,
                run_session(&ctx, ca, CancellationToken::new()),
            )
            .await
        }
        AndroidCommand::Teardown => {
            let device = mode1::select_device(&adb, serial).await?;
            mode1::teardown(&adb, &ctx.paths.state_dir(), &device, port).await?;
            println!("Device {device}: http_proxy restored and adb reverse removed.");
            Ok(())
        }
        AndroidCommand::Nsc => {
            print!("{}", mode1::nsc_snippet());
            Ok(())
        }
        AndroidCommand::Health => {
            let device = mode1::select_device(&adb, serial).await?;
            let checks = mode1::health(&adb, &device, port).await;
            for check in &checks {
                println!("{check}");
            }
            if !mode1::all_passed(&checks) {
                bail!("health check failed");
            }
            Ok(())
        }
        AndroidCommand::InjectCa => {
            let ca = ctx.load_ca()?;
            system_ca::inject(&adb, serial, ca.cert_pem(), &ca.subject_hash_old()?).await
        }
        AndroidCommand::Frida {
            package,
            frida_server,
        } => {
            let device = mode1::select_device(&adb, serial).await?;
            let host = frida::FridaCli::default();
            // The frida session blocks until it ends; Ctrl+C or SIGTERM ends
            // it early. Either way `run_until` stops frida-server on exit.
            frida::run_until(
                &adb,
                &host,
                Some(&device),
                &package,
                &frida_server,
                shutdown_signal(),
            )
            .await
        }
        AndroidCommand::Vpn => {
            ensure_port_free(ctx.config.proxy.listen)?;
            ensure_port_free(ctx.config.ui.listen)?;
            let device = mode1::select_device(&adb, serial).await?;
            let ca = ctx.load_ca()?;
            vpn::setup(&adb, &device, port, ca.cert_pem()).await?;
            println!("Device {device} can reach the proxy at 127.0.0.1:{port}.\n");
            match vpn::launch(&adb, &device, port).await {
                Ok(()) => println!("Opened the sieve VPN app on the device.\n"),
                Err(e) => println!(
                    "Could not open the VPN app ({e:#}). Install android-vpn/ first; see its README.\n"
                ),
            }
            println!("{}\n", vpn::install_steps());
            let result = run_session(&ctx, ca, CancellationToken::new()).await;
            let cleanup = vpn::teardown(&adb, &device, port).await;
            result.and(cleanup)
        }
    }
}

/// Ctrl+C, plus SIGTERM and SIGHUP on Unix, so a `kill` or a closed terminal
/// still stops the root `frida-server` left running on the device.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

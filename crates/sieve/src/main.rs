mod app;
mod cmd;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "sieve",
    version,
    about = "HTTP/HTTPS debug proxy for Android and iOS"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy and the web UI.
    Start(cmd::start::StartArgs),
    /// Manage the root certificate authority.
    Ca(cmd::ca::CaArgs),
    /// Set up Android emulators and devices.
    Android(cmd::android::AndroidArgs),
    /// Set up booted iOS Simulators (macOS only).
    IosSim(cmd::ios_sim::IosSimArgs),
    /// Set up a physical iOS device over Wi-Fi.
    IosDevice(cmd::ios_device::IosDeviceArgs),
    /// Diagnose and repair leftover proxy settings.
    Doctor(cmd::doctor::DoctorArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SIEVE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let ctx = app::Context::load()?;
    match cli.command {
        Command::Start(args) => cmd::start::run(ctx, args).await,
        Command::Ca(args) => cmd::ca::run(ctx, args).await,
        Command::Android(args) => cmd::android::run(ctx, args).await,
        Command::IosSim(args) => cmd::ios_sim::run(ctx, args).await,
        Command::IosDevice(args) => cmd::ios_device::run(ctx, args).await,
        Command::Doctor(args) => cmd::doctor::run(ctx, args).await,
    }
}

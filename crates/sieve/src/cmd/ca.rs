use std::{
    fs,
    io::{IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use sieve_core::{CertAuthority, ca::ExportFormat};

use crate::app::Context;

#[derive(Args)]
pub struct CaArgs {
    #[command(subcommand)]
    command: CaCommand,
}

#[derive(Subcommand)]
enum CaCommand {
    /// Write the CA certificate in a device-friendly format.
    Export {
        #[arg(long, value_enum, default_value_t = Format::Pem)]
        format: Format,
        /// Output file [default: stdout].
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Print the CA's subject_hash_old (the Android cacerts file name).
    Hash,
    /// Show the CA's fingerprint, validity, and file locations.
    Info,
    /// Print where the CA files live.
    Path,
    /// Replace the CA with a new one. Devices must trust it again.
    Regenerate,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Pem,
    Der,
    Cer,
}

impl From<Format> for ExportFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Pem => Self::Pem,
            Format::Der => Self::Der,
            Format::Cer => Self::Cer,
        }
    }
}

pub async fn run(ctx: Context, args: CaArgs) -> Result<()> {
    match args.command {
        CaCommand::Export { format, out } => {
            let bytes = ctx.load_ca()?.export(format.into());
            match out {
                Some(path) => fs::write(&path, bytes)
                    .with_context(|| format!("writing {}", path.display()))?,
                None if !matches!(format, Format::Pem) && std::io::stdout().is_terminal() => {
                    bail!("refusing to write binary DER to the terminal; pass --out <FILE>")
                }
                None => std::io::stdout().write_all(&bytes)?,
            }
        }
        CaCommand::Hash => println!("{}", ctx.load_ca()?.subject_hash_old()?),
        CaCommand::Info => {
            let ca = ctx.load_ca()?;
            println!("certificate  {}", ctx.paths.ca_cert().display());
            println!("key          {}", ctx.paths.ca_key().display());
            println!(
                "valid        {} to {}",
                ca.not_before().date(),
                ca.not_after().date()
            );
            println!("sha256       {}", ca.sha256_fingerprint());
            println!("android name {}.0", ca.subject_hash_old()?);
        }
        CaCommand::Path => {
            println!("{}", ctx.paths.ca_cert().display());
            println!("{}", ctx.paths.ca_key().display());
        }
        CaCommand::Regenerate => {
            let ca = CertAuthority::regenerate(&ctx.paths)?;
            println!("new CA written to {}", ctx.paths.ca_cert().display());
            println!("sha256 {}", ca.sha256_fingerprint());
            println!("devices that trusted the old CA must trust this one instead");
        }
    }
    Ok(())
}

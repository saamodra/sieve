use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result};
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
                None => std::io::Write::write_all(&mut std::io::stdout(), &bytes)?,
            }
        }
        CaCommand::Hash => println!("{}", ctx.load_ca()?.subject_hash_old()?),
        CaCommand::Path => {
            println!("{}", ctx.paths.ca_cert().display());
            println!("{}", ctx.paths.ca_key().display());
        }
        CaCommand::Regenerate => {
            CertAuthority::regenerate(&ctx.paths)?;
            println!("new CA written to {}", ctx.paths.ca_cert().display());
        }
    }
    Ok(())
}

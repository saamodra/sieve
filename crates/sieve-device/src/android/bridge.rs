//! Thin wrapper around the `adb` executable, behind a trait so tests can mock it.

use std::{future::Future, path::PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

pub trait AndroidBridge: Send + Sync {
    /// Runs `adb [-s <serial>] <args>` and returns stdout. A non-zero exit is an
    /// error carrying stderr.
    fn adb(
        &self,
        serial: Option<&str>,
        args: &[&str],
    ) -> impl Future<Output = Result<String>> + Send;

    /// Runs `adb [-s <serial>] shell <command>`.
    fn shell(
        &self,
        serial: Option<&str>,
        command: &str,
    ) -> impl Future<Output = Result<String>> + Send {
        async move { self.adb(serial, &["shell", command]).await }
    }
}

/// The real `adb`, found on `PATH` unless `program` says otherwise.
#[derive(Debug, Clone)]
pub struct Adb {
    pub program: PathBuf,
}

impl Default for Adb {
    fn default() -> Self {
        Self {
            program: PathBuf::from("adb"),
        }
    }
}

impl AndroidBridge for Adb {
    async fn adb(&self, serial: Option<&str>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.program);
        if let Some(serial) = serial {
            cmd.args(["-s", serial]);
        }
        cmd.args(args);
        let output = cmd.output().await.with_context(|| {
            format!(
                "running {} (is it installed and on PATH?)",
                self.program.display()
            )
        })?;
        if !output.status.success() {
            bail!(
                "adb {} failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

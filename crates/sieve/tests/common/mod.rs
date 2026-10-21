//! Helpers shared by the end-to-end tests. Each test binary uses a different
//! subset, so unused items are expected.
#![allow(dead_code)]

use std::{
    fs::{self, File},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

use tempfile::TempDir;
use tokio::net::TcpStream;

/// How long a freshly started process gets to bind its ports.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `sieve start` gets to exit after SIGINT.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A `sieve` command whose config lives in `config_dir`, never in the real
/// per-user directory.
pub fn sieve_command(config_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sieve"));
    cmd.env("SIEVE_CONFIG_DIR", config_dir);
    cmd
}

/// Runs a one-shot `sieve` subcommand to completion.
pub fn run_sieve(config_dir: &Path, args: &[&str]) -> Output {
    sieve_command(config_dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running sieve")
}

/// Runs a one-shot `sieve` subcommand and asserts it succeeded.
pub fn run_sieve_ok(config_dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = run_sieve(config_dir, args);
    assert!(
        out.status.success(),
        "sieve {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A free loopback address. The port is released before returning, so another
/// process could in theory take it first; that race is acceptable for tests.
pub fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding a free port");
    listener.local_addr().expect("local address")
}

/// A running `sieve start` whose stdout and stderr are captured to files in
/// the config dir. The child is killed on drop, so a failing test never leaves
/// a proxy behind.
pub struct Daemon {
    child: Child,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

impl Daemon {
    /// Spawns `sieve start <args>` with `SIEVE_CONFIG_DIR` set to `config_dir`.
    pub fn start(config_dir: &TempDir, args: &[&str]) -> Self {
        let stdout_log = config_dir.path().join("stdout.log");
        let stderr_log = config_dir.path().join("stderr.log");
        let child = sieve_command(config_dir.path())
            .arg("start")
            .args(args)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout_log).expect("creating the stdout log"))
            .stderr(File::create(&stderr_log).expect("creating the stderr log"))
            .spawn()
            .expect("spawning sieve start");
        Self {
            child,
            stdout_log,
            stderr_log,
        }
    }

    /// Everything the process has written to stderr so far.
    pub fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }

    /// Everything the process has written to stdout so far.
    pub fn stdout(&self) -> String {
        fs::read_to_string(&self.stdout_log).unwrap_or_default()
    }

    /// Stdout followed by stderr. The process logs through tracing, which
    /// writes to stdout, so most of its log lines are here.
    pub fn output(&self) -> String {
        self.stdout() + &self.stderr()
    }

    /// Waits until `addr` accepts connections. Panics early, with the stderr
    /// output, if the process exits first.
    pub async fn wait_for_port(&mut self, addr: SocketAddr) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("polling sieve") {
                panic!(
                    "sieve exited with {status} before {addr} came up:\n{}",
                    self.output()
                );
            }
            assert!(
                Instant::now() < deadline,
                "{addr} did not come up within {STARTUP_TIMEOUT:?}:\n{}",
                self.output()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until the output contains `needle`, and panics with the output
    /// so far if it does not show up in time.
    pub async fn wait_for_output(&self, needle: &str) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while !self.output().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{needle:?} did not appear within {STARTUP_TIMEOUT:?}:\n{}",
                self.output()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Sends SIGINT, the same signal Ctrl+C delivers, and returns the exit
    /// status. Panics if the process is still running after `timeout`.
    pub fn interrupt(&mut self, timeout: Duration) -> ExitStatus {
        let sent = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .status()
            .expect("running kill");
        assert!(sent.success(), "kill -INT failed");

        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("polling sieve") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "sieve did not exit within {timeout:?} after SIGINT:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Makes rustls use the `ring` provider for clients built in this process.
/// Safe to call from every test; only the first call takes effect.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

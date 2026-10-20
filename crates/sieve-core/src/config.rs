//! On-disk locations and user configuration.

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Default cap on how much of a single body is kept in memory.
pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Per-OS directories: `~/.config/sieve/` on Linux,
/// `~/Library/Application Support/sieve/` on macOS.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
}

impl Paths {
    /// `SIEVE_CONFIG_DIR` overrides the per-OS default.
    pub fn discover() -> Result<Self> {
        if let Some(dir) = std::env::var_os("SIEVE_CONFIG_DIR") {
            return Ok(Self::at(dir));
        }
        let dirs =
            ProjectDirs::from("", "", "sieve").context("could not determine the home directory")?;
        Ok(Self::at(dirs.config_dir()))
    }

    /// Uses an explicit directory, e.g. a tempdir in tests.
    pub fn at(dir: impl AsRef<Path>) -> Self {
        Self {
            config_dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("creating {}", self.config_dir.display()))?;
        fs::create_dir_all(self.state_dir())
            .with_context(|| format!("creating {}", self.state_dir().display()))
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn ca_cert(&self) -> PathBuf {
        self.config_dir.join("ca.pem")
    }

    pub fn ca_key(&self) -> PathBuf {
        self.config_dir.join("ca.key")
    }

    /// Where device setup saves the state it must restore on teardown
    /// (e.g. the Mac's previous proxy settings).
    pub fn state_dir(&self) -> PathBuf {
        self.config_dir.join("state")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub ui: UiConfig,
}

impl Config {
    /// Reads `config.toml`, falling back to defaults when it does not exist.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    /// Hosts that are tunneled without interception (pinned or uninteresting).
    /// Exact names, or `*.example.com` for every subdomain of example.com.
    pub passthrough_hosts: Vec<String>,
    pub max_body_bytes: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            passthrough_hosts: Vec::new(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub listen: SocketAddr,
    /// Required on API requests when the UI is bound to a non-loopback address.
    pub token: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8081)),
            token: None,
        }
    }
}

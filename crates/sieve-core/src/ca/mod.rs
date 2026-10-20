//! Root CA management and per-host leaf certificates. Implemented in phase 1a.

use anyhow::{Result, bail};

use crate::config::Paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Pem,
    Der,
    /// DER with a `.cer` name, as iOS expects.
    Cer,
}

pub struct CertAuthority {
    cert_pem: String,
    cert_der: Vec<u8>,
}

impl CertAuthority {
    /// Loads `ca.pem`/`ca.key` from `paths`, generating them on first use.
    pub fn load_or_create(_paths: &Paths) -> Result<Self> {
        bail!("CA is not implemented yet (phase 1a)")
    }

    /// Replaces the CA on disk with a freshly generated one.
    pub fn regenerate(_paths: &Paths) -> Result<Self> {
        bail!("CA is not implemented yet (phase 1a)")
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub fn export(&self, format: ExportFormat) -> Vec<u8> {
        match format {
            ExportFormat::Pem => self.cert_pem.as_bytes().to_vec(),
            ExportFormat::Der | ExportFormat::Cer => self.cert_der.clone(),
        }
    }

    /// OpenSSL's `subject_hash_old`, used as the Android cacerts file name.
    pub fn subject_hash_old(&self) -> Result<String> {
        bail!("CA is not implemented yet (phase 1a)")
    }
}

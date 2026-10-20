//! rustls configuration for both sides of the proxy.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    crypto::{CryptoProvider, ring},
    pki_types::CertificateDer,
};
use tracing::debug;

use crate::ca::LeafCert;

/// Every config uses ring explicitly, so the result does not depend on which
/// rustls provider other crates in the build happen to enable.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// What the proxy presents to clients for one host. Offers h2 and HTTP/1.1;
/// the client picks.
pub(crate) fn server_config(leaf: &LeafCert) -> Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(leaf.chain.clone(), leaf.key.clone_key())
        .context("building the TLS server config")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// How the proxy verifies upstream servers: the Mozilla roots, the OS trust
/// store (for corporate and mkcert roots), and any `extra` roots.
pub(crate) fn upstream_config(extra: &[CertificateDer<'static>]) -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    for error in &native.errors {
        debug!("skipping part of the OS trust store: {error}");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    debug!("trusting {added} OS root certificates ({ignored} unparsable)");
    for cert in extra {
        roots
            .add(cert.clone())
            .context("adding an upstream root certificate")?;
    }
    Ok(ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

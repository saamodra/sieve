//! The MITM proxy. Implemented in phase 1b.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use crate::{ca::CertAuthority, config::ProxyConfig, flow::FlowSender};

/// Serves the proxy on `config.listen` until `shutdown` is cancelled,
/// publishing every exchange on `flows`.
pub async fn run(
    _config: ProxyConfig,
    _ca: Arc<CertAuthority>,
    _flows: FlowSender,
    _shutdown: CancellationToken,
) -> Result<()> {
    bail!("proxy is not implemented yet (phase 1b)")
}

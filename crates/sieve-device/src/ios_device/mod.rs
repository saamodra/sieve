//! Physical iOS device setup: `.mobileconfig` profile, PAC file, and the
//! `/setup/ios` page. Implemented in phase 4.

use std::{net::SocketAddr, sync::Arc};

use axum::Router;
use sieve_core::{CertAuthority, FlowSender};

pub struct SetupContext {
    pub ca: Arc<CertAuthority>,
    /// The proxy's listen address as devices should reach it.
    pub proxy_addr: SocketAddr,
    pub flows: FlowSender,
}

/// Routes mounted on the UI server: `/setup/ios`, `/setup/ios.mobileconfig`,
/// and `/proxy.pac`.
pub fn routes(_ctx: SetupContext) -> Router {
    Router::new()
}

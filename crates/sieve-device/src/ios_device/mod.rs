//! Physical iOS device setup: `.mobileconfig` profile, PAC file, and the
//! `/setup/ios` page.

mod page;
mod profile;

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use qrcode::{QrCode, render::unicode};
use sieve_core::{CertAuthority, Flow, FlowSender};
use tokio::sync::broadcast::error::RecvError;

pub use profile::{mobileconfig, pac};

pub struct SetupContext {
    pub ca: Arc<CertAuthority>,
    /// The proxy's listen address as devices should reach it.
    pub proxy_addr: SocketAddr,
    pub flows: FlowSender,
}

#[derive(Clone)]
struct State_ {
    ca_der: Arc<Vec<u8>>,
    proxy_addr: SocketAddr,
    verified: Arc<AtomicBool>,
}

/// Routes mounted on the UI server: `/setup/ios`, `/setup/ios.mobileconfig`,
/// `/setup/ios/status`, and `/proxy.pac`.
///
/// Must be called inside a Tokio runtime: it spawns the task that watches
/// `ctx.flows` for the first successful HTTPS request from the device.
pub fn routes(ctx: SetupContext) -> Router {
    build(ctx.ca.cert_der().to_vec(), ctx.proxy_addr, &ctx.flows)
}

fn build(ca_der: Vec<u8>, proxy_addr: SocketAddr, flows: &FlowSender) -> Router {
    let verified = Arc::new(AtomicBool::new(false));
    tokio::spawn(watch_flows(flows.clone(), verified.clone()));
    let state = State_ {
        ca_der: Arc::new(ca_der),
        proxy_addr,
        verified,
    };
    Router::new()
        .route("/setup/ios", get(setup_page))
        .route("/setup/ios.mobileconfig", get(profile_download))
        .route("/setup/ios/status", get(status))
        .route("/proxy.pac", get(pac_file))
        .with_state(state)
}

/// A flow proves the device works when it is an HTTPS exchange from a remote
/// client that did not fail (e.g. the CA is trusted).
fn proves_setup(flow: &Flow) -> bool {
    flow.error.is_none()
        && flow.request.url.starts_with("https://")
        && flow.client.is_some_and(|c| !c.ip().is_loopback())
}

async fn watch_flows(flows: FlowSender, verified: Arc<AtomicBool>) {
    let mut rx = flows.subscribe();
    drop(flows);
    loop {
        match rx.recv().await {
            Ok(flow) => {
                if proves_setup(&flow) {
                    verified.store(true, Ordering::Relaxed);
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

async fn setup_page(headers: HeaderMap) -> Html<String> {
    Html(page::render(&request_host(&headers)))
}

async fn profile_download(State(s): State<State_>) -> Response {
    let mut res = mobileconfig(&s.ca_der).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-apple-aspen-config"),
    );
    res.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"sieve.mobileconfig\""),
    );
    res
}

async fn pac_file(State(s): State<State_>) -> Response {
    let body = match advertised_addr(s.proxy_addr) {
        Ok(addr) => pac(addr),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    (
        [(header::CONTENT_TYPE, "application/x-ns-proxy-autoconfig")],
        body,
    )
        .into_response()
}

async fn status(State(s): State<State_>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "ok": s.verified.load(Ordering::Relaxed) })),
    )
}

/// The `Host` the device used to reach us, so the page links work on whatever
/// address the phone sees. Anything that is not a plain host[:port] is dropped.
fn request_host(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        })
        .unwrap_or("")
        .to_string()
}

/// The address a device should use for the proxy: the listen address, with an
/// unspecified IP replaced by the Mac's LAN IPv4.
fn advertised_addr(proxy_addr: SocketAddr) -> Result<SocketAddr> {
    if proxy_addr.ip().is_unspecified() {
        Ok(SocketAddr::new(lan_ipv4()?.into(), proxy_addr.port()))
    } else {
        Ok(proxy_addr)
    }
}

/// The Mac's Wi-Fi/LAN IPv4 address.
pub fn lan_ipv4() -> Result<Ipv4Addr> {
    if let Ok(IpAddr::V4(ip)) = local_ip_address::local_ip()
        && !ip.is_loopback()
    {
        return Ok(ip);
    }
    if let Ok(ifs) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in ifs {
            if let IpAddr::V4(ip) = ip
                && !ip.is_loopback()
                && !ip.is_link_local()
            {
                return Ok(ip);
            }
        }
    }
    bail!("could not detect the LAN IPv4 address; is the Mac connected to Wi-Fi?")
}

/// A random token for the UI's `/api`, 32 hex characters.
pub fn random_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Renders `url` as a terminal QR code using unicode half blocks.
pub fn render_qr(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes())?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

#[cfg(test)]
mod tests;

//! The MITM proxy: plain HTTP proxying, CONNECT tunnels with TLS interception,
//! WebSocket upgrades, and per-host passthrough.
//!
//! Built on hyper and tokio-rustls directly rather than hudsucker, because
//! hudsucker only logs a failed client TLS handshake, and sieve has to report
//! it as a flow so a pinned or untrusting client shows up in the UI.

mod capture;
mod decode;
mod guard;
mod io;
mod peer;
pub mod replay;
mod rules;
mod tls;
mod tunnel;

use std::{
    convert::Infallible,
    error::Error as StdError,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header,
    uri::{Authority, Scheme},
};
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::{net::TcpListener, sync::broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use self::capture::{CaptureBody, Exchange, RequestHead, Side};
use crate::{
    ca::CertAuthority,
    config::ProxyConfig,
    flow::{FlowId, FlowKind, FlowSender},
    rules::{Plan, RuleEngine},
    script::ScriptEngine,
};

pub(crate) type BoxError = Box<dyn StdError + Send + Sync>;
type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;
type UpstreamClient = Client<HttpsConnector<HttpConnector>, ProxyBody>;

/// What [`run_with_options`] applies on top of the plain proxy.
#[derive(Default)]
pub struct RunOptions {
    /// Rewrites, mocks and breakpoints for intercepted requests.
    pub rules: Option<Arc<RuleEngine>>,
    /// Rhai plugins (phase 5e), run on requests that no declarative rule
    /// already answered.
    pub scripts: Option<Arc<ScriptEngine>>,
}

/// Serves the proxy on `config.listen` until `shutdown` is cancelled,
/// publishing every exchange on `flows`.
pub async fn run(
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    shutdown: CancellationToken,
) -> Result<()> {
    run_with_options(config, ca, flows, shutdown, RunOptions::default()).await
}

/// Like [`run`], applying `rules` to the requests that pass through.
pub async fn run_with_rules(
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    shutdown: CancellationToken,
    rules: Option<Arc<RuleEngine>>,
) -> Result<()> {
    let options = RunOptions {
        rules,
        ..RunOptions::default()
    };
    run_with_options(config, ca, flows, shutdown, options).await
}

/// Like [`run_with_rules`], also running `scripts` (rhai plugins, phase 5e)
/// on requests that no declarative rule already answered.
pub async fn run_with_rules_and_scripts(
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    shutdown: CancellationToken,
    rules: Option<Arc<RuleEngine>>,
    scripts: Option<Arc<ScriptEngine>>,
) -> Result<()> {
    let options = RunOptions { rules, scripts };
    run_with_options(config, ca, flows, shutdown, options).await
}

/// The single implementation behind [`run`] and its variants: binds
/// `config.listen`, trusts the configured upstream CA files, and serves.
pub async fn run_with_options(
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    shutdown: CancellationToken,
    options: RunOptions,
) -> Result<()> {
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the proxy to {}", config.listen))?;
    let mut proxy = Proxy::new(config, ca, flows);
    proxy.rules = options.rules;
    proxy.scripts = options.scripts;
    for path in proxy.config.upstream_ca_files.clone() {
        for cert in load_pem_certs(&path)? {
            proxy = proxy.trust_upstream_root(cert.to_vec());
        }
    }
    proxy.serve(listener, shutdown).await
}

/// Reads every certificate from a PEM file, failing if there is none.
fn load_pem_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .with_context(|| format!("reading upstream CA file {}", path.display()))?;
    anyhow::ensure!(
        !certs.is_empty(),
        "upstream CA file {} contains no certificates",
        path.display()
    );
    Ok(certs)
}

/// A configured proxy that is not serving yet. [`run`] covers the usual case;
/// this is for callers that bind their own listener or need to trust extra
/// upstream roots, such as tests against a local TLS server.
pub struct Proxy {
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    upstream_roots: Vec<CertificateDer<'static>>,
    rules: Option<Arc<RuleEngine>>,
    scripts: Option<Arc<ScriptEngine>>,
}

impl Proxy {
    pub fn new(config: ProxyConfig, ca: Arc<CertAuthority>, flows: FlowSender) -> Self {
        Self {
            config,
            ca,
            flows,
            upstream_roots: Vec::new(),
            rules: None,
            scripts: None,
        }
    }

    /// Applies `rules` (rewrites, mocks, breakpoints) to intercepted requests.
    pub fn with_rules(mut self, rules: Arc<RuleEngine>) -> Self {
        self.rules = Some(rules);
        self
    }

    /// Runs `scripts` (rhai plugins) on requests that no declarative rule
    /// already answered.
    pub fn with_scripts(mut self, scripts: Arc<ScriptEngine>) -> Self {
        self.scripts = Some(scripts);
        self
    }

    /// Trusts `cert_der` as a root when verifying upstream servers, on top of
    /// the Mozilla roots and the OS trust store.
    pub fn trust_upstream_root(mut self, cert_der: impl Into<Vec<u8>>) -> Self {
        self.upstream_roots
            .push(CertificateDer::from(cert_der.into()));
        self
    }

    /// Accepts connections on `listener` until `shutdown` is cancelled.
    pub async fn serve(self, listener: TcpListener, shutdown: CancellationToken) -> Result<()> {
        let upstream = tls::upstream_config(&self.upstream_roots)?;
        let shared = Arc::new(Shared {
            client: upstream_client(upstream.clone(), true),
            upgrade_client: upstream_client(upstream, false),
            config: self.config,
            ca: self.ca,
            flows: self.flows,
            rules: self.rules,
            scripts: self.scripts,
            muted: broadcast::channel(1).0,
            next_id: AtomicU64::new(1),
            shutdown: shutdown.clone(),
        });
        info!("proxy listening on {}", listener.local_addr()?);

        loop {
            let (stream, peer) = tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        // Usually out of file descriptors; back off instead of spinning.
                        warn!("accepting a proxy connection failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let _ = stream.set_nodelay(true);
            let shared = shared.clone();
            tokio::spawn(async move {
                let quiet = !shared.intercepts(peer).await;
                let conn = Conn {
                    shared,
                    client: peer,
                    tunnel: None,
                    quiet,
                };
                serve_http(conn, TokioIo::new(stream)).await;
            });
        }
    }
}

fn upstream_client(tls: rustls::ClientConfig, http2: bool) -> UpstreamClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(30)));
    let builder = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1();
    // WebSocket upgrades need HTTP/1.1, so their client does not offer h2.
    let connector = if http2 {
        builder.enable_http2().wrap_connector(http)
    } else {
        builder.wrap_connector(http)
    };
    Client::builder(TokioExecutor::new()).build(connector)
}

struct Shared {
    config: ProxyConfig,
    ca: Arc<CertAuthority>,
    flows: FlowSender,
    rules: Option<Arc<RuleEngine>>,
    scripts: Option<Arc<ScriptEngine>>,
    /// Has no receivers; flows from quiet connections go here and vanish.
    muted: FlowSender,
    client: UpstreamClient,
    upgrade_client: UpstreamClient,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

impl Shared {
    fn exchange(
        &self,
        client: SocketAddr,
        quiet: bool,
        kind: FlowKind,
        head: RequestHead<'_>,
    ) -> Arc<Exchange> {
        let id: FlowId = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (flows, max_body) = if quiet {
            (self.muted.clone(), 0)
        } else {
            (self.flows.clone(), self.config.max_body_bytes)
        };
        Exchange::new(id, client, kind, head, flows, max_body)
    }

    /// Whether to intercept and record a connection from `client`; see
    /// [`ProxyConfig::intercept_processes`].
    async fn intercepts(&self, client: SocketAddr) -> bool {
        let wanted = &self.config.intercept_processes;
        if wanted.is_empty() || !client.ip().is_loopback() {
            return true;
        }
        match peer::client_process_path(client).await {
            Some(path) => {
                let hit = wanted.iter().any(|w| path.contains(w.as_str()));
                debug!(%client, path, intercept = hit, "matched client process");
                hit
            }
            None => {
                debug!(%client, "could not tell which process connected; tunneling it untouched");
                false
            }
        }
    }

    fn is_passthrough(&self, host: &str) -> bool {
        is_passthrough(&self.config.passthrough_hosts, host)
    }
}

/// An intercepted CONNECT tunnel: requests inside it carry only a path, so the
/// target comes from the tunnel.
struct Tunnel {
    scheme: Scheme,
    authority: Authority,
    /// The CONNECT target was already checked by [`guard`], and requests go
    /// to it; false when the SNI name replaced it, which has to be checked.
    vetted: bool,
    requests: AtomicUsize,
}

/// One client connection, either to the proxy port or inside a tunnel.
#[derive(Clone)]
struct Conn {
    shared: Arc<Shared>,
    client: SocketAddr,
    tunnel: Option<Arc<Tunnel>>,
    /// Excluded by `intercept_processes`: tunnel CONNECTs untouched and
    /// record nothing.
    quiet: bool,
}

/// Serves HTTP/1.1 or h2 on `io`. Boxed because a CONNECT handled here serves
/// the tunnel's inner connection with this function again.
fn serve_http<I>(conn: Conn, io: I) -> Pin<Box<dyn Future<Output = ()> + Send>>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    Box::pin(async move {
        let shutdown = conn.shared.shutdown.clone();
        let client = conn.client;
        let service = service_fn(move |req| conn.clone().handle(req));
        let builder = auto::Builder::new(TokioExecutor::new());
        let connection = builder.serve_connection_with_upgrades(io, service);
        tokio::select! {
            result = connection => {
                if let Err(e) = result {
                    debug!("connection from {client} ended: {e}");
                }
            }
            () = shutdown.cancelled() => {}
        }
    })
}

impl Conn {
    async fn handle(self, req: Request<Incoming>) -> Result<Response<ProxyBody>, Infallible> {
        if let Some(tunnel) = &self.tunnel {
            tunnel.requests.fetch_add(1, Ordering::Relaxed);
        }
        if req.method() == Method::CONNECT {
            return Ok(match self.tunnel {
                None => self.connect(req).await,
                Some(_) => {
                    text_response(StatusCode::BAD_REQUEST, "nested CONNECT is not supported")
                }
            });
        }
        Ok(match self.target(&req) {
            Some(uri) => {
                if let Err(message) = self.guard(&uri).await {
                    return Ok(self.refuse(&req, &uri, message));
                }
                if is_websocket_upgrade(&req) {
                    self.upgrade(req, uri).await
                } else {
                    self.forward(req, uri).await
                }
            }
            None => text_response(
                StatusCode::BAD_REQUEST,
                "sieve is a proxy: configure it as the HTTP(S) proxy instead of requesting it directly",
            ),
        })
    }

    /// The absolute URI to send upstream.
    fn target(&self, req: &Request<Incoming>) -> Option<Uri> {
        match &self.tunnel {
            Some(tunnel) => {
                let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
                Uri::builder()
                    .scheme(tunnel.scheme.clone())
                    .authority(tunnel.authority.clone())
                    .path_and_query(path)
                    .build()
                    .ok()
            }
            None => (req.uri().scheme().is_some() && req.uri().authority().is_some())
                .then(|| req.uri().clone()),
        }
    }

    /// Refuses loopback targets for clients that are not on this machine;
    /// see [`guard`]. Requests inside a tunnel whose target was vetted at
    /// CONNECT are not checked again.
    async fn guard(&self, uri: &Uri) -> Result<(), String> {
        if self.tunnel.as_ref().is_some_and(|t| t.vetted) {
            return Ok(());
        }
        let default_port = if uri.scheme() == Some(&Scheme::HTTPS) {
            443
        } else {
            80
        };
        let port = uri.port_u16().unwrap_or(default_port);
        guard::check(
            &self.shared.config,
            self.client,
            uri.host().unwrap_or_default(),
            port,
        )
        .await
    }

    /// Answers a request that [`Conn::guard`] refused with a `403`, and
    /// records it as a flow with an error.
    fn refuse(&self, req: &Request<Incoming>, uri: &Uri, message: String) -> Response<ProxyBody> {
        let (kind, url) = if is_websocket_upgrade(req) {
            (FlowKind::WebSocket, websocket_url(uri))
        } else {
            (FlowKind::Http, uri.to_string())
        };
        let exchange = self.exchange(kind, req, url);
        exchange.emit_start();
        let res = text_response(StatusCode::FORBIDDEN, &message);
        exchange.set_response(StatusCode::FORBIDDEN.as_u16(), req.version(), res.headers());
        exchange.finish(None, Some(format!("refused: {message}")));
        res
    }

    fn exchange(&self, kind: FlowKind, req: &Request<Incoming>, url: String) -> Arc<Exchange> {
        self.shared.exchange(
            self.client,
            self.quiet,
            kind,
            RequestHead {
                method: req.method().as_str(),
                url,
                version: req.version(),
                headers: req.headers(),
            },
        )
    }

    async fn forward(&self, req: Request<Incoming>, uri: Uri) -> Response<ProxyBody> {
        let exchange = self.exchange(FlowKind::Http, &req, uri.to_string());
        exchange.emit_start();
        let client_version = req.version();
        let plan = rules::plan_for(self, &req, &uri);

        let (mut parts, body) = req.into_parts();
        parts.uri = uri;
        // The upstream client picks HTTP/1.1 or h2 from ALPN, whatever the client spoke.
        parts.version = Version::HTTP_11;
        strip_hop_by_hop(&mut parts.headers, false);
        parts.headers.remove(header::HOST);
        if let Some(plan) = plan {
            return rules::forward(self, parts, body, exchange, client_version, plan).await;
        }
        // No declarative rule matched; scripting plugins get a turn instead.
        // Like rules, they leave quiet connections alone.
        if !self.quiet
            && let Some(scripts) = self.shared.scripts.clone().filter(|s| !s.is_empty())
        {
            return rules::forward_with_scripts(
                self,
                parts,
                body,
                exchange,
                client_version,
                scripts,
            )
            .await;
        }
        let body = CaptureBody::new(body, Side::Request, exchange.clone()).boxed_unsync();

        match self
            .shared
            .client
            .request(Request::from_parts(parts, body))
            .await
        {
            Ok(res) => respond(res, exchange, client_version, None),
            Err(e) => upstream_failed(&exchange, &e),
        }
    }

    /// Relays a WebSocket handshake and, once upgraded, the connection.
    ///
    /// Rules apply to the handshake: block and mock answer it without an
    /// upgrade, and request header rules and map_remote change what is sent.
    /// Response edits, delays and breakpoints are not applied: the handshake
    /// has no body worth editing, and its answer has to stay a valid `101`.
    async fn upgrade(&self, mut req: Request<Incoming>, uri: Uri) -> Response<ProxyBody> {
        let exchange = self.exchange(FlowKind::WebSocket, &req, websocket_url(&uri));
        exchange.emit_start();
        let client_version = req.version();
        let plan = rules::plan_for(self, &req, &uri);
        if let Some(plan) = plan.as_ref().filter(|p| p.block || p.mock.is_some()) {
            // Read the request so the flow records it.
            let _ = CaptureBody::new(req.into_body(), Side::Request, exchange.clone())
                .collect()
                .await;
            return rules::synthetic(plan, exchange, client_version).await;
        }
        let client_upgrade = hyper::upgrade::on(&mut req);

        let (mut parts, body) = req.into_parts();
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        strip_hop_by_hop(&mut parts.headers, true);
        parts.headers.remove(header::HOST);
        if let Some(plan) = &plan
            && edit_handshake(&mut parts, plan)
        {
            exchange.update_request(
                parts.method.as_str(),
                websocket_url(&parts.uri),
                &parts.headers,
            );
        }
        let body = CaptureBody::new(body, Side::Request, exchange.clone()).boxed_unsync();

        let request = Request::from_parts(parts, body);
        let mut res = match self.shared.upgrade_client.request(request).await {
            Ok(res) => res,
            Err(e) => return upstream_failed(&exchange, &e),
        };
        if res.status() != StatusCode::SWITCHING_PROTOCOLS {
            return respond(res, exchange, client_version, None);
        }

        // The flow covers the handshake; messages are relayed but not recorded.
        exchange.set_response(res.status().as_u16(), res.version(), res.headers());
        exchange.finish(None, None);
        let server_upgrade = hyper::upgrade::on(&mut res);
        let shutdown = self.shared.shutdown.clone();
        tokio::spawn(async move {
            let (client, server) = match tokio::try_join!(client_upgrade, server_upgrade) {
                Ok(pair) => pair,
                Err(e) => {
                    debug!("WebSocket upgrade failed: {e}");
                    return;
                }
            };
            let (mut client, mut server) = (TokioIo::new(client), TokioIo::new(server));
            tokio::select! {
                result = tokio::io::copy_bidirectional(&mut client, &mut server) => {
                    if let Err(e) = result {
                        debug!("WebSocket relay ended: {e}");
                    }
                }
                () = shutdown.cancelled() => {}
            }
        });

        let (mut parts, _) = res.into_parts();
        strip_hop_by_hop(&mut parts.headers, true);
        Response::from_parts(parts, empty_body())
    }

    async fn connect(&self, req: Request<Incoming>) -> Response<ProxyBody> {
        let Some(authority) = req.uri().authority().cloned() else {
            return text_response(StatusCode::BAD_REQUEST, "CONNECT needs a host:port target");
        };
        let head = tunnel::ConnectHead {
            authority,
            version: req.version(),
            headers: req.headers().clone(),
        };
        if let Err(message) = guard::check(
            &self.shared.config,
            self.client,
            head.authority.host(),
            head.authority.port_u16().unwrap_or(443),
        )
        .await
        {
            return tunnel::refuse(self, &head, message);
        }
        if self.quiet || self.shared.is_passthrough(head.authority.host()) {
            return tunnel::passthrough(self.clone(), req, head).await;
        }
        let conn = self.clone();
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => tunnel::intercept(conn, upgraded, head).await,
                Err(e) => debug!("CONNECT upgrade failed: {e}"),
            }
        });
        Response::new(empty_body())
    }

    fn in_tunnel(&self, tunnel: Arc<Tunnel>) -> Self {
        Self {
            tunnel: Some(tunnel),
            ..self.clone()
        }
    }
}

/// Passes an upstream response to the client while capturing its body.
fn respond(
    res: Response<Incoming>,
    exchange: Arc<Exchange>,
    client_version: Version,
    plan: Option<&Plan>,
) -> Response<ProxyBody> {
    let (mut parts, body) = res.into_parts();
    let replacement = plan.and_then(|plan| rules::edit_response(&mut parts, plan));
    exchange.set_response(parts.status.as_u16(), parts.version, &parts.headers);
    strip_hop_by_hop(&mut parts.headers, false);
    parts.version = client_version;
    let body = match replacement {
        Some(bytes) => CaptureBody::new(Full::new(bytes), Side::Response, exchange).boxed_unsync(),
        None => CaptureBody::new(body, Side::Response, exchange).boxed_unsync(),
    };
    Response::from_parts(parts, body)
}

/// Applies the request header rules and map_remote of `plan` to a WebSocket
/// handshake. Returns whether anything changed.
fn edit_handshake(parts: &mut http::request::Parts, plan: &Plan) -> bool {
    let mut changed =
        !plan.set_request_headers.is_empty() || !plan.remove_request_headers.is_empty();
    rules::apply_headers(
        &mut parts.headers,
        &plan.set_request_headers,
        &plan.remove_request_headers,
    );
    if let Some(target) = &plan.map_remote {
        match rules::remap(&parts.uri, target) {
            Some(uri) => {
                parts.uri = uri;
                changed = true;
            }
            None => {
                warn!("map_remote {target:?} is not a usable address; sending the handshake as is")
            }
        }
    }
    changed
}

fn upstream_failed(exchange: &Exchange, error: &(dyn StdError + 'static)) -> Response<ProxyBody> {
    let message = format!("upstream request failed: {}", error_chain(error));
    exchange.finish(None, Some(message.clone()));
    text_response(StatusCode::BAD_GATEWAY, &message)
}

/// `error` and its sources, joined, since hyper's top-level messages are vague.
fn error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(e) = source {
        let text = e.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = e.source();
    }
    parts.join(": ")
}

fn text_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let mut res = Response::new(
        Full::new(Bytes::from(format!("sieve: {message}\n")))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

fn empty_body() -> ProxyBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

fn is_websocket_upgrade<B>(req: &Request<B>) -> bool {
    let has_token = |name, token: &str| {
        req.headers().get_all(name).iter().any(|v| {
            v.to_str()
                .is_ok_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        })
    };
    req.version() <= Version::HTTP_11
        && has_token(header::CONNECTION, "upgrade")
        && has_token(header::UPGRADE, "websocket")
}

fn websocket_url(uri: &Uri) -> String {
    let url = uri.to_string();
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url
    }
}

/// Removes headers that apply to one connection only. With `keep_upgrade`,
/// `Connection: upgrade` and `Upgrade` survive for a WebSocket handshake.
fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        if !(keep_upgrade && name == header::UPGRADE) {
            headers.remove(name);
        }
    }
    // `te: trailers` is allowed end to end and gRPC needs it.
    if headers
        .get(header::TE)
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"trailers"))
    {
        headers.remove(header::TE);
    }
    for name in [
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if keep_upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(header::CONNECTION);
        headers.remove(header::UPGRADE);
    }
}

/// Matches a host against `passthrough_hosts`: exact names, or `*.example.com`
/// for any subdomain of example.com.
fn is_passthrough(patterns: &[String], host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('.')),
            None => host == pattern,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_matches_exact_and_wildcard_hosts() {
        let patterns = vec!["pinned.example".to_string(), "*.apple.com".to_string()];
        assert!(is_passthrough(&patterns, "pinned.example"));
        assert!(is_passthrough(&patterns, "PINNED.example."));
        assert!(!is_passthrough(&patterns, "sub.pinned.example"));
        assert!(is_passthrough(&patterns, "gs.apple.com"));
        assert!(is_passthrough(&patterns, "a.b.apple.com"));
        assert!(!is_passthrough(&patterns, "apple.com"));
        assert!(!is_passthrough(&patterns, "notapple.com"));
        assert!(is_passthrough(&["::1".to_string()], "[::1]"));
    }

    #[test]
    fn strips_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-private"),
        );
        headers.insert("x-private", HeaderValue::from_static("1"));
        headers.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
        headers.insert(header::TE, HeaderValue::from_static("gzip"));
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        strip_hop_by_hop(&mut headers, false);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key(header::ACCEPT));

        let mut headers = HeaderMap::new();
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
        strip_hop_by_hop(&mut headers, false);
        assert!(headers.contains_key(header::TE));
    }

    #[test]
    fn keeps_the_websocket_handshake_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("sec-websocket-key", HeaderValue::from_static("abc"));
        strip_hop_by_hop(&mut headers, true);
        assert_eq!(headers[header::CONNECTION], "upgrade");
        assert_eq!(headers[header::UPGRADE], "websocket");
        assert!(headers.contains_key("sec-websocket-key"));
    }

    #[test]
    fn websocket_urls_use_ws_schemes() {
        assert_eq!(
            websocket_url(&"https://example.com/socket".parse().unwrap()),
            "wss://example.com/socket"
        );
        assert_eq!(
            websocket_url(&"http://example.com/".parse().unwrap()),
            "ws://example.com/"
        );
    }
}

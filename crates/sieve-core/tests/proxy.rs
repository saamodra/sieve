//! End-to-end tests: reqwest -> sieve proxy -> a local rustls server, all on
//! 127.0.0.1 with throwaway CAs.

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use async_compression::tokio::write::GzipEncoder;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use sieve_core::{
    CertAuthority, Flow, Paths, ProxyConfig,
    flow::{self, FlowKind, FlowReceiver},
    proxy::Proxy,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

const HELLO: &str = "hello from upstream";
const TEXT: &str = "{\"message\": \"this body was gzipped by the upstream server\"}";
const BIG_LEN: usize = 64 * 1024;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn big_body() -> Vec<u8> {
    (0..BIG_LEN).map(|i| (i % 251) as u8).collect()
}

async fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(data).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

async fn upstream_handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let version = format!("{:?}", req.version());
    let path = req.uri().path().to_string();
    if path == "/ws" {
        return Ok(websocket_echo(req));
    }
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let res = Response::builder().header("x-upstream", "yes");
    let res = match path.as_str() {
        "/hello" => res
            .header("content-type", "text/plain")
            .body(Full::new(Bytes::from(HELLO))),
        "/echo" => res.body(Full::new(body)),
        "/gzip" => res
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Full::new(gzip(TEXT.as_bytes()).await.into())),
        "/big" => res.body(Full::new(big_body().into())),
        "/version" => res.body(Full::new(Bytes::from(version))),
        _ => res.status(404).body(Full::new(Bytes::new())),
    };
    Ok(res.unwrap())
}

/// Accepts the upgrade and echoes raw bytes back; enough to prove the proxy
/// relays an upgraded connection.
fn websocket_echo(mut req: Request<Incoming>) -> Response<Full<Bytes>> {
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let mut io = TokioIo::new(upgrade.await.unwrap());
        let mut buf = [0; 64];
        while let Ok(n @ 1..) = io.read(&mut buf).await {
            io.write_all(&buf[..n]).await.unwrap();
        }
    });
    Response::builder()
        .status(101)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        .body(Full::new(Bytes::new()))
        .unwrap()
}

struct Upstream {
    addr: SocketAddr,
    ca_der: CertificateDer<'static>,
}

/// An HTTPS server for localhost and 127.0.0.1 with its own CA, speaking the
/// given ALPN protocols.
async fn spawn_https_upstream(alpn: &[&[u8]]) -> Upstream {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(tls), service_fn(upstream_handler))
                    .await;
            });
        }
    });
    Upstream {
        addr,
        ca_der: ca_cert.der().clone(),
    }
}

async fn spawn_http_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(tcp), service_fn(upstream_handler))
                    .await;
            });
        }
    });
    addr
}

struct Sieve {
    addr: SocketAddr,
    ca: Arc<CertAuthority>,
    flows: FlowReceiver,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Sieve {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn spawn_sieve(config: ProxyConfig, upstream_ca: Option<&CertificateDer<'static>>) -> Sieve {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(CertAuthority::load_or_create(&Paths::at(dir.path())).unwrap());
    let flows = flow::channel();
    let rx = flows.subscribe();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut proxy = Proxy::new(config, ca.clone(), flows);
    if let Some(der) = upstream_ca {
        proxy = proxy.trust_upstream_root(der.to_vec());
    }
    let shutdown = CancellationToken::new();
    tokio::spawn(proxy.serve(listener, shutdown.clone()));
    Sieve {
        addr,
        ca,
        flows: rx,
        shutdown,
        _dir: dir,
    }
}

/// A reqwest client that uses sieve as its proxy and trusts only `root`.
fn client(proxy: SocketAddr, root: &[u8], http2: bool) -> reqwest::Client {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(root.to_vec())).unwrap();
    let mut tls = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = if http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
        .timeout(Duration::from_secs(10));
    if !http2 {
        builder = builder.http1_only();
    }
    builder.build().unwrap()
}

/// Waits for the completed version of the next flow matching `pred`.
async fn completed_flow(rx: &mut FlowReceiver, pred: impl Fn(&Flow) -> bool) -> Arc<Flow> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let flow = rx.recv().await.unwrap();
            if (flow.response.is_some() || flow.error.is_some())
                && flow.timing.total_ms.is_some()
                && pred(&flow)
            {
                return flow;
            }
        }
    })
    .await
    .expect("timed out waiting for a flow")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn assert_https_roundtrip(client_h2: bool, upstream_alpn: &[&[u8]], expect: (&str, &str)) {
    let upstream = spawn_https_upstream(upstream_alpn).await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), client_h2);

    let url = format!("https://localhost:{}/hello", upstream.addr.port());
    let res = client
        .get(&url)
        .header("x-client", "reqwest")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let client_version = format!("{:?}", res.version());
    assert_eq!(res.headers()["x-upstream"], "yes");
    assert_eq!(res.text().await.unwrap(), HELLO);
    assert_eq!(client_version, expect.0);

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.kind, FlowKind::Http);
    assert_eq!(flow.request.method, "GET");
    assert_eq!(flow.request.http_version, expect.0);
    assert_eq!(header(&flow.request.headers, "x-client"), Some("reqwest"));
    assert!(flow.client.unwrap().ip().is_loopback());
    assert!(flow.error.is_none(), "{:?}", flow.error);

    let res = flow.response.as_ref().unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(res.http_version, expect.1);
    assert_eq!(header(&res.headers, "x-upstream"), Some("yes"));
    assert_eq!(res.body.raw, HELLO.as_bytes());
    assert_eq!(res.body.size, HELLO.len() as u64);
    assert!(!res.body.truncated);
    assert_eq!(res.body.content_type.as_deref(), Some("text/plain"));
    assert!(res.body.decoded.is_none());
    assert!(flow.timing.wait_ms.unwrap() <= flow.timing.total_ms.unwrap());
}

#[tokio::test]
async fn https_http1_roundtrip() {
    assert_https_roundtrip(false, &[b"http/1.1"], ("HTTP/1.1", "HTTP/1.1")).await;
}

#[tokio::test]
async fn https_http2_roundtrip() {
    assert_https_roundtrip(true, &[b"h2"], ("HTTP/2.0", "HTTP/2.0")).await;
}

#[tokio::test]
async fn https_http1_client_to_http2_upstream() {
    assert_https_roundtrip(false, &[b"h2", b"http/1.1"], ("HTTP/1.1", "HTTP/2.0")).await;
}

#[tokio::test]
async fn https_to_an_ip_address() {
    let upstream = spawn_https_upstream(&[b"h2", b"http/1.1"]).await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), true);

    let url = format!("https://127.0.0.1:{}/hello", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), HELLO);
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.response.as_ref().unwrap().status, 200);
}

#[tokio::test]
async fn request_body_is_captured_and_forwarded() {
    let upstream = spawn_https_upstream(&[b"h2"]).await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), true);

    let url = format!("https://localhost:{}/echo", upstream.addr.port());
    let res = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{\"ping\":1}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "{\"ping\":1}");

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.request.method, "POST");
    assert_eq!(flow.request.body.raw, "{\"ping\":1}".as_bytes());
    assert_eq!(flow.request.body.size, 10);
    assert_eq!(
        flow.request.body.content_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(
        flow.response.as_ref().unwrap().body.raw,
        "{\"ping\":1}".as_bytes()
    );
}

/// An upstream that answers 413 as soon as it has read the request head,
/// without reading the body.
async fn spawn_early_413_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tcp.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tcp
                    .write_all(
                        b"HTTP/1.1 413 Payload Too Large\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                // Keep the socket open without reading the rest of the body.
                tokio::time::sleep(Duration::from_secs(1)).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn request_body_is_recorded_when_the_server_responds_early() {
    let upstream = spawn_early_413_upstream().await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let sent = vec![b'x'; 4 * 1024 * 1024];
    let url = format!("http://127.0.0.1:{}/upload", upstream.port());
    // The client may fail while sending; only the recorded flow matters here.
    let _ = client.post(&url).body(sent.clone()).send().await;

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.response.as_ref().unwrap().status, 413);
    let body = &flow.request.body;
    assert!(!body.raw.is_empty(), "the request body was not recorded");
    assert_eq!(body.size, body.raw.len() as u64);
    assert!(sent.starts_with(&body.raw));
}

#[tokio::test]
async fn gzip_body_is_decoded_in_the_flow_but_forwarded_as_is() {
    let upstream = spawn_https_upstream(&[b"http/1.1"]).await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let url = format!("https://localhost:{}/gzip", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.headers()["content-encoding"], "gzip");
    let on_the_wire = res.bytes().await.unwrap();
    assert_eq!(on_the_wire, gzip(TEXT.as_bytes()).await);

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    let body = &flow.response.as_ref().unwrap().body;
    assert_eq!(body.raw, on_the_wire);
    assert_eq!(body.content_encoding.as_deref(), Some("gzip"));
    assert_eq!(body.decoded.as_deref(), Some(TEXT.as_bytes()));
}

#[tokio::test]
async fn bodies_over_the_limit_are_truncated_in_the_flow_only() {
    let upstream = spawn_https_upstream(&[b"h2"]).await;
    let config = ProxyConfig {
        max_body_bytes: 1024,
        ..ProxyConfig::default()
    };
    let mut sieve = spawn_sieve(config, Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), true);

    let url = format!("https://localhost:{}/big", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.bytes().await.unwrap(), big_body());

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    let body = &flow.response.as_ref().unwrap().body;
    assert!(body.truncated);
    assert_eq!(body.size, BIG_LEN as u64);
    assert_eq!(body.raw, big_body()[..1024]);

    let url = format!("https://localhost:{}/echo", upstream.addr.port());
    let res = client.post(&url).body(big_body()).send().await.unwrap();
    assert_eq!(res.bytes().await.unwrap(), big_body());
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert!(flow.request.body.truncated);
    assert_eq!(flow.request.body.size, BIG_LEN as u64);
    assert_eq!(flow.request.body.raw.len(), 1024);
}

#[tokio::test]
async fn plain_http_is_proxied() {
    let upstream = spawn_http_upstream().await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let url = format!("http://127.0.0.1:{}/hello", upstream.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), HELLO);

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.kind, FlowKind::Http);
    assert_eq!(flow.response.as_ref().unwrap().body.raw, HELLO.as_bytes());
}

#[tokio::test]
async fn passthrough_hosts_are_tunneled_untouched() {
    let upstream = spawn_https_upstream(&[b"http/1.1"]).await;
    let config = ProxyConfig {
        passthrough_hosts: vec!["localhost".into()],
        ..ProxyConfig::default()
    };
    let mut sieve = spawn_sieve(config, None).await;
    // Trusting only the upstream's own CA proves the client saw the server's
    // certificate, not one from sieve.
    let client = client(sieve.addr, &upstream.ca_der, false);

    let url = format!("https://localhost:{}/hello", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), HELLO);
    drop(client);

    let target = format!("localhost:{}", upstream.addr.port());
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == target).await;
    assert_eq!(flow.kind, FlowKind::Passthrough);
    assert_eq!(flow.request.method, "CONNECT");
    assert!(flow.error.is_none(), "{:?}", flow.error);
    assert_eq!(flow.response.as_ref().unwrap().status, 200);
    assert!(flow.request.body.size > 0);
    assert!(flow.response.as_ref().unwrap().body.size > 0);
}

#[tokio::test]
async fn client_tls_rejection_becomes_a_flow_with_an_error() {
    let upstream = spawn_https_upstream(&[b"http/1.1"]).await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), Some(&upstream.ca_der)).await;
    // This client does not trust the sieve CA.
    let client = client(sieve.addr, &upstream.ca_der, false);

    let url = format!("https://localhost:{}/hello", upstream.addr.port());
    assert!(client.get(&url).send().await.is_err());

    let target = format!("localhost:{}", upstream.addr.port());
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == target).await;
    assert_eq!(flow.request.method, "CONNECT");
    assert!(flow.response.is_none());
    let error = flow.error.as_deref().unwrap();
    assert!(error.contains("TLS handshake"), "{error}");
    assert!(error.contains("does not trust the sieve CA"), "{error}");
}

#[tokio::test]
async fn upstream_failure_is_a_502_and_a_flow_with_an_error() {
    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = unused.local_addr().unwrap().port();
    drop(unused);
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let url = format!("http://127.0.0.1:{port}/");
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.status(), 502);

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert!(flow.response.is_none());
    assert!(
        flow.error
            .as_deref()
            .unwrap()
            .contains("upstream request failed")
    );
}

#[tokio::test]
async fn websocket_upgrade_is_relayed() {
    let upstream = spawn_http_upstream().await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;

    let mut tcp = TcpStream::connect(sieve.addr).await.unwrap();
    let request = format!(
        "GET http://{upstream}/ws HTTP/1.1\r\nHost: {upstream}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    tcp.write_all(request.as_bytes()).await.unwrap();

    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(tcp.read_u8().await.unwrap());
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("upgrade: websocket"),
        "{head}"
    );

    tcp.write_all(b"ping").await.unwrap();
    let mut echo = [0; 4];
    tcp.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");

    let url = format!("ws://{upstream}/ws");
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.kind, FlowKind::WebSocket);
    assert_eq!(flow.response.as_ref().unwrap().status, 101);
}

#[tokio::test]
async fn flow_ids_are_unique() {
    let upstream = spawn_http_upstream().await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let url = format!("http://127.0.0.1:{}/hello", upstream.port());
    let mut ids = Vec::new();
    for _ in 0..3 {
        client.get(&url).send().await.unwrap().text().await.unwrap();
        ids.push(
            completed_flow(&mut sieve.flows, |f| f.request.url == url)
                .await
                .id,
        );
    }
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

/// No flow at all arrives within a short wait.
async fn assert_no_flows(rx: &mut FlowReceiver) {
    let next = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(next.is_err(), "unexpected flow: {next:?}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn processes_outside_intercept_processes_are_tunneled_unrecorded() {
    let upstream = spawn_https_upstream(&[b"http/1.1"]).await;
    let config = ProxyConfig {
        intercept_processes: vec!["/no-such-program/".into()],
        ..ProxyConfig::default()
    };
    let mut sieve = spawn_sieve(config, None).await;
    // Trusting only the upstream's own CA proves the request was not intercepted.
    let client = client(sieve.addr, &upstream.ca_der, false);

    let url = format!("https://localhost:{}/hello", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), HELLO);
    drop(client);
    assert_no_flows(&mut sieve.flows).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn processes_in_intercept_processes_are_intercepted() {
    let upstream = spawn_https_upstream(&[b"http/1.1"]).await;
    // This test binary is the client.
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let config = ProxyConfig {
        intercept_processes: vec![exe.file_name().unwrap().to_string_lossy().into_owned()],
        ..ProxyConfig::default()
    };
    let mut sieve = spawn_sieve(config, Some(&upstream.ca_der)).await;
    let client = client(sieve.addr, sieve.ca.cert_der(), false);

    let url = format!("https://localhost:{}/hello", upstream.addr.port());
    let res = client.get(&url).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), HELLO);

    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.kind, FlowKind::Http);
    assert_eq!(flow.response.as_ref().unwrap().status, 200);
}

/// An upstream that accepts connections and never answers.
async fn spawn_silent_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            held.push(tcp);
        }
    });
    addr
}

#[tokio::test]
async fn a_request_the_client_cancels_becomes_a_flow_with_an_error() {
    let upstream = spawn_silent_upstream().await;
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;

    let url = format!("http://{upstream}/slow");
    let mut tcp = TcpStream::connect(sieve.addr).await.unwrap();
    let request = format!("GET {url} HTTP/1.1\r\nHost: {upstream}\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.unwrap();

    // The flow is published once when the request arrives, still pending.
    let started = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let flow = sieve.flows.recv().await.unwrap();
            if flow.request.url == url {
                return flow;
            }
        }
    })
    .await
    .expect("timed out waiting for the request");
    assert!(started.response.is_none() && started.error.is_none());

    drop(tcp);
    let flow = completed_flow(&mut sieve.flows, |f| f.id == started.id).await;
    assert!(flow.response.is_none());
    let error = flow.error.as_deref().unwrap();
    assert!(error.contains("cancelled"), "{error}");
}

async fn spawn_sieve_with_rules(rules: Vec<sieve_core::rules::Rule>) -> Sieve {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(CertAuthority::load_or_create(&Paths::at(dir.path())).unwrap());
    let engine = sieve_core::rules::RuleEngine::new(None);
    engine.replace(rules).unwrap();
    let flows = flow::channel();
    let rx = flows.subscribe();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = Proxy::new(ProxyConfig::default(), ca.clone(), flows).with_rules(engine);
    let shutdown = CancellationToken::new();
    tokio::spawn(proxy.serve(listener, shutdown.clone()));
    Sieve {
        addr,
        ca,
        flows: rx,
        shutdown,
        _dir: dir,
    }
}

fn host_rule(host: &str, action: sieve_core::rules::Action) -> sieve_core::rules::Rule {
    sieve_core::rules::Rule {
        matcher: sieve_core::rules::Matcher {
            host: Some(host.to_string()),
            ..Default::default()
        },
        action,
        ..Default::default()
    }
}

/// Sends a WebSocket handshake for `url` through the proxy and returns the
/// response head.
async fn websocket_handshake(proxy: SocketAddr, url: &str, host: &str) -> (TcpStream, String) {
    let mut tcp = TcpStream::connect(proxy).await.unwrap();
    let request = format!(
        "GET {url} HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(tcp.read_u8().await.unwrap());
        }
    })
    .await
    .expect("timed out waiting for the handshake response");
    (tcp, String::from_utf8(head).unwrap())
}

#[tokio::test]
async fn a_block_rule_blocks_the_websocket_handshake() {
    // Counts the connections that reach the upstream.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let (reached, mut reached_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let _ = reached.send(tcp);
        }
    });
    let mut sieve = spawn_sieve_with_rules(vec![host_rule(
        "127.0.0.1",
        sieve_core::rules::Action {
            block: true,
            ..Default::default()
        },
    )])
    .await;

    let url = format!("http://{upstream}/ws");
    let (_tcp, head) = websocket_handshake(sieve.addr, &url, &upstream.to_string()).await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");

    let flow = completed_flow(&mut sieve.flows, |f| {
        f.request.url == format!("ws://{upstream}/ws")
    })
    .await;
    assert_eq!(flow.kind, FlowKind::WebSocket);
    assert_eq!(flow.response.as_ref().unwrap().status, 403);
    let reached = tokio::time::timeout(Duration::from_millis(300), reached_rx.recv()).await;
    assert!(reached.is_err(), "the handshake reached the upstream");
}

#[tokio::test]
async fn map_remote_and_header_rules_apply_to_the_websocket_handshake() {
    let upstream = spawn_http_upstream().await;
    let mut sieve = spawn_sieve_with_rules(vec![host_rule(
        "ws.invalid",
        sieve_core::rules::Action {
            map_remote: Some(upstream.to_string()),
            set_request_headers: [("x-via".to_string(), "sieve".to_string())].into(),
            ..Default::default()
        },
    )])
    .await;

    let (mut tcp, head) =
        websocket_handshake(sieve.addr, "http://ws.invalid/ws", "ws.invalid").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    tcp.write_all(b"ping").await.unwrap();
    let mut echo = [0; 4];
    tcp.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");

    let flow = completed_flow(&mut sieve.flows, |f| f.kind == FlowKind::WebSocket).await;
    assert_eq!(flow.request.url, format!("ws://{upstream}/ws"));
    assert_eq!(header(&flow.request.headers, "x-via"), Some("sieve"));
    assert_eq!(flow.response.as_ref().unwrap().status, 101);
}

/// An address of this machine other than loopback, found by asking the OS
/// which interface routes to a documentation address (nothing is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Sends `request` to the proxy from `from`, so the proxy sees a client that
/// is not on loopback, and returns the response: its head, then as many body
/// bytes as its `content-length` says (the connection may stay open).
async fn raw_request_from(from: std::net::IpAddr, proxy: SocketAddr, request: &str) -> String {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(from, 0)).unwrap();
    let mut tcp = socket.connect(proxy).await.unwrap();
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = [0; 1024];
        loop {
            let text = String::from_utf8_lossy(&answer).to_ascii_lowercase();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let wanted = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .map_or(0, |n| n.trim().parse().unwrap());
                if body.len() >= wanted {
                    return;
                }
            }
            match tcp.read(&mut buf).await.unwrap() {
                0 => return,
                n => answer.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .expect("timed out waiting for the proxy");
    String::from_utf8_lossy(&answer).into_owned()
}

async fn assert_refused_flow(sieve: &mut Sieve, url: &str) {
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url == url).await;
    assert_eq!(flow.response.as_ref().unwrap().status, 403);
    let error = flow.error.as_deref().unwrap();
    assert!(error.contains("loopback"), "{error}");
}

#[tokio::test]
async fn lan_clients_cannot_reach_loopback_targets() {
    let Some(lan) = lan_ip() else {
        eprintln!("skipped: no non-loopback address on this machine");
        return;
    };
    // Counts the connections that reach the upstream.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let (reached, mut reached_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let _ = reached.send(tcp);
        }
    });
    let mut sieve = spawn_sieve(ProxyConfig::default(), None).await;

    let url = format!("http://{upstream}/hello");
    let request = format!("GET {url} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n");
    let answer = raw_request_from(lan, sieve.addr, &request).await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert!(answer.contains("sieve: "), "{answer}");
    assert_refused_flow(&mut sieve, &url).await;

    let localhost = format!("http://localhost:{}/hello", upstream.port());
    let request =
        format!("GET {localhost} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let answer = raw_request_from(lan, sieve.addr, &request).await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

    let request = format!("CONNECT {upstream} HTTP/1.1\r\nHost: {upstream}\r\n\r\n");
    let answer = raw_request_from(lan, sieve.addr, &request).await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert_refused_flow(&mut sieve, &upstream.to_string()).await;

    let ws = format!("http://{upstream}/ws");
    let request = format!(
        "GET {ws} HTTP/1.1\r\nHost: {upstream}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    let answer = raw_request_from(lan, sieve.addr, &request).await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert_refused_flow(&mut sieve, &format!("ws://{upstream}/ws")).await;

    let reached = tokio::time::timeout(Duration::from_millis(300), reached_rx.recv()).await;
    assert!(reached.is_err(), "a refused request reached the upstream");
}

#[tokio::test]
async fn lan_clients_may_reach_loopback_with_the_opt_in() {
    let Some(lan) = lan_ip() else {
        eprintln!("skipped: no non-loopback address on this machine");
        return;
    };
    let upstream = spawn_http_upstream().await;
    let config = ProxyConfig {
        allow_lan_to_loopback: true,
        ..Default::default()
    };
    let sieve = spawn_sieve(config, None).await;

    let url = format!("http://{upstream}/hello");
    let request = format!("GET {url} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n");
    let answer = raw_request_from(lan, sieve.addr, &request).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with(HELLO), "{answer}");
}

#[tokio::test]
async fn map_remote_to_loopback_works_for_lan_clients() {
    let Some(lan) = lan_ip() else {
        eprintln!("skipped: no non-loopback address on this machine");
        return;
    };
    let upstream = spawn_http_upstream().await;
    let sieve = spawn_sieve_with_rules(vec![host_rule(
        "mapped.invalid",
        sieve_core::rules::Action {
            map_remote: Some(upstream.to_string()),
            ..Default::default()
        },
    )])
    .await;

    let request = "GET http://mapped.invalid/hello HTTP/1.1\r\nHost: mapped.invalid\r\n\
                   Connection: close\r\n\r\n";
    let answer = raw_request_from(lan, sieve.addr, request).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with(HELLO), "{answer}");
}

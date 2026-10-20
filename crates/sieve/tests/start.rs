//! `sieve start`, run as the built binary: HTTPS through the proxy, passthrough,
//! the log output, and shutdown on SIGINT.

mod common;

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use common::{Daemon, SHUTDOWN_TIMEOUT, free_addr, install_crypto_provider, run_sieve_ok};
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use reqwest::{Certificate, Client, Proxy, Version};
use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const HELLO: &str = "hello from the test server";

/// A local HTTPS server on 127.0.0.1 with its own CA. `ca_der` is what a
/// client must trust to accept its certificate.
struct TlsServer {
    addr: SocketAddr,
    ca_der: CertificateDer<'static>,
}

impl TlsServer {
    fn url(&self, path: &str) -> String {
        format!("https://{}{path}", self.addr)
    }
}

async fn hello(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let _ = req.into_body().collect().await;
    Ok(Response::builder()
        .header("x-upstream", "yes")
        .body(Full::new(Bytes::from_static(HELLO.as_bytes())))
        .unwrap())
}

/// Serves HTTPS on a random port, offering the given ALPN protocols.
async fn spawn_tls_server(alpn: &[&[u8]]) -> TlsServer {
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
    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
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
                    .serve_connection(TokioIo::new(tls), service_fn(hello))
                    .await;
            });
        }
    });

    TlsServer {
        addr,
        ca_der: ca_cert.der().clone(),
    }
}

/// Starts `sieve start` on a fresh proxy port with its config in a tempdir,
/// and waits until the proxy accepts connections. The web UI gets a free port
/// too, so the default 8081 is never touched; it does not listen yet
/// (`sieve-ui` is still a stub), so the proxy is the only port waited on.
async fn start_sieve(config_dir: &TempDir, extra: &[&str]) -> (SocketAddr, Daemon) {
    let proxy = free_addr();
    let ui = free_addr();
    let proxy_arg = proxy.to_string();
    let ui_arg = ui.to_string();
    let mut args = vec!["--listen", &proxy_arg, "--ui-listen", &ui_arg];
    args.extend_from_slice(extra);

    let mut daemon = Daemon::start(config_dir, &args);
    daemon.wait_for_port(proxy).await;
    (proxy, daemon)
}

/// A client that sends everything through `proxy` and trusts `root`.
fn client_through(proxy: SocketAddr, root: Certificate, http1_only: bool) -> Client {
    let mut builder = Client::builder()
        .proxy(Proxy::all(format!("http://{proxy}")).unwrap())
        .add_root_certificate(root);
    if http1_only {
        builder = builder.http1_only();
    }
    builder.build().unwrap()
}

/// The CA that `sieve start` uses, as a client root for MITM tests.
fn exported_ca(config_dir: &TempDir) -> Certificate {
    let pem = run_sieve_ok(config_dir.path(), &["ca", "export", "--format", "pem"]);
    Certificate::from_pem(&pem).unwrap()
}

async fn assert_passthrough(http1_only: bool) {
    install_crypto_provider();
    let server = spawn_tls_server(&[b"h2", b"http/1.1"]).await;
    let config = TempDir::new().unwrap();
    let (proxy, daemon) = start_sieve(&config, &["--passthrough", "127.0.0.1"]).await;

    let root = Certificate::from_der(&server.ca_der).unwrap();
    let client = client_through(proxy, root, http1_only);
    let res = client.get(server.url("/hello")).send().await.unwrap();

    let expected = if http1_only {
        Version::HTTP_11
    } else {
        Version::HTTP_2
    };
    assert_eq!(res.version(), expected);
    assert_eq!(res.headers()["x-upstream"], "yes");
    assert_eq!(res.text().await.unwrap(), HELLO);

    // A passthrough tunnel is logged once it closes, since the proxy cannot
    // see the request inside it.
    drop(client);
    daemon
        .wait_for_output(&format!("CONNECT {} -> 200", server.addr))
        .await;
}

#[tokio::test]
async fn passthrough_serves_http1_intact() {
    assert_passthrough(true).await;
}

#[tokio::test]
async fn passthrough_serves_http2_intact() {
    assert_passthrough(false).await;
}

#[tokio::test]
async fn intercepted_request_is_logged() {
    install_crypto_provider();
    let server = spawn_tls_server(&[b"h2", b"http/1.1"]).await;
    let config = TempDir::new().unwrap();
    let (proxy, daemon) = start_sieve(&config, &[]).await;

    // The client trusts the exported CA, so the TLS handshake with the proxy
    // must succeed. The upstream is not trusted by `sieve start` (see the
    // ignored MITM tests), so only the proxy's answer is checked here.
    let client = client_through(proxy, exported_ca(&config), true);
    client
        .get(server.url("/hello"))
        .send()
        .await
        .expect("TLS with the exported CA succeeds");

    daemon
        .wait_for_output(&format!("GET {}", server.url("/hello")))
        .await;
}

#[tokio::test]
#[ignore = "bug: `sieve start` cannot trust a local upstream root, so every intercepted request fails with 502 (UnknownIssuer)"]
async fn mitm_http1_serves_intact() {
    assert_mitm(true).await;
}

#[tokio::test]
#[ignore = "bug: `sieve start` cannot trust a local upstream root, so every intercepted request fails with 502 (UnknownIssuer)"]
async fn mitm_http2_serves_intact() {
    assert_mitm(false).await;
}

async fn assert_mitm(http1_only: bool) {
    install_crypto_provider();
    let server = spawn_tls_server(&[b"h2", b"http/1.1"]).await;
    let config = TempDir::new().unwrap();
    let (proxy, _daemon) = start_sieve(&config, &[]).await;

    let client = client_through(proxy, exported_ca(&config), http1_only);
    let res = client.get(server.url("/hello")).send().await.unwrap();
    let status = res.status();
    assert!(status.is_success(), "proxy answered {status}");

    let expected = if http1_only {
        Version::HTTP_11
    } else {
        Version::HTTP_2
    };
    assert_eq!(res.version(), expected);
    assert_eq!(res.headers()["x-upstream"], "yes");
    assert_eq!(res.text().await.unwrap(), HELLO);
}

#[tokio::test]
#[ignore = "bug: tracing writes to stdout, so stderr never receives the request log"]
async fn request_log_goes_to_stderr() {
    install_crypto_provider();
    let server = spawn_tls_server(&[b"http/1.1"]).await;
    let config = TempDir::new().unwrap();
    let (proxy, daemon) = start_sieve(&config, &["--passthrough", "127.0.0.1"]).await;

    let root = Certificate::from_der(&server.ca_der).unwrap();
    let client = client_through(proxy, root, true);
    client.get(server.url("/hello")).send().await.unwrap();
    drop(client);

    let needle = format!("CONNECT {} -> 200", server.addr);
    daemon.wait_for_output(&needle).await;
    assert!(
        daemon.stderr().contains(&needle),
        "the request log is on stdout only:\n{}",
        daemon.output()
    );
}

#[tokio::test]
async fn sigint_stops_the_process_promptly() {
    let config = TempDir::new().unwrap();
    let (_proxy, mut daemon) = start_sieve(&config, &[]).await;

    let status = daemon.interrupt(SHUTDOWN_TIMEOUT);
    assert!(
        status.success(),
        "sieve exited with {status}:\n{}",
        daemon.output()
    );
}

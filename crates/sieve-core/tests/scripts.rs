//! Scripting plugins end to end: reqwest -> sieve -> a local HTTP server,
//! all on 127.0.0.1.

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rustls::{ClientConfig, RootCertStore, crypto::ring};
use sieve_core::{CertAuthority, Paths, ProxyConfig, flow, proxy::Proxy, script::ScriptEngine};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

async fn upstream_handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    let via = req
        .headers()
        .get("x-via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    Ok(Response::builder()
        .body(Full::new(Bytes::from(format!("{path} via={via}"))))
        .unwrap())
}

async fn spawn_upstream() -> SocketAddr {
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
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Sieve {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn spawn_sieve(source: &str) -> Sieve {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(CertAuthority::load_or_create(&Paths::at(dir.path())).unwrap());
    let mut engine = ScriptEngine::new();
    engine.load_source("test", source).unwrap();
    let flows = flow::channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = Proxy::new(ProxyConfig::default(), ca, flows).with_scripts(Arc::new(engine));
    let shutdown = CancellationToken::new();
    tokio::spawn(proxy.serve(listener, shutdown.clone()));
    Sieve {
        addr,
        shutdown,
        _dir: dir,
    }
}

fn client(proxy: SocketAddr) -> reqwest::Client {
    let tls = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

#[tokio::test]
async fn on_request_hook_rewrites_a_header_seen_by_the_server() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(
        r#"fn on_request(req) {
            req.headers.set_header("x-via", "script");
            req
        }"#,
    )
    .await;
    let res = client(sieve.addr)
        .get(format!("http://{upstream}/thing"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "/thing via=script");
}

#[tokio::test]
async fn on_request_hook_can_answer_without_the_server() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(
        r#"fn on_request(req) {
            if req.url.contains("/blocked") {
                req.respond = #{ status: 204, body: "" };
            }
            req
        }"#,
    )
    .await;
    let res = client(sieve.addr)
        .get(format!("http://{upstream}/blocked"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
}

#[tokio::test]
async fn on_response_hook_rewrites_the_body_the_client_sees() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(
        r#"fn on_response(req, res) {
            res.body.replace("via=-", "via=rewritten");
            res
        }"#,
    )
    .await;
    let res = client(sieve.addr)
        .get(format!("http://{upstream}/thing"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "/thing via=rewritten");
}

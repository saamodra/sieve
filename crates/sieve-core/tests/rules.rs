//! Rules and breakpoints end to end: reqwest -> sieve -> a local HTTP server,
//! all on 127.0.0.1.

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rustls::{ClientConfig, RootCertStore, crypto::ring};
use sieve_core::{
    CertAuthority, Flow, Paths, ProxyConfig,
    flow::{self, FlowReceiver},
    proxy::Proxy,
    rules::{
        Action, Decision, Matcher, Mock, RequestEdit, Rule, RuleEngine, breakpoint::BreakpointEvent,
    },
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Replies with the method, path, `x-via` header and body it received.
async fn upstream_handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let via = req
        .headers()
        .get("x-via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let text = format!(
        "{method} {path} via={via} body={}",
        String::from_utf8_lossy(&body)
    );
    Ok(Response::builder()
        .header("x-upstream", "yes")
        .body(Full::new(Bytes::from(text)))
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
    flows: FlowReceiver,
    engine: Arc<RuleEngine>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Sieve {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn spawn_sieve(rules: Vec<Rule>) -> Sieve {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(CertAuthority::load_or_create(&Paths::at(dir.path())).unwrap());
    let engine = RuleEngine::new(None);
    engine.replace(rules).unwrap();
    let flows = flow::channel();
    let rx = flows.subscribe();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = Proxy::new(ProxyConfig::default(), ca, flows).with_rules(engine.clone());
    let shutdown = CancellationToken::new();
    tokio::spawn(proxy.serve(listener, shutdown.clone()));
    Sieve {
        addr,
        flows: rx,
        engine,
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

fn rule(host: &str, action: Action) -> Rule {
    Rule {
        matcher: Matcher {
            host: Some(host.to_string()),
            ..Matcher::default()
        },
        action,
        ..Rule::default()
    }
}

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

#[tokio::test]
async fn map_remote_sends_the_request_to_another_server() {
    let upstream = spawn_upstream().await;
    let mut sieve = spawn_sieve(vec![rule(
        "api.prod.invalid",
        Action {
            map_remote: Some(upstream.to_string()),
            ..Action::default()
        },
    )])
    .await;
    let res = client(sieve.addr)
        .get("http://api.prod.invalid/v1/thing")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "GET /v1/thing via=- body=");
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url.contains("/v1/thing")).await;
    assert_eq!(flow.request.url, format!("http://{upstream}/v1/thing"));
}

#[tokio::test]
async fn mock_and_block_answer_without_the_server() {
    let mut sieve = spawn_sieve(vec![
        Rule {
            matcher: Matcher {
                host: Some("mock.invalid".into()),
                ..Matcher::default()
            },
            action: Action {
                mock: Some(Mock {
                    status: 418,
                    headers: [("x-mock".to_string(), "1".to_string())].into(),
                    body: Some("teapot".into()),
                    body_file: None,
                }),
                ..Action::default()
            },
            ..Rule::default()
        },
        rule(
            "blocked.invalid",
            Action {
                block: true,
                ..Action::default()
            },
        ),
    ])
    .await;
    let client = client(sieve.addr);

    let res = client.get("http://mock.invalid/x").send().await.unwrap();
    assert_eq!(res.status(), 418);
    assert_eq!(res.headers()["x-mock"], "1");
    assert_eq!(res.text().await.unwrap(), "teapot");
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url.contains("mock.invalid")).await;
    assert_eq!(flow.response.as_ref().unwrap().status, 418);
    assert_eq!(flow.response.as_ref().unwrap().body.raw, "teapot");

    let res = client.get("http://blocked.invalid/x").send().await.unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn rewrites_change_the_request_and_the_response() {
    let upstream = spawn_upstream().await;
    let host = upstream.to_string();
    let mut sieve = spawn_sieve(vec![rule(
        &host,
        Action {
            set_request_headers: [("x-via".to_string(), "sieve".to_string())].into(),
            set_response_headers: [("x-extra".to_string(), "added".to_string())].into(),
            remove_response_headers: vec!["x-upstream".into()],
            response_status: Some(202),
            delay_ms: Some(150),
            ..Action::default()
        },
    )])
    .await;
    let started = std::time::Instant::now();
    let res = client(sieve.addr)
        .post(format!("http://{upstream}/p"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(res.status(), 202);
    assert_eq!(res.headers()["x-extra"], "added");
    assert!(res.headers().get("x-upstream").is_none());
    assert_eq!(res.text().await.unwrap(), "POST /p via=sieve body=data");
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url.ends_with("/p")).await;
    assert_eq!(flow.response.as_ref().unwrap().status, 202);
}

#[tokio::test]
async fn response_bodies_can_be_replaced() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(vec![rule(
        &upstream.to_string(),
        Action {
            response_body: Some("replaced".into()),
            ..Action::default()
        },
    )])
    .await;
    let res = client(sieve.addr)
        .get(format!("http://{upstream}/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "replaced");
}

fn breakpoint_rule(upstream: SocketAddr) -> Rule {
    rule(
        &upstream.to_string(),
        Action {
            breakpoint: true,
            ..Action::default()
        },
    )
}

async fn next_held(events: &mut tokio::sync::broadcast::Receiver<BreakpointEvent>) -> u64 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let BreakpointEvent::Held { request } = events.recv().await.unwrap() {
                return request.id;
            }
        }
    })
    .await
    .expect("no request was held")
}

#[tokio::test]
async fn a_breakpoint_holds_the_request_until_continue() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(vec![breakpoint_rule(upstream)]).await;
    let mut events = sieve.engine.breakpoints().subscribe();
    let client = client(sieve.addr);
    let url = format!("http://{upstream}/held");
    let request = tokio::spawn(async move { client.post(url).body("original").send().await });

    let id = next_held(&mut events).await;
    let held = sieve.engine.breakpoints().pending();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].body.as_deref(), Some("original"));
    assert_eq!(held[0].method, "POST");
    assert!(!request.is_finished());

    let mut headers = held[0].headers.clone();
    headers.push(("x-via".into(), "edited".into()));
    let edit = RequestEdit {
        headers: Some(headers),
        body: Some("changed body".into()),
        ..RequestEdit::default()
    };
    assert!(
        sieve
            .engine
            .breakpoints()
            .resolve(id, Decision::Continue(edit))
    );

    let res = request.await.unwrap().unwrap();
    assert_eq!(
        res.text().await.unwrap(),
        "POST /held via=edited body=changed body"
    );
    assert!(sieve.engine.breakpoints().pending().is_empty());
}

#[tokio::test]
async fn aborting_a_breakpoint_fails_the_request() {
    let upstream = spawn_upstream().await;
    let mut sieve = spawn_sieve(vec![breakpoint_rule(upstream)]).await;
    let mut events = sieve.engine.breakpoints().subscribe();
    let client = client(sieve.addr);
    let url = format!("http://{upstream}/abort");
    let request = tokio::spawn(async move { client.get(url).send().await });

    let id = next_held(&mut events).await;
    assert!(sieve.engine.breakpoints().resolve(id, Decision::Abort));
    let res = request.await.unwrap().unwrap();
    assert_eq!(res.status().as_u16(), 499);
    let flow = completed_flow(&mut sieve.flows, |f| f.request.url.ends_with("/abort")).await;
    assert!(flow.error.as_deref().unwrap().contains("breakpoint"));
}

#[tokio::test]
async fn an_unanswered_breakpoint_times_out_and_continues() {
    let upstream = spawn_upstream().await;
    let sieve = spawn_sieve(vec![breakpoint_rule(upstream)]).await;
    sieve
        .engine
        .breakpoints()
        .set_timeout(Duration::from_millis(200));
    let res = client(sieve.addr)
        .get(format!("http://{upstream}/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "GET /slow via=- body=");
    assert!(sieve.engine.breakpoints().pending().is_empty());
}

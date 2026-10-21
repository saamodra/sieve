//! Serves the web UI with a stream of synthetic flows, no proxy needed.
//!
//! `cargo run -p sieve-ui --example demo [-- 127.0.0.1:8081]`

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use sieve_core::{
    Flow, UiConfig,
    flow::{self, Body, FlowKind, Request, Response, Timing},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// A 1x1 PNG, enough to exercise the image preview.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const HOSTS: &[&str] = &[
    "api.example.com",
    "cdn.example.com",
    "auth.example.com",
    "analytics.tracker.io",
    "images.example.net",
];
const CLIENTS: &[&str] = &[
    "192.168.1.23:51234",
    "192.168.1.57:40022",
    "10.0.2.16:33000",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next() as usize % items.len()]
    }
}

fn body(bytes: Bytes, content_type: &str) -> Body {
    Body {
        size: bytes.len() as u64,
        raw: bytes,
        content_type: Some(content_type.into()),
        ..Body::default()
    }
}

fn synthetic(id: u64, rng: &mut Rng) -> Flow {
    let host = *rng.pick(HOSTS);
    let (method, path, req_body, status, resp) = match rng.next() % 8 {
        0 => (
            "POST",
            "/v1/login".to_string(),
            body(
                Bytes::from_static(br#"{"email":"dev@example.com","password":"hunter2"}"#),
                "application/json",
            ),
            200,
            body(
                Bytes::from(format!(
                    r#"{{"token":"tok_{id:08x}","user":{{"id":{id},"name":"Dev","roles":["admin","dev"]}}}}"#
                )),
                "application/json",
            ),
        ),
        1 => (
            "GET",
            format!("/img/{id}.png"),
            Body::default(),
            200,
            body(Bytes::from_static(PNG), "image/png"),
        ),
        2 => (
            "GET",
            format!("/blob/{id}.bin"),
            Body::default(),
            200,
            body(
                Bytes::from((0..300).map(|i| (i * 7 + id) as u8).collect::<Vec<_>>()),
                "application/octet-stream",
            ),
        ),
        3 => (
            "PUT",
            format!("/v1/items/{id}"),
            body(Bytes::from_static(br#"{"done":true}"#), "application/json"),
            404,
            body(
                Bytes::from_static(br#"{"error":"not_found"}"#),
                "application/json",
            ),
        ),
        4 => (
            "GET",
            format!("/v1/search?q=rust&page={}", id % 5),
            Body::default(),
            500,
            body(Bytes::from_static(b"internal error"), "text/plain"),
        ),
        5 => (
            "POST",
            "/collect".to_string(),
            body(
                Bytes::from_static(b"event=view&n=1"),
                "application/x-www-form-urlencoded",
            ),
            204,
            Body::default(),
        ),
        _ => (
            "GET",
            format!("/v1/feed?cursor={id}"),
            Body::default(),
            200,
            body(
                Bytes::from(format!(
                    r#"{{"items":[{{"id":{id},"title":"Item {id}"}},{{"id":{},"title":"Item {}"}}],"next":"{}"}}"#,
                    id + 1,
                    id + 1,
                    id + 2
                )),
                "application/json",
            ),
        ),
    };
    let wait = 5.0 + (rng.next() % 300) as f64;
    Flow {
        id,
        started_at: OffsetDateTime::now_utc(),
        client: Some(rng.pick(CLIENTS).parse().unwrap()),
        kind: FlowKind::Http,
        request: Request {
            method: method.into(),
            url: format!("https://{host}{path}"),
            http_version: "HTTP/2".into(),
            headers: vec![
                ("host".into(), host.into()),
                ("user-agent".into(), "okhttp/4.12.0".into()),
                ("accept".into(), "*/*".into()),
            ],
            body: req_body,
        },
        response: Some(Response {
            status,
            http_version: "HTTP/2".into(),
            headers: vec![
                (
                    "content-type".into(),
                    resp.content_type.clone().unwrap_or_default(),
                ),
                ("server".into(), "demo".into()),
            ],
            body: resp,
        }),
        error: None,
        timing: Timing {
            wait_ms: Some(wait),
            total_ms: Some(wait + (rng.next() % 80) as f64),
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8081".into())
        .parse()?;
    let tx = flow::channel();
    let shutdown = CancellationToken::new();
    let ui = tokio::spawn(sieve_ui::serve(
        UiConfig {
            listen,
            token: None,
        },
        tx.clone(),
        axum::Router::new(),
        shutdown.clone(),
    ));

    let mut rng = Rng(42);
    let mut id = 0;
    println!("demo UI on http://{listen}  (ctrl-c to stop)");
    // Each flow is announced as a pending request first, then completed, the
    // way the proxy reports them.
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => {
                id += 1;
                let done = synthetic(id, &mut rng);
                let mut pending = done.clone();
                pending.response = None;
                pending.timing = Timing::default();
                let _ = tx.send(Arc::new(pending));
                tokio::time::sleep(Duration::from_millis(40)).await;
                let _ = tx.send(Arc::new(done));
            }
        }
    }
    shutdown.cancel();
    ui.await??;
    Ok(())
}

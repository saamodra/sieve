use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    routing::get,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use sieve_core::{
    Flow, FlowSender, UiConfig,
    flow::{self, Body as FlowBody, FlowKind, Request as Req, Response as Resp, Timing},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::build;
use crate::store::FlowStore;

fn flow(id: u64, method: &str, url: &str, status: Option<u16>) -> Flow {
    Flow {
        id,
        started_at: OffsetDateTime::UNIX_EPOCH,
        client: Some("10.0.0.5:5000".parse().unwrap()),
        kind: FlowKind::Http,
        request: Req {
            method: method.into(),
            url: url.into(),
            http_version: "HTTP/1.1".into(),
            headers: vec![("Accept".into(), "*/*".into())],
            body: FlowBody::default(),
        },
        response: status.map(|status| Resp {
            status,
            http_version: "HTTP/1.1".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: FlowBody {
                raw: Bytes::from_static(b"{\"ok\":true}"),
                size: 11,
                content_type: Some("application/json".into()),
                ..FlowBody::default()
            },
        }),
        error: None,
        timing: Timing {
            wait_ms: Some(10.0),
            total_ms: Some(25.0),
        },
    }
}

struct Harness {
    app: Router,
    tx: FlowSender,
    shutdown: CancellationToken,
}

fn harness(config: UiConfig, extra: Router) -> Harness {
    let tx = flow::channel();
    let shutdown = CancellationToken::new();
    let app = build(&config, &tx, extra, &shutdown).unwrap();
    Harness { app, tx, shutdown }
}

fn loopback() -> Harness {
    harness(UiConfig::default(), Router::new())
}

async fn send(h: &Harness, flows: impl IntoIterator<Item = Flow>) {
    for f in flows {
        h.tx.send(Arc::new(f)).unwrap();
    }
    // Let the ingest task drain the channel.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    call_with(app, method, uri, &[]).await
}

async fn call_with(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (
        status,
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
}

async fn get_json(app: &Router, uri: &str) -> Value {
    let (status, body) = call(app, "GET", uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice(&body).unwrap()
}

fn ids(list: &Value) -> Vec<u64> {
    list["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn list_pages_and_upserts() {
    let h = loopback();
    send(
        &h,
        (1..=5).map(|i| flow(i, "GET", &format!("https://a.test/{i}"), Some(200))),
    )
    .await;
    let all = get_json(&h.app, "/api/flows").await;
    assert_eq!(all["total"], 5);
    assert_eq!(ids(&all), [1, 2, 3, 4, 5]);

    let page = get_json(&h.app, "/api/flows?offset=1&limit=2").await;
    assert_eq!(page["total"], 5);
    assert_eq!(ids(&page), [2, 3]);

    // Same id again replaces the flow in place.
    send(&h, [flow(2, "GET", "https://a.test/2", Some(404))]).await;
    let all = get_json(&h.app, "/api/flows").await;
    assert_eq!(all["total"], 5);
    assert_eq!(all["flows"][1]["status"], 404);
}

#[tokio::test]
async fn list_filters() {
    let h = loopback();
    let mut other_client = flow(4, "GET", "https://c.test/x", Some(200));
    other_client.client = Some("10.0.0.9:1".parse().unwrap());
    let mut failed = flow(5, "GET", "https://a.test/boom", None);
    failed.error = Some("tls handshake failed".into());
    send(
        &h,
        [
            flow(1, "GET", "https://a.test/users?id=7", Some(200)),
            flow(2, "POST", "https://b.test/login", Some(401)),
            flow(3, "GET", "https://a.test/missing", Some(404)),
            other_client,
            failed,
        ],
    )
    .await;
    let q = |s: &str| {
        let app = h.app.clone();
        let uri = format!("/api/flows?{s}");
        async move { ids(&get_json(&app, &uri).await) }
    };
    assert_eq!(q("host=B.TEST").await, [2]);
    assert_eq!(q("method=post").await, [2]);
    assert_eq!(q("method=GET,POST").await, [1, 2, 3, 4, 5]);
    assert_eq!(q("status=404").await, [3]);
    assert_eq!(q("status=4xx").await, [2, 3]);
    assert_eq!(q("status=2xx,err").await, [1, 4, 5]);
    assert_eq!(q("q=USERS").await, [1]);
    assert_eq!(q("q=tls").await, [5]);
    assert_eq!(q("client=10.0.0.9").await, [4]);
    assert_eq!(q("host=a.test&status=4xx").await, [3]);
    assert!(q("q=nothing-matches").await.is_empty());
}

#[tokio::test]
async fn detail_has_base64_and_text() {
    let h = loopback();
    let mut f = flow(1, "POST", "https://a.test/up", Some(200));
    f.request.body = FlowBody {
        raw: Bytes::from_static(&[0xff, 0x00, 0xfe]),
        size: 3,
        ..FlowBody::default()
    };
    send(&h, [f]).await;
    let d = get_json(&h.app, "/api/flows/1").await;
    assert_eq!(d["summary"]["host"], "a.test");
    assert_eq!(d["summary"]["path"], "/up");
    assert_eq!(d["response"]["body"]["text"], "{\"ok\":true}");
    assert_eq!(d["response"]["body"]["base64"], "eyJvayI6dHJ1ZX0=");
    assert_eq!(d["request"]["body"]["base64"], "/wD+");
    assert!(d["request"]["body"]["text"].is_null());
    assert_eq!(d["request"]["headers"][0][0], "Accept");

    let (status, _) = call(&h.app, "GET", "/api/flows/99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn decoded_body_wins_over_raw() {
    let h = loopback();
    let mut f = flow(1, "GET", "https://a.test/", Some(200));
    let resp = f.response.as_mut().unwrap();
    resp.body.raw = Bytes::from_static(&[0x1f, 0x8b]);
    resp.body.decoded = Some(Bytes::from_static(b"hello"));
    send(&h, [f]).await;
    let d = get_json(&h.app, "/api/flows/1").await;
    assert_eq!(d["response"]["body"]["text"], "hello");
}

#[tokio::test]
async fn delete_clears_everything() {
    let h = loopback();
    send(&h, [flow(1, "GET", "https://a.test/", Some(200))]).await;
    let (status, _) = call(&h.app, "DELETE", "/api/flows").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(get_json(&h.app, "/api/flows").await["total"], 0);
    let (status, _) = call(&h.app, "GET", "/api/flows/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn har_export() {
    let h = loopback();
    send(
        &h,
        [flow(1, "GET", "https://a.test/p?x=1&y=two", Some(200))],
    )
    .await;
    let har = get_json(&h.app, "/api/flows/1/har").await;
    assert_eq!(har["log"]["version"], "1.2");
    let entries = har["log"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e["request"]["method"], "GET");
    assert_eq!(e["request"]["queryString"][1]["name"], "y");
    assert_eq!(e["response"]["status"], 200);
    assert_eq!(e["response"]["content"]["text"], "{\"ok\":true}");
    assert_eq!(e["timings"]["wait"], 10.0);
    assert_eq!(e["timings"]["receive"], 15.0);
    assert_eq!(e["startedDateTime"], "1970-01-01T00:00:00Z");
}

#[tokio::test]
async fn curl_output() {
    let h = loopback();
    let mut f = flow(1, "POST", "https://a.test/it's", Some(200));
    f.request
        .headers
        .push(("Content-Length".into(), "5".into()));
    f.request.body.raw = Bytes::from_static(b"it's");
    send(&h, [f]).await;
    let (status, body) = call(&h.app, "GET", "/api/flows/1/curl").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.starts_with("curl -X 'POST' \\\n  'https://a.test/it'\\''s'"),
        "{text}"
    );
    assert!(text.contains("-H 'Accept: */*'"));
    assert!(!text.to_lowercase().contains("content-length"));
    assert!(text.contains("--data-raw 'it'\\''s'"));
}

#[tokio::test]
async fn store_evicts_oldest() {
    let store = FlowStore::new(3);
    for i in 1..=5 {
        store.upsert(Arc::new(flow(i, "GET", "https://a.test/", Some(200))));
    }
    assert_eq!(store.len(), 3);
    assert!(store.get(2).is_none());
    // Updating a retained flow must not evict anything.
    store.upsert(Arc::new(flow(3, "GET", "https://a.test/", Some(500))));
    let (total, page) = store.list(&Default::default(), 0, 10);
    assert_eq!(total, 3);
    assert_eq!(page.iter().map(|s| s.id).collect::<Vec<_>>(), [3, 4, 5]);
}

fn lan_config(token: Option<&str>) -> UiConfig {
    UiConfig {
        listen: "0.0.0.0:8081".parse().unwrap(),
        token: token.map(str::to_string),
    }
}

fn device_routes() -> Router {
    Router::new()
        .route("/setup/ca.pem", get(|| async { "cert" }))
        .route("/proxy.pac", get(|| async { "pac" }))
}

#[tokio::test]
async fn non_loopback_requires_token_config() {
    let tx = flow::channel();
    let shutdown = CancellationToken::new();
    for token in [None, Some("")] {
        let err = build(&lan_config(token), &tx, Router::new(), &shutdown).unwrap_err();
        assert!(err.to_string().contains("ui.token"), "{err}");
    }
}

#[tokio::test]
async fn token_enforced_on_api_and_ui_but_not_setup() {
    let h = harness(lan_config(Some("s3cret")), device_routes());

    for uri in ["/api/flows", "/api/flows/1", "/", "/index.html", "/nope"] {
        let (status, _) = call(&h.app, "GET", uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, _) = call_with(
        &h.app,
        "GET",
        "/api/flows",
        &[("authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&h.app, "DELETE", "/api/flows").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    for uri in ["/setup/ca.pem", "/proxy.pac"] {
        let (status, _) = call(&h.app, "GET", uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }

    let (status, _) = call_with(
        &h.app,
        "GET",
        "/api/flows",
        &[("authorization", "Bearer s3cret")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_with(&h.app, "GET", "/api/flows", &[("x-sieve-token", "s3cret")]).await;
    assert_eq!(status, StatusCode::OK);

    // ?token= works once and hands back a cookie for later requests.
    let resp = h
        .app
        .clone()
        .oneshot(Request::get("/?token=s3cret").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    let pair = cookie.split(';').next().unwrap();
    let (status, _) = call_with(&h.app, "GET", "/api/flows", &[("cookie", pair)]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call_with(&h.app, "GET", "/api/flows", &[("cookie", "sieve_token=00")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn loopback_needs_no_token() {
    let h = harness(
        UiConfig {
            token: Some("ignored".into()),
            ..UiConfig::default()
        },
        device_routes(),
    );
    let (status, _) = call(&h.app, "GET", "/api/flows").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(&h.app, "GET", "/proxy.pac").await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"pac"[..]));
}

#[tokio::test]
async fn unknown_api_path_is_json_404_and_ui_route_serves_html() {
    let h = loopback();
    let (status, _) = call(&h.app, "GET", "/api/nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = call(&h.app, "GET", "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("<html")
            || String::from_utf8_lossy(&body).contains("<!doctype")
    );
}

#[tokio::test]
async fn websocket_streams_summaries() {
    let h = loopback();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let app = h.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    send(&h, [flow(7, "GET", "https://a.test/live", Some(200))]).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for a WebSocket message")
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(v["type"], "flow");
    assert_eq!(v["flow"]["id"], 7);
    assert_eq!(v["flow"]["host"], "a.test");
    assert_eq!(v["flow"]["status"], 200);

    let (status, _) = call(&h.app, "DELETE", "/api/flows").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(v["type"], "clear");
    h.shutdown.cancel();
}

mod extra;

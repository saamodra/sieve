use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use sieve_core::{Flow, flow};
use tower::ServiceExt;

use super::*;

const DER: &[u8] = &[0x30, 0x82, 0x01, 0x0a, 0xde, 0xad, 0xbe, 0xef];

fn app(addr: &str) -> (Router, FlowSender) {
    let flows = flow::channel();
    (build(DER.to_vec(), addr.parse().unwrap(), &flows), flows)
}

async fn get_path(app: &Router, path: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::get(path)
                .header(header::HOST, "192.168.1.5:8081")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn test_flow(url: &str, client: &str, error: Option<&str>) -> Arc<Flow> {
    let json = serde_json::json!({
        "id": 1,
        "started_at": "2026-01-01T00:00:00Z",
        "client": client,
        "kind": "http",
        "request": {
            "method": "GET", "url": url, "http_version": "HTTP/1.1",
            "headers": [],
            "body": { "size": 0, "truncated": false, "content_type": null, "content_encoding": null }
        },
        "response": null,
        "error": error,
        "timing": { "wait_ms": null, "total_ms": null }
    });
    Arc::new(serde_json::from_value(json).unwrap())
}

async fn status_ok(app: &Router) -> bool {
    let v: serde_json::Value =
        serde_json::from_str(&body_string(get_path(app, "/setup/ios/status").await).await).unwrap();
    v["ok"].as_bool().unwrap()
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[test]
fn mobileconfig_parses_with_root_payload() {
    let xml = mobileconfig(DER);
    let value = plist::Value::from_reader_xml(xml.as_bytes()).unwrap();
    let top = value.as_dictionary().unwrap();
    assert_eq!(top["PayloadType"].as_string(), Some("Configuration"));
    let inner = top["PayloadContent"].as_array().unwrap()[0]
        .as_dictionary()
        .unwrap();
    assert_eq!(
        inner["PayloadType"].as_string(),
        Some("com.apple.security.root")
    );
    assert_eq!(inner["PayloadContent"].as_data(), Some(DER));
    assert_ne!(
        top["PayloadUUID"].as_string(),
        inner["PayloadUUID"].as_string()
    );
}

#[test]
fn pac_text_is_exact() {
    let addr: SocketAddr = "192.168.1.5:8080".parse().unwrap();
    assert_eq!(
        pac(addr),
        "function FindProxyForURL(url, host) {\n  return \"PROXY 192.168.1.5:8080; DIRECT\";\n}\n"
    );
}

#[tokio::test]
async fn routes_status_codes_and_content_types() {
    let (app, _flows) = app("10.0.0.2:8080");

    let res = get_path(&app, "/setup/ios.mobileconfig").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/x-apple-aspen-config"
    );

    let res = get_path(&app, "/proxy.pac").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/x-ns-proxy-autoconfig"
    );
    assert!(
        body_string(res)
            .await
            .contains("PROXY 10.0.0.2:8080; DIRECT")
    );

    let res = get_path(&app, "/setup/ios").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = body_string(res).await;
    assert!(html.contains("/setup/ios.mobileconfig"));
    assert!(html.contains("http://192.168.1.5:8081/proxy.pac"));
    assert!(html.contains("Certificate Trust Settings"));

    let res = get_path(&app, "/setup/ios/status").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json");
}

#[tokio::test]
async fn status_flips_only_for_qualifying_flow() {
    let (app, flows) = app("10.0.0.2:8080");
    settle().await;
    assert!(!status_ok(&app).await);

    flows
        .send(test_flow("https://example.com/", "127.0.0.1:5000", None))
        .unwrap();
    flows
        .send(test_flow(
            "https://example.com/",
            "192.168.1.9:5000",
            Some("tls handshake failed"),
        ))
        .unwrap();
    flows
        .send(test_flow("http://example.com/", "192.168.1.9:5000", None))
        .unwrap();
    settle().await;
    assert!(!status_ok(&app).await);

    flows
        .send(test_flow("https://example.com/", "192.168.1.9:5000", None))
        .unwrap();
    settle().await;
    assert!(status_ok(&app).await);
}

#[test]
fn advertised_addr_keeps_specific_ip() {
    let addr: SocketAddr = "10.1.2.3:9000".parse().unwrap();
    assert_eq!(advertised_addr(addr).unwrap(), addr);
}

#[test]
fn qr_render_is_non_empty() {
    let qr = render_qr("http://192.168.1.5:8081/setup/ios").unwrap();
    assert!(!qr.trim().is_empty());
    assert!(qr.lines().count() > 5);
}

#[test]
fn random_tokens_differ() {
    let a = random_token();
    assert_eq!(a.len(), 32);
    assert_ne!(a, random_token());
}

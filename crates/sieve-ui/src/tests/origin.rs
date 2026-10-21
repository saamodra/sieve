//! Tests for the Host and Origin checks on a loopback-bound UI.

use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use super::*;

const PORT: u16 = 8731;

fn bound(addr: SocketAddr) -> Harness {
    harness(
        UiConfig {
            listen: addr,
            token: None,
        },
        device_routes(),
    )
}

fn local() -> Harness {
    bound(SocketAddr::from(([127, 0, 0, 1], PORT)))
}

#[tokio::test]
async fn foreign_host_is_refused() {
    let h = local();
    for host in [
        "evil.example",
        "evil.example:8731",
        "127.0.0.1:9999",
        "localhost",
        "localhost:",
        "127.0.0.1.evil.example:8731",
        "::1:8731",
    ] {
        let (status, body) = call_with(&h.app, "GET", "/api/flows/1", &[("host", host)]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
        assert!(
            String::from_utf8_lossy(&body).starts_with("forbidden"),
            "{host}"
        );
    }
    // The page and setup routes are covered too, not just the API.
    for uri in ["/", "/setup/ca.pem", "/proxy.pac"] {
        let (status, _) = call_with(&h.app, "GET", uri, &[("host", "evil.example")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn loopback_hosts_are_allowed() {
    let h = local();
    for host in [
        "127.0.0.1:8731",
        "localhost:8731",
        "LocalHost:8731",
        "[::1]:8731",
    ] {
        let (status, _) = call_with(&h.app, "GET", "/api/flows", &[("host", host)]).await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    // No Host at all is not a browser.
    let (status, _) = call(&h.app, "GET", "/api/flows").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn cross_origin_post_is_refused() {
    let h = local();
    for origin in [
        "http://evil.example",
        "http://localhost:3000",
        "https://127.0.0.1:8731",
        "http://127.0.0.1:8731/",
        "null",
    ] {
        for uri in ["/api/flows/1/replay", "/api/breakpoints/1/abort"] {
            let (status, body) = call_with(
                &h.app,
                "POST",
                uri,
                &[("host", "127.0.0.1:8731"), ("origin", origin)],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin} {uri}");
            assert!(String::from_utf8_lossy(&body).starts_with("forbidden"));
        }
    }
}

#[tokio::test]
async fn same_origin_and_no_origin_pass() {
    let h = local();
    for origin in [
        Some("http://127.0.0.1:8731"),
        Some("http://localhost:8731"),
        Some("http://[::1]:8731"),
        None,
    ] {
        let mut headers = vec![("host", "127.0.0.1:8731")];
        headers.extend(origin.map(|o| ("origin", o)));
        // Flow 1 does not exist, so getting past the check means a 404.
        let (status, _) = call_with(&h.app, "POST", "/api/flows/1/replay", &headers).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{origin:?}");
    }
}

#[tokio::test]
async fn cross_origin_websocket_is_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = bound(addr);
    let app = h.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let connect = |origin: String| async move {
        let mut req = format!("ws://{addr}/api/ws").into_client_request().unwrap();
        req.headers_mut().insert("origin", origin.parse().unwrap());
        tokio_tungstenite::connect_async(req).await
    };
    match connect("http://evil.example".into()).await {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
        other => panic!("expected a 403 handshake, got {:?}", other.map(|_| ())),
    }
    connect(format!("http://{addr}"))
        .await
        .expect("same-origin WebSocket");
    h.shutdown.cancel();
}

#[tokio::test]
async fn token_mode_skips_the_origin_check() {
    let h = harness(lan_config(Some("s3cret")), device_routes());
    // Phones reach setup by LAN address, from pages on any origin.
    let foreign = [
        ("host", "192.168.1.20:8081"),
        ("origin", "http://evil.example"),
    ];
    for uri in ["/setup/ca.pem", "/proxy.pac"] {
        let (status, _) = call_with(&h.app, "GET", uri, &foreign).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (status, _) = call_with(&h.app, "GET", "/api/flows", &foreign).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut with_token = foreign.to_vec();
    with_token.push(("authorization", "Bearer s3cret"));
    let (status, _) = call_with(&h.app, "GET", "/api/flows", &with_token).await;
    assert_eq!(status, StatusCode::OK);
}

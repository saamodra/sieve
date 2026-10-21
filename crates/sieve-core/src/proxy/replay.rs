//! Re-sends a captured request straight to its origin.
//!
//! Replay does not go through the proxy, so it never shows up as a second
//! capture of itself; the caller decides whether to publish the result as a new
//! [`Flow`](crate::Flow).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request as HttpRequest};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};

use super::{decode, tls};
use crate::flow::{Body, Request, Response, Timing};

/// Headers that describe the original connection or framing, and are rebuilt
/// by the HTTP client for the new one.
const SKIPPED_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
    // Asking for identity keeps the replayed body readable without decoding.
    "accept-encoding",
];

/// The result of a replay.
#[derive(Debug, Clone)]
pub struct Replayed {
    pub response: Response,
    pub timing: Timing,
}

/// Sends `req` to the server named in its URL and returns the response, with
/// the body capped at `max_body_bytes`.
pub async fn replay(req: &Request, timeout: Duration, max_body_bytes: usize) -> Result<Replayed> {
    let uri: http::Uri = req.url.parse().context("the request URL is not valid")?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        bail!("only http:// and https:// requests can be replayed");
    }
    let method = Method::from_bytes(req.method.as_bytes()).context("invalid request method")?;
    let body = req.body.decoded.as_ref().unwrap_or(&req.body.raw).clone();

    let mut builder = HttpRequest::builder().method(method).uri(uri);
    for (name, value) in &req.headers {
        if name.starts_with(':') || SKIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        // Headers that cannot be represented are dropped rather than failing
        // the whole replay.
        let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        builder = builder.header(n, v);
    }
    let http_req = builder
        .body(Full::new(body))
        .context("building the replay request")?;

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls::upstream_config(&[])?)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);

    let start = Instant::now();
    let work = async {
        let resp = client
            .request(http_req)
            .await
            .context("sending the request")?;
        let wait_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (parts, incoming) = resp.into_parts();
        let bytes = incoming
            .collect()
            .await
            .context("reading the response body")?
            .to_bytes();
        Ok::<_, anyhow::Error>((parts, bytes, wait_ms))
    };
    let (parts, bytes, wait_ms) = tokio::time::timeout(timeout, work)
        .await
        .context("the replay timed out")??;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;

    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let header = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
    };
    let content_encoding = header("content-encoding");
    let size = bytes.len() as u64;
    let truncated = bytes.len() > max_body_bytes;
    let raw = if truncated {
        bytes.slice(..max_body_bytes)
    } else {
        bytes
    };
    let decoded = match &content_encoding {
        Some(enc) => decode::decode(enc, &raw, max_body_bytes).await,
        None => None,
    };
    let http_version = if parts.version == http::Version::HTTP_2 {
        "HTTP/2.0"
    } else {
        "HTTP/1.1"
    };
    Ok(Replayed {
        response: Response {
            status: parts.status.as_u16(),
            http_version: http_version.into(),
            body: Body {
                content_type: header("content-type"),
                raw,
                decoded,
                size,
                truncated,
                content_encoding,
            },
            headers,
        },
        timing: Timing {
            wait_ms: Some(wait_ms),
            total_ms: Some(total_ms),
        },
    })
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn replays_method_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut got = Vec::new();
            while !String::from_utf8_lossy(&got).contains("hello") {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(
                b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await
            .unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });

        let req = Request {
            method: "POST".into(),
            url: format!("http://{addr}/submit?a=1"),
            http_version: "HTTP/1.1".into(),
            headers: vec![
                ("X-Test".into(), "yes".into()),
                ("Content-Length".into(), "999".into()),
            ],
            body: Body {
                raw: Bytes::from_static(b"hello"),
                size: 5,
                ..Body::default()
            },
        };
        let out = replay(&req, Duration::from_secs(5), 1024).await.unwrap();
        assert_eq!(out.response.status, 201);
        assert_eq!(out.response.body.raw, Bytes::from_static(b"ok"));
        assert_eq!(
            out.response.body.content_type.as_deref(),
            Some("text/plain")
        );
        let seen = server.await.unwrap().to_lowercase();
        assert!(seen.starts_with("post /submit?a=1 http/1.1"), "{seen}");
        assert!(seen.contains("x-test: yes"));
        assert!(seen.contains("content-length: 5"));
    }

    #[tokio::test]
    async fn rejects_non_http_urls() {
        let req = Request {
            method: "GET".into(),
            url: "example.com:443".into(),
            http_version: "HTTP/1.1".into(),
            headers: vec![],
            body: Body::default(),
        };
        assert!(replay(&req, Duration::from_secs(1), 10).await.is_err());
    }
}

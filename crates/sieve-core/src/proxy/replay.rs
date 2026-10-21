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
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};

use rustls::pki_types::CertificateDer;

use super::{decode, tls};
use crate::{
    ProxyConfig,
    flow::{Body, Request, Response, Timing},
};

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

type ReplayClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Re-sends captured requests with one shared HTTP client, so the OS trust
/// store is read once and connections are pooled across replays.
#[derive(Clone)]
pub struct Replayer {
    client: ReplayClient,
    max_body_bytes: usize,
}

impl Replayer {
    /// Trusts the built-in and OS roots plus `extra_roots`, and caps response
    /// bodies at `max_body_bytes`.
    pub fn new(extra_roots: &[CertificateDer<'static>], max_body_bytes: usize) -> Result<Self> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls::upstream_config(extra_roots)?)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            max_body_bytes,
        })
    }

    /// Builds a replayer that trusts the same extra roots as the proxy and
    /// honours its body limit.
    pub fn from_config(config: &ProxyConfig) -> Result<Self> {
        let mut roots = Vec::new();
        for path in &config.upstream_ca_files {
            roots.extend(super::load_pem_certs(path)?);
        }
        Self::new(&roots, config.max_body_bytes)
    }

    /// Sends `req` to the server named in its URL and returns the response.
    pub async fn replay(&self, req: &Request, timeout: Duration) -> Result<Replayed> {
        let max_body_bytes = self.max_body_bytes;
        let uri: http::Uri = req.url.parse().context("the request URL is not valid")?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            bail!("only http:// and https:// requests can be replayed");
        }
        if req.body.truncated {
            bail!("the captured request body was truncated, so it cannot be replayed faithfully");
        }
        let method = Method::from_bytes(req.method.as_bytes()).context("invalid request method")?;
        // The wire bytes match the captured Content-Encoding header.
        let body = req.body.raw.clone();

        let mut builder = HttpRequest::builder().method(method).uri(uri);
        for (name, value) in &req.headers {
            if name.starts_with(':')
                || SKIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            {
                continue;
            }
            // Headers that cannot be represented are dropped rather than
            // failing the whole replay.
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
        let client = &self.client;

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
}

/// Sends `req` with a one-off [`Replayer`] that trusts only the built-in and OS
/// roots. Callers that replay repeatedly should keep a [`Replayer`] instead.
pub async fn replay(req: &Request, timeout: Duration, max_body_bytes: usize) -> Result<Replayed> {
    Replayer::new(&[], max_body_bytes)?
        .replay(req, timeout)
        .await
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

#[cfg(test)]
mod replayer_tests {
    use std::sync::Arc;

    use async_compression::tokio::write::GzipEncoder;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use rustls::{ServerConfig, crypto::ring, pki_types::PrivatePkcs8KeyDer};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_rustls::TlsAcceptor;

    use super::*;

    const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

    fn get(url: String) -> Request {
        Request {
            method: "GET".into(),
            url,
            http_version: "HTTP/1.1".into(),
            headers: vec![],
            body: Body::default(),
        }
    }

    /// Serves TLS on 127.0.0.1 with a fresh CA; returns the address and the
    /// CA certificate as DER and PEM.
    async fn tls_server() -> (std::net::SocketAddr, CertificateDer<'static>, String) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 2048];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(OK).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        (addr, ca_cert.der().clone(), ca_cert.pem())
    }

    #[tokio::test]
    async fn trusts_extra_roots() {
        let (addr, ca, _) = tls_server().await;
        let req = get(format!("https://{addr}/"));

        let with_root = Replayer::new(&[ca], 1024).unwrap();
        let out = with_root
            .replay(&req, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out.response.status, 200);

        let without = Replayer::new(&[], 1024).unwrap();
        assert!(without.replay(&req, Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn from_config_loads_upstream_ca_files() {
        let (addr, _, pem) = tls_server().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.pem");
        std::fs::write(&path, pem).unwrap();
        let config = ProxyConfig {
            upstream_ca_files: vec![path],
            ..ProxyConfig::default()
        };
        let out = Replayer::from_config(&config)
            .unwrap()
            .replay(&get(format!("https://{addr}/")), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out.response.status, 200);
    }

    #[tokio::test]
    async fn from_config_applies_the_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .unwrap();
        });
        let config = ProxyConfig {
            max_body_bytes: 4,
            ..ProxyConfig::default()
        };
        let out = Replayer::from_config(&config)
            .unwrap()
            .replay(&get(format!("http://{addr}/")), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(out.response.body.truncated);
        assert_eq!(out.response.body.raw, Bytes::from_static(b"0123"));
    }

    #[tokio::test]
    async fn gzip_request_body_is_sent_as_captured() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            let (head_end, len) = loop {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while got.len() < head_end + len {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(OK).await.unwrap();
            got[head_end..].to_vec()
        });

        let plain = b"{\"hello\":\"gzip\"}";
        let mut enc = GzipEncoder::new(Vec::new());
        enc.write_all(plain).await.unwrap();
        enc.shutdown().await.unwrap();
        let wire = Bytes::from(enc.into_inner());
        let mut req = get(format!("http://{addr}/"));
        req.method = "POST".into();
        req.headers = vec![("Content-Encoding".into(), "gzip".into())];
        req.body = Body {
            raw: wire.clone(),
            decoded: Some(Bytes::from_static(plain)),
            size: wire.len() as u64,
            content_encoding: Some("gzip".into()),
            ..Body::default()
        };
        replay(&req, Duration::from_secs(5), 1024).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, wire.to_vec());
        let decoded = decode::decode("gzip", &received, 1024).await.unwrap();
        assert_eq!(decoded, Bytes::from_static(plain));
    }

    #[tokio::test]
    async fn refuses_a_truncated_request_body() {
        let mut req = get("http://127.0.0.1:9/".into());
        req.method = "POST".into();
        req.body = Body {
            raw: Bytes::from_static(b"cut"),
            truncated: true,
            size: 100,
            ..Body::default()
        };
        let err = replay(&req, Duration::from_secs(1), 1024)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("truncated"), "{err:#}");
    }
}

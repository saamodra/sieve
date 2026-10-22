//! CONNECT handling: passthrough tunnels, protocol sniffing, and TLS
//! interception.

use std::{
    io,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use bytes::Bytes;
use http::{
    HeaderMap, Request, Response, StatusCode, Version,
    uri::{Authority, Scheme},
};
use hyper::{body::Incoming, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use rustls::{AlertDescription, server::Acceptor};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::LazyConfigAcceptor;
use tracing::debug;

use super::{
    Conn, ProxyBody, Tunnel,
    capture::{Captured, Exchange, RequestHead},
    empty_body,
    io::{Counted, Rewind},
    serve_http, text_response, tls,
};
use crate::flow::FlowKind;

/// How long to wait for the client to speak first before treating the tunnel
/// as an opaque protocol where the server speaks first.
const SNIFF_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a raw tunnel's target to accept the connection. Matches
/// the connect timeout of the HTTP upstream client in `proxy/mod.rs`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct ConnectHead {
    pub authority: Authority,
    pub version: Version,
    pub headers: HeaderMap,
}

impl ConnectHead {
    fn port(&self) -> u16 {
        self.authority.port_u16().unwrap_or(443)
    }

    /// `host:port`, which is also the flow URL for CONNECT-level flows.
    fn addr(&self) -> String {
        format!("{}:{}", self.authority.host(), self.port())
    }

    fn exchange(&self, conn: &Conn, kind: FlowKind) -> Arc<Exchange> {
        conn.shared.exchange(
            conn.client,
            conn.quiet,
            kind,
            RequestHead {
                method: "CONNECT",
                url: self.addr(),
                version: self.version,
                headers: &self.headers,
            },
        )
    }

    /// Where requests inside the tunnel go: the SNI name if the client sent
    /// one (it may have CONNECTed by IP), else the CONNECT target. Default
    /// ports are dropped so flow URLs read naturally.
    fn tunnel(&self, scheme: Scheme, sni: Option<&str>) -> Option<Arc<Tunnel>> {
        let host = sni.unwrap_or(self.authority.host());
        let port = self.port();
        let default_port = if scheme == Scheme::HTTPS { 443 } else { 80 };
        let authority = if port == default_port {
            host.to_string()
        } else {
            format!("{host}:{port}")
        };
        Some(Arc::new(Tunnel {
            scheme,
            authority: authority.parse().ok()?,
            vetted: sni.is_none_or(|sni| sni.eq_ignore_ascii_case(self.authority.host())),
            requests: Default::default(),
        }))
    }
}

/// Connects to `addr`, failing with a "timed out" message once `limit` passes
/// instead of waiting for the OS default.
async fn connect(addr: &str, limit: Duration) -> Result<TcpStream, String> {
    match tokio::time::timeout(limit, TcpStream::connect(addr)).await {
        Ok(Ok(server)) => Ok(server),
        Ok(Err(e)) => Err(format!("connecting to {addr} failed: {e}")),
        Err(_) => Err(format!(
            "connecting to {addr} timed out after {}s",
            limit.as_secs_f64()
        )),
    }
}

/// Answers a CONNECT that the loopback guard refused with a `403`, recorded
/// as a flow with an error.
pub(super) fn refuse(conn: &Conn, head: &ConnectHead, message: String) -> Response<ProxyBody> {
    let exchange = head.exchange(conn, FlowKind::Http);
    exchange.emit_start();
    let res = text_response(StatusCode::FORBIDDEN, &message);
    exchange.set_response(StatusCode::FORBIDDEN.as_u16(), head.version, res.headers());
    exchange.finish(None, Some(format!("refused: {message}")));
    res
}

/// Connects to the target first, so a dead target turns into a 502 for the
/// client, then relays bytes without looking at them.
pub(super) async fn passthrough(
    conn: Conn,
    req: Request<Incoming>,
    head: ConnectHead,
) -> Response<ProxyBody> {
    let exchange = head.exchange(&conn, FlowKind::Passthrough);
    exchange.emit_start();
    let server = match connect(&head.addr(), CONNECT_TIMEOUT).await {
        Ok(server) => server,
        Err(message) => {
            exchange.finish(None, Some(message.clone()));
            return text_response(StatusCode::BAD_GATEWAY, &message);
        }
    };
    exchange.set_response(StatusCode::OK.as_u16(), head.version, &HeaderMap::new());
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => relay(&conn, TokioIo::new(upgraded), server, &exchange).await,
            Err(e) => exchange.finish(None, Some(format!("CONNECT upgrade failed: {e}"))),
        }
    });
    Response::new(empty_body())
}

/// Sniffs the first byte of an accepted tunnel: TLS is intercepted, plain
/// HTTP is proxied, anything else is relayed as is.
pub(super) async fn intercept(conn: Conn, upgraded: Upgraded, head: ConnectHead) {
    let mut io = TokioIo::new(upgraded);
    let mut first = [0u8; 1];
    let n = match tokio::time::timeout(SNIFF_TIMEOUT, io.read(&mut first)).await {
        Ok(Ok(0)) => return,
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            debug!("reading from the tunnel to {} failed: {e}", head.addr());
            return;
        }
        Err(_) => 0,
    };
    let io = Rewind::new(Bytes::copy_from_slice(&first[..n]), io);
    match first[..n] {
        // A TLS handshake record.
        [0x16] => intercept_tls(conn, io, head).await,
        // An HTTP method.
        [b'A'..=b'Z'] => match head.tunnel(Scheme::HTTP, None) {
            Some(tunnel) => serve_http(conn.in_tunnel(tunnel), TokioIo::new(io)).await,
            None => debug!("invalid CONNECT target {}", head.addr()),
        },
        _ => {
            let exchange = head.exchange(&conn, FlowKind::Passthrough);
            exchange.emit_start();
            match connect(&head.addr(), CONNECT_TIMEOUT).await {
                Ok(server) => {
                    exchange.set_response(StatusCode::OK.as_u16(), head.version, &HeaderMap::new());
                    relay(&conn, io, server, &exchange).await;
                }
                Err(message) => exchange.finish(None, Some(message)),
            }
        }
    }
}

async fn intercept_tls<I>(conn: Conn, io: I, head: ConnectHead)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let start = match LazyConfigAcceptor::new(Acceptor::default(), io).await {
        Ok(start) => start,
        Err(e) => {
            return tls_failed(
                &conn,
                &head,
                format!("reading the TLS ClientHello failed: {e}"),
            );
        }
    };
    let sni = start.client_hello().server_name().map(str::to_string);
    let host = sni.as_deref().unwrap_or(head.authority.host());
    let config = match tls::cached_server_config(&conn.shared.ca, host) {
        Ok(config) => config,
        Err(e) => return tls_failed(&conn, &head, format!("no certificate for {host}: {e:#}")),
    };
    let stream = match start.into_stream(config).await {
        Ok(stream) => stream,
        Err(e) => return tls_failed(&conn, &head, handshake_error(&e)),
    };
    let Some(tunnel) = head.tunnel(Scheme::HTTPS, sni.as_deref()) else {
        return debug!("invalid CONNECT target {}", head.addr());
    };

    serve_http(conn.in_tunnel(tunnel.clone()), TokioIo::new(stream)).await;

    if tunnel.requests.load(Ordering::Relaxed) == 0 && !conn.shared.shutdown.is_cancelled() {
        tls_failed(
            &conn,
            &head,
            "the client closed the connection after the TLS handshake without sending a \
             request; it may pin its certificates (or it was an unused preconnect)"
                .to_string(),
        );
    }
}

/// Records a tunnel that never carried a request, as a CONNECT flow with
/// `error` set and no response.
fn tls_failed(conn: &Conn, head: &ConnectHead, message: String) {
    debug!("TLS with {} failed: {message}", head.addr());
    head.exchange(conn, FlowKind::Http)
        .finish(None, Some(message));
}

fn handshake_error(e: &io::Error) -> String {
    let rejected = e
        .get_ref()
        .and_then(|e| e.downcast_ref::<rustls::Error>())
        .is_some_and(|e| {
            matches!(
                e,
                rustls::Error::AlertReceived(
                    AlertDescription::UnknownCA
                        | AlertDescription::BadCertificate
                        | AlertDescription::CertificateUnknown
                        | AlertDescription::UnsupportedCertificate
                        | AlertDescription::AccessDenied
                )
            )
        });
    let hint = if rejected {
        " (the client does not trust the sieve CA, or pins its certificates)"
    } else {
        ""
    };
    format!("TLS handshake with the client failed: {e}{hint}")
}

/// Copies bytes both ways until either side closes, then completes the flow
/// with the byte counts (client to server as the request size).
async fn relay<C>(conn: &Conn, client: C, mut server: TcpStream, exchange: &Exchange)
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut client = Counted::new(client);
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut server) => {
            if let Err(e) = result {
                debug!("tunnel ended: {e}");
            }
        }
        () = conn.shared.shutdown.cancelled() => return,
    }
    let up = client.read.load(Ordering::Relaxed);
    let down = client.written.load(Ordering::Relaxed);
    exchange.request_body_done(Captured::opaque(up), None);
    exchange.finish(Some(Captured::opaque(down)), None);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn connect_times_out_against_a_blackholed_address() {
        let start = Instant::now();
        let err = connect("10.255.255.1:81", Duration::from_millis(50))
            .await
            .unwrap_err();
        // Some sandboxes reject unroutable addresses outright; either way it
        // must fail fast, and a timeout must say so.
        assert!(start.elapsed() < Duration::from_secs(2), "{err}");
        assert!(err.contains("timed out") || err.contains("failed"), "{err}");
    }

    #[tokio::test]
    async fn connect_reaches_a_listening_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(connect(&addr, Duration::from_secs(5)).await.is_ok());
    }

    #[tokio::test]
    async fn connect_reports_refused_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let err = connect(&addr, Duration::from_secs(5)).await.unwrap_err();
        assert!(err.contains("failed"), "{err}");
    }
}

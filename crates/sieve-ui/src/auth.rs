//! Token check for UIs exposed beyond loopback.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

const COOKIE: &str = "sieve_token";

/// Paths a phone must reach before it can hold a token.
fn is_public(path: &str) -> bool {
    path == "/proxy.pac" || path == "/setup" || path.starts_with("/setup/")
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if b.len() >= i + 3 => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

enum Presented {
    /// Valid token given via `?token=`; remember it in a cookie.
    Query,
    Other,
}

fn check(req: &Request, token: &str) -> Option<Presented> {
    let headers = req.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let custom = headers.get("x-sieve-token").and_then(|v| v.to_str().ok());
    if bearer.is_some_and(|t| ct_eq(t, token)) || custom.is_some_and(|t| ct_eq(t, token)) {
        return Some(Presented::Other);
    }
    let want_cookie = hex(token);
    let cookie_ok = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && ct_eq(v, &want_cookie));
    if cookie_ok {
        return Some(Presented::Other);
    }
    let query_ok = req
        .uri()
        .query()
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| k == "token" && ct_eq(&percent_decode(v), token));
    query_ok.then_some(Presented::Query)
}

pub async fn require_token(
    State(token): State<std::sync::Arc<str>>,
    req: Request,
    next: Next,
) -> Response {
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    match check(&req, &token) {
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: open the UI with ?token=<token> or send Authorization: Bearer <token>\n",
        )
            .into_response(),
        Some(Presented::Other) => next.run(req).await,
        Some(Presented::Query) => {
            let mut resp = next.run(req).await;
            let cookie = format!(
                "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
                hex(&token)
            );
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().append(header::SET_COOKIE, v);
            }
            resp
        }
    }
}

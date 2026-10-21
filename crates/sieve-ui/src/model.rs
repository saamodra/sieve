//! JSON views of a [`Flow`]: list summaries, full detail, HAR, and curl.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use bytes::Bytes;
use serde::Serialize;
use serde_json::{Value, json};
use sieve_core::{
    Flow,
    flow::{Body, FlowId, FlowKind},
};
use time::format_description::well_known::Rfc3339;

/// One row of the request table.
#[derive(Debug, Clone, Serialize)]
pub struct FlowSummary {
    pub id: FlowId,
    pub started_at: String,
    pub client: Option<String>,
    pub kind: FlowKind,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    pub status: Option<u16>,
    /// Response body size on the wire.
    pub size: Option<u64>,
    pub duration_ms: Option<f64>,
    pub content_type: Option<String>,
    pub error: Option<String>,
}

impl From<&Flow> for FlowSummary {
    fn from(f: &Flow) -> Self {
        let (host, path) = split_url(&f.request.url);
        let resp = f.response.as_ref();
        Self {
            id: f.id,
            started_at: rfc3339(f),
            client: f.client.map(|c| c.ip().to_string()),
            kind: f.kind,
            method: f.request.method.clone(),
            url: f.request.url.clone(),
            host,
            path,
            status: resp.map(|r| r.status),
            size: resp.map(|r| r.body.size),
            duration_ms: f.timing.total_ms,
            content_type: resp.and_then(|r| r.body.content_type.clone()),
            error: f.error.clone(),
        }
    }
}

fn rfc3339(f: &Flow) -> String {
    f.started_at.format(&Rfc3339).unwrap_or_default()
}

/// Splits `scheme://host[:port]/path?query` into `(host, path?query)`.
/// CONNECT-style targets (`host:443`) have no scheme and an empty path.
pub fn split_url(url: &str) -> (String, String) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    (authority.to_string(), path)
}

/// The bytes to show for a body: content-decoded when available.
fn display_bytes(body: &Body) -> &Bytes {
    body.decoded.as_ref().unwrap_or(&body.raw)
}

fn body_json(body: &Body) -> Value {
    let bytes = display_bytes(body);
    json!({
        "size": body.size,
        "truncated": body.truncated,
        "content_type": body.content_type,
        "content_encoding": body.content_encoding,
        "captured": bytes.len(),
        "base64": B64.encode(bytes),
        "text": std::str::from_utf8(bytes).ok(),
    })
}

fn headers_json(headers: &[(String, String)]) -> Value {
    headers.iter().map(|(k, v)| json!([k, v])).collect()
}

/// Full flow including bodies: base64 always, plus `text` when valid UTF-8.
pub fn detail(f: &Flow) -> Value {
    json!({
        "summary": FlowSummary::from(f),
        "request": {
            "method": f.request.method,
            "url": f.request.url,
            "http_version": f.request.http_version,
            "headers": headers_json(&f.request.headers),
            "body": body_json(&f.request.body),
        },
        "response": f.response.as_ref().map(|r| json!({
            "status": r.status,
            "http_version": r.http_version,
            "headers": headers_json(&r.headers),
            "body": body_json(&r.body),
        })),
        "error": f.error,
        "timing": f.timing,
    })
}

fn har_headers(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect()
}

fn query_string(url: &str) -> Vec<Value> {
    let Some((_, query)) = url.split_once('?') else {
        return Vec::new();
    };
    let query = query.split('#').next().unwrap_or_default();
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            json!({"name": k, "value": v})
        })
        .collect()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// HAR 1.2 `entry` for one flow.
pub fn har_entry(f: &Flow) -> Value {
    let req = &f.request;
    let req_bytes = display_bytes(&req.body);
    let post_data = (!req_bytes.is_empty()).then(|| {
        let mime = req
            .body
            .content_type
            .clone()
            .or_else(|| header(&req.headers, "content-type").map(str::to_string))
            .unwrap_or_default();
        json!({
            "mimeType": mime,
            "text": String::from_utf8_lossy(req_bytes),
        })
    });
    let mut request = json!({
        "method": req.method,
        "url": req.url,
        "httpVersion": req.http_version,
        "cookies": [],
        "headers": har_headers(&req.headers),
        "queryString": query_string(&req.url),
        "headersSize": -1,
        "bodySize": req.body.size,
    });
    if let Some(pd) = post_data {
        request["postData"] = pd;
    }

    let response = match &f.response {
        Some(r) => {
            let bytes = display_bytes(&r.body);
            let mut content = json!({
                "size": bytes.len(),
                "mimeType": r.body.content_type.clone().unwrap_or_default(),
            });
            match std::str::from_utf8(bytes) {
                Ok(text) => content["text"] = json!(text),
                Err(_) => {
                    content["text"] = json!(B64.encode(bytes));
                    content["encoding"] = json!("base64");
                }
            }
            json!({
                "status": r.status,
                "statusText": "",
                "httpVersion": r.http_version,
                "cookies": [],
                "headers": har_headers(&r.headers),
                "content": content,
                "redirectURL": header(&r.headers, "location").unwrap_or_default(),
                "headersSize": -1,
                "bodySize": r.body.size,
            })
        }
        None => json!({
            "status": 0,
            "statusText": f.error.clone().unwrap_or_default(),
            "httpVersion": req.http_version,
            "cookies": [],
            "headers": [],
            "content": {"size": 0, "mimeType": ""},
            "redirectURL": "",
            "headersSize": -1,
            "bodySize": -1,
        }),
    };

    let wait = f.timing.wait_ms.unwrap_or(0.0);
    let total = f.timing.total_ms.unwrap_or(wait);
    json!({
        "startedDateTime": rfc3339(f),
        "time": total,
        "request": request,
        "response": response,
        "cache": {},
        "timings": {"send": 0, "wait": wait, "receive": (total - wait).max(0.0)},
        "serverIPAddress": "",
        "comment": format!("sieve flow {}", f.id),
    })
}

/// HAR 1.2 document for the given flows.
pub fn har<'a>(flows: impl IntoIterator<Item = &'a Flow>) -> Value {
    json!({
        "log": {
            "version": "1.2",
            "creator": {"name": "sieve", "version": env!("CARGO_PKG_VERSION")},
            "entries": flows.into_iter().map(har_entry).collect::<Vec<_>>(),
        }
    })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A `curl` command reproducing the request. Binary bodies are piped in from
/// base64 so the command stays copy-pasteable.
pub fn curl(f: &Flow) -> String {
    let req = &f.request;
    let mut parts = Vec::new();
    if req.method != "GET" {
        parts.push(format!("-X {}", shell_quote(&req.method)));
    }
    parts.push(shell_quote(&req.url));
    for (k, v) in &req.headers {
        if k.starts_with(':') || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        parts.push(format!("-H {}", shell_quote(&format!("{k}: {v}"))));
    }
    let body = display_bytes(&req.body);
    let join = |parts: &[String]| format!("curl {}", parts.join(" \\\n  "));
    if body.is_empty() {
        return join(&parts);
    }
    match std::str::from_utf8(body) {
        Ok(text) => {
            parts.push(format!("--data-raw {}", shell_quote(text)));
            join(&parts)
        }
        Err(_) => {
            parts.push("--data-binary @-".to_string());
            format!(
                "echo {} | base64 -d | {}",
                shell_quote(&B64.encode(body)),
                join(&parts)
            )
        }
    }
}

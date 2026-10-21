//! HAR 1.2 import: turns `log.entries` back into [`Flow`]s.

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use bytes::Bytes;
use serde_json::Value;
use sieve_core::{
    Flow,
    flow::{Body, FlowKind, Request, Response, Timing},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Parses a HAR document. `next_id` assigns each flow its id. Entries that
/// cannot be understood fail the whole import, naming the entry.
pub fn parse(har: &Value, mut next_id: impl FnMut() -> u64) -> Result<Vec<Flow>> {
    let entries = har
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .context("not a HAR file: log.entries is missing")?;
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| entry(e, next_id()).with_context(|| format!("HAR entry {i}")))
        .collect()
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn headers(v: &Value) -> Vec<(String, String)> {
    v.get("headers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|h| {
            (
                str_of(h, "name").to_string(),
                str_of(h, "value").to_string(),
            )
        })
        .collect()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn body(bytes: Bytes, content_type: Option<String>) -> Body {
    Body {
        size: bytes.len() as u64,
        raw: bytes,
        content_type,
        ..Body::default()
    }
}

fn entry(e: &Value, id: u64) -> Result<Flow> {
    let req = e.get("request").context("no request")?;
    let url = str_of(req, "url");
    if url.is_empty() {
        bail!("request has no url");
    }
    let req_headers = headers(req);
    let req_body = req.get("postData").map_or_else(Body::default, |pd| {
        let mime = Some(str_of(pd, "mimeType").to_string()).filter(|m| !m.is_empty());
        body(Bytes::copy_from_slice(str_of(pd, "text").as_bytes()), mime)
    });
    let request = Request {
        method: str_of(req, "method").to_ascii_uppercase(),
        url: url.to_string(),
        http_version: Some(str_of(req, "httpVersion"))
            .filter(|v| !v.is_empty())
            .unwrap_or("HTTP/1.1")
            .to_string(),
        headers: req_headers,
        body: req_body,
    };

    let mut error = None;
    let response = match e.get("response") {
        Some(r) if r.get("status").and_then(Value::as_u64).unwrap_or(0) != 0 => {
            let content = r.get("content").unwrap_or(&Value::Null);
            let text = str_of(content, "text");
            let bytes = if str_of(content, "encoding") == "base64" {
                Bytes::from(
                    B64.decode(text)
                        .context("response content is not valid base64")?,
                )
            } else {
                Bytes::copy_from_slice(text.as_bytes())
            };
            let hdrs = headers(r);
            let mime = Some(str_of(content, "mimeType").to_string())
                .filter(|m| !m.is_empty())
                .or_else(|| header(&hdrs, "content-type").map(str::to_string));
            Some(Response {
                status: u16::try_from(r["status"].as_u64().unwrap_or(0))
                    .context("status out of range")?,
                http_version: Some(str_of(r, "httpVersion"))
                    .filter(|v| !v.is_empty())
                    .unwrap_or("HTTP/1.1")
                    .to_string(),
                headers: hdrs,
                body: body(bytes, mime),
            })
        }
        Some(r) => {
            error = Some(str_of(r, "statusText").to_string()).filter(|s| !s.is_empty());
            None
        }
        None => None,
    };

    let started_at = OffsetDateTime::parse(str_of(e, "startedDateTime"), &Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    let wait = e.pointer("/timings/wait").and_then(Value::as_f64);
    let total = e.get("time").and_then(Value::as_f64).filter(|t| *t >= 0.0);
    Ok(Flow {
        id,
        started_at,
        client: None,
        kind: FlowKind::Http,
        request,
        response,
        error,
        timing: Timing {
            wait_ms: wait.filter(|w| *w >= 0.0),
            total_ms: total,
        },
    })
}

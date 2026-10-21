//! Comparing the responses of two flows.

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Value, json};
use sieve_core::{Flow, flow::Response};

/// Above this many line pairs the body diff stops being a minimal diff and
/// reports the differing middle as one removal plus one addition.
const MAX_LCS_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Equal,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Line {
    pub op: Op,
    pub text: String,
}

/// Line-based diff of `a` into `b`.
pub fn diff_lines(a: &str, b: &str) -> Vec<Line> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let line = |op, t: &str| Line {
        op,
        text: t.to_string(),
    };
    let mut out: Vec<Line> = a[..prefix].iter().map(|t| line(Op::Equal, t)).collect();
    if ma.len().saturating_mul(mb.len()) > MAX_LCS_CELLS {
        out.extend(ma.iter().map(|t| line(Op::Del, t)));
        out.extend(mb.iter().map(|t| line(Op::Add, t)));
    } else {
        // lcs[i][j] = length of the LCS of ma[i..] and mb[j..].
        let w = mb.len() + 1;
        let mut lcs = vec![0u32; (ma.len() + 1) * w];
        for i in (0..ma.len()).rev() {
            for j in (0..mb.len()).rev() {
                lcs[i * w + j] = if ma[i] == mb[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < ma.len() && j < mb.len() {
            if ma[i] == mb[j] {
                out.push(line(Op::Equal, ma[i]));
                i += 1;
                j += 1;
            } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
                out.push(line(Op::Del, ma[i]));
                i += 1;
            } else {
                out.push(line(Op::Add, mb[j]));
                j += 1;
            }
        }
        out.extend(ma[i..].iter().map(|t| line(Op::Del, t)));
        out.extend(mb[j..].iter().map(|t| line(Op::Add, t)));
    }
    out.extend(a[a.len() - suffix..].iter().map(|t| line(Op::Equal, t)));
    out
}

fn body_bytes(r: &Response) -> &Bytes {
    r.body.decoded.as_ref().unwrap_or(&r.body.raw)
}

fn header_diff(a: &Response, b: &Response) -> Value {
    let find = |r: &Response, name: &str| -> Vec<String> {
        r.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .collect()
    };
    let mut names: Vec<String> = Vec::new();
    for (k, _) in a.headers.iter().chain(&b.headers) {
        let k = k.to_ascii_lowercase();
        if !names.contains(&k) {
            names.push(k);
        }
    }
    let (mut added, mut removed, mut changed) = (vec![], vec![], vec![]);
    for name in &names {
        let (va, vb) = (find(a, name), find(b, name));
        if va == vb {
            continue;
        }
        let entry = json!({"name": name, "a": va, "b": vb});
        match (va.is_empty(), vb.is_empty()) {
            (true, _) => added.push(entry),
            (_, true) => removed.push(entry),
            _ => changed.push(entry),
        }
    }
    json!({"added": added, "removed": removed, "changed": changed})
}

/// Differences between the responses of flows `a` and `b`. `None` when either
/// has no response.
pub fn responses(a: &Flow, b: &Flow) -> Option<Value> {
    let (ra, rb) = (a.response.as_ref()?, b.response.as_ref()?);
    let (ba, bb) = (body_bytes(ra), body_bytes(rb));
    let body = match (std::str::from_utf8(ba), std::str::from_utf8(bb)) {
        (Ok(ta), Ok(tb)) => json!({
            "kind": "text",
            "identical": ta == tb,
            "lines": diff_lines(ta, tb),
        }),
        _ => {
            json!({"kind": "binary", "identical": ba == bb, "size_a": ba.len(), "size_b": bb.len()})
        }
    };
    Some(json!({
        "a": a.id,
        "b": b.id,
        "status": {"a": ra.status, "b": rb.status, "changed": ra.status != rb.status},
        "headers": header_diff(ra, rb),
        "body": body,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(d: &[Line]) -> String {
        d.iter()
            .map(|l| match l.op {
                Op::Equal => format!(" {}", l.text),
                Op::Add => format!("+{}", l.text),
                Op::Del => format!("-{}", l.text),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn line_diff_is_minimal() {
        let d = diff_lines("a\nb\nc\nd\n", "a\nc\nx\nd\n");
        assert_eq!(render(&d), " a|-b| c|+x| d");
    }

    #[test]
    fn identical_and_empty_inputs() {
        assert_eq!(render(&diff_lines("a\nb", "a\nb")), " a| b");
        assert_eq!(render(&diff_lines("", "")), "");
        assert_eq!(render(&diff_lines("", "x")), "+x");
        assert_eq!(render(&diff_lines("x", "")), "-x");
    }
}

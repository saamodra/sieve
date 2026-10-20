//! Bounded in-memory flow storage, fed from the proxy's [`FlowSender`].

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use sieve_core::{Flow, flow::FlowId};

use crate::model::FlowSummary;

/// Default cap on the number of retained flows.
pub const DEFAULT_MAX_FLOWS: usize = 10_000;

/// Filters accepted by `GET /api/flows`. Empty fields match everything.
#[derive(Debug, Default, Clone)]
pub struct FlowFilter {
    /// Case-insensitive substring of the host.
    pub host: Option<String>,
    /// Comma-separated list of methods, e.g. `GET,POST`.
    pub method: Option<String>,
    /// `200`, `2xx`, `err` (failed or no response), or a comma-separated list.
    pub status: Option<String>,
    /// Case-insensitive substring of the URL, method or status.
    pub q: Option<String>,
    /// Client IP address, exact match.
    pub client: Option<String>,
}

impl FlowFilter {
    pub fn matches(&self, s: &FlowSummary) -> bool {
        if let Some(host) = non_empty(&self.host)
            && !contains_ci(&s.host, host)
        {
            return false;
        }
        if let Some(methods) = non_empty(&self.method)
            && !methods
                .split(',')
                .any(|m| m.trim().eq_ignore_ascii_case(&s.method))
        {
            return false;
        }
        if let Some(status) = non_empty(&self.status)
            && !status.split(',').any(|p| status_matches(p.trim(), s))
        {
            return false;
        }
        if let Some(client) = non_empty(&self.client)
            && s.client.as_deref() != Some(client)
        {
            return false;
        }
        if let Some(q) = non_empty(&self.q) {
            let status = s.status.map(|c| c.to_string()).unwrap_or_default();
            let hit = contains_ci(&s.url, q)
                || contains_ci(&s.method, q)
                || status.contains(q)
                || s.error.as_deref().is_some_and(|e| contains_ci(e, q))
                || s.content_type.as_deref().is_some_and(|c| contains_ci(c, q));
            if !hit {
                return false;
            }
        }
        true
    }
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn status_matches(pattern: &str, s: &FlowSummary) -> bool {
    match pattern.to_ascii_lowercase().as_str() {
        "" => false,
        "err" | "error" => s.error.is_some() || s.status.is_none(),
        p => match (s.status, p.as_bytes()) {
            (Some(code), [d, b'x', b'x']) => code / 100 == u16::from(d.wrapping_sub(b'0')),
            (Some(code), _) => p.parse::<u16>().is_ok_and(|want| want == code),
            (None, _) => false,
        },
    }
}

struct Inner {
    order: VecDeque<FlowId>,
    flows: HashMap<FlowId, Arc<Flow>>,
}

/// Flows keyed by id in arrival order. Once `max_flows` is reached the oldest
/// flow is evicted for each new one.
pub struct FlowStore {
    max_flows: usize,
    inner: Mutex<Inner>,
}

impl FlowStore {
    pub fn new(max_flows: usize) -> Self {
        Self {
            max_flows: max_flows.max(1),
            inner: Mutex::new(Inner {
                order: VecDeque::new(),
                flows: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inserts a new flow or replaces the one with the same id, keeping its
    /// position in the arrival order.
    pub fn upsert(&self, flow: Arc<Flow>) {
        let mut inner = self.lock();
        if inner.flows.insert(flow.id, flow.clone()).is_none() {
            inner.order.push_back(flow.id);
            while inner.order.len() > self.max_flows {
                if let Some(old) = inner.order.pop_front() {
                    inner.flows.remove(&old);
                }
            }
        }
    }

    pub fn get(&self, id: FlowId) -> Option<Arc<Flow>> {
        self.lock().flows.get(&id).cloned()
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.order.clear();
        inner.flows.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().order.len()
    }

    /// Summaries matching `filter` in arrival order: returns the total number
    /// of matches and the `offset..offset + limit` window of them.
    pub fn list(
        &self,
        filter: &FlowFilter,
        offset: usize,
        limit: usize,
    ) -> (usize, Vec<FlowSummary>) {
        let inner = self.lock();
        let mut total = 0;
        let mut page = Vec::new();
        for id in &inner.order {
            let summary = FlowSummary::from(inner.flows[id].as_ref());
            if !filter.matches(&summary) {
                continue;
            }
            if total >= offset && page.len() < limit {
                page.push(summary);
            }
            total += 1;
        }
        (total, page)
    }
}

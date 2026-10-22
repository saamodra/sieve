//! Bounded in-memory flow storage, fed from the proxy's [`FlowSender`].

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use sieve_core::{Flow, flow::FlowId};

use crate::{api::SYNTHETIC_ID_BASE, model::FlowSummary};

/// Default cap on the number of retained flows.
pub const DEFAULT_MAX_FLOWS: usize = 10_000;

/// Default cap on the body bytes retained across all flows. The count cap
/// alone allows tens of GB, since one flow can hold several MiB of bodies. A
/// gigabyte keeps long sessions of ordinary API traffic fully retained while
/// leaving headroom on a laptop for the browser tab and the proxy itself.
pub const DEFAULT_MAX_BYTES: usize = 1 << 30;

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
    /// Sum of [`flow_bytes`] over the retained flows.
    bytes: usize,
    /// Highest id evicted so far, per id range (see [`range_of`]). A flow with
    /// an id at or below it that is not retained was evicted, and an update
    /// for it is dropped instead of re-inserting it as the newest flow.
    evicted_up_to: [Option<FlowId>; 2],
}

/// Proxy flows and UI-created flows (replay, HAR import) are numbered by
/// separate counters, so each range keeps its own eviction watermark.
fn range_of(id: FlowId) -> usize {
    usize::from(id >= SYNTHETIC_ID_BASE)
}

/// Body bytes a flow references: raw and decoded copies of both bodies. A
/// `Bytes` shared with another flow is counted in full, so this errs high.
fn flow_bytes(flow: &Flow) -> usize {
    let body = |b: &sieve_core::flow::Body| b.raw.len() + b.decoded.as_ref().map_or(0, |d| d.len());
    body(&flow.request.body) + flow.response.as_ref().map_or(0, |r| body(&r.body))
}

/// Marks a flow whose final update never reached the store.
pub const LOST_UPDATE_ERROR: &str = "final update lost: the web UI fell behind the proxy";

/// Flows keyed by id in arrival order. The oldest flows are evicted while
/// there are more than `max_flows` of them or they hold more than `max_bytes`
/// of body data. The flow just stored is never evicted, so a single flow over
/// the byte budget is still kept.
pub struct FlowStore {
    max_flows: usize,
    max_bytes: usize,
    inner: Mutex<Inner>,
}

impl FlowStore {
    pub fn new(max_flows: usize) -> Self {
        Self::with_limits(max_flows, DEFAULT_MAX_BYTES)
    }

    pub fn with_limits(max_flows: usize, max_bytes: usize) -> Self {
        Self {
            max_flows: max_flows.max(1),
            max_bytes,
            inner: Mutex::new(Inner {
                order: VecDeque::new(),
                flows: HashMap::new(),
                bytes: 0,
                evicted_up_to: [None; 2],
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inserts a new flow or replaces the one with the same id, keeping its
    /// position in the arrival order. An update for a flow that was already
    /// evicted is dropped.
    pub fn upsert(&self, flow: Arc<Flow>) {
        let mut inner = self.lock();
        let id = flow.id;
        let size = flow_bytes(&flow);
        if let Some(slot) = inner.flows.get_mut(&id) {
            let old = flow_bytes(slot);
            *slot = flow;
            inner.bytes = inner.bytes - old + size;
        } else {
            if inner.evicted_up_to[range_of(id)].is_some_and(|evicted| id <= evicted) {
                return;
            }
            inner.flows.insert(id, flow);
            inner.order.push_back(id);
            inner.bytes += size;
        }
        // An update can grow a flow past the budget just like an insert.
        while inner.order.len() > self.max_flows || inner.bytes > self.max_bytes {
            if inner.order.front() == Some(&id) {
                break;
            }
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            if let Some(gone) = inner.flows.remove(&old) {
                inner.bytes -= flow_bytes(&gone);
            }
            let mark = &mut inner.evicted_up_to[range_of(old)];
            *mark = Some(mark.map_or(old, |m| m.max(old)));
        }
    }

    /// Gives every retained flow that still has neither a response nor an
    /// error an error saying its final update was lost, and returns the
    /// rewritten flows. Called after the feed dropped messages, since a lost
    /// final version would otherwise leave the flow pending forever. A flow
    /// that is genuinely still in flight is corrected by its real final
    /// update, which replaces this one.
    pub fn mark_lost_pending(&self) -> Vec<Arc<Flow>> {
        let mut inner = self.lock();
        let Inner {
            order,
            flows,
            bytes,
            ..
        } = &mut *inner;
        let mut marked = Vec::new();
        for id in order.iter() {
            let Some(slot) = flows.get_mut(id) else {
                continue;
            };
            if slot.response.is_none() && slot.error.is_none() {
                let mut flow = Flow::clone(slot);
                flow.error = Some(LOST_UPDATE_ERROR.to_string());
                *bytes = *bytes - flow_bytes(slot) + flow_bytes(&flow);
                *slot = Arc::new(flow);
                marked.push(slot.clone());
            }
        }
        marked
    }

    pub fn get(&self, id: FlowId) -> Option<Arc<Flow>> {
        self.lock().flows.get(&id).cloned()
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.order.clear();
        inner.flows.clear();
        inner.bytes = 0;
        inner.evicted_up_to = [None; 2];
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().order.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// All retained flows in arrival order. Readers filter this copy so the
    /// lock is held only for cheap `Arc` clones and never blocks the ingest
    /// task while summaries are built.
    fn snapshot(&self) -> Vec<Arc<Flow>> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .map(|id| inner.flows[id].clone())
            .collect()
    }

    /// Every flow matching `filter`, in arrival order.
    pub fn matching(&self, filter: &FlowFilter) -> Vec<Arc<Flow>> {
        self.snapshot()
            .into_iter()
            .filter(|f| filter.matches(&FlowSummary::from(f.as_ref())))
            .collect()
    }

    /// Summaries matching `filter` in arrival order: returns the total number
    /// of matches and the `offset..offset + limit` window of them.
    pub fn list(
        &self,
        filter: &FlowFilter,
        offset: usize,
        limit: usize,
    ) -> (usize, Vec<FlowSummary>) {
        let mut total = 0;
        let mut page = Vec::new();
        for flow in self.snapshot() {
            let summary = FlowSummary::from(flow.as_ref());
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

#[cfg(test)]
pub(crate) mod tests {
    use sieve_core::flow::{Body, FlowKind, Request, Response, Timing};
    use time::OffsetDateTime;

    use super::*;

    /// A GET flow; with `status` it looks finished, without it pending.
    pub(crate) fn flow(id: FlowId, status: Option<u16>) -> Arc<Flow> {
        Arc::new(Flow {
            id,
            started_at: OffsetDateTime::UNIX_EPOCH,
            client: None,
            kind: FlowKind::Http,
            request: Request {
                method: "GET".into(),
                url: format!("https://a.test/{id}"),
                http_version: "HTTP/1.1".into(),
                headers: Vec::new(),
                body: Body::default(),
            },
            response: status.map(|status| Response {
                status,
                http_version: "HTTP/1.1".into(),
                headers: Vec::new(),
                body: Body::default(),
            }),
            error: None,
            timing: Timing::default(),
        })
    }

    /// A finished flow whose response body holds `raw` bytes plus, with
    /// `decoded`, a decoded copy of that many bytes.
    fn sized_flow(id: FlowId, raw: usize, decoded: usize) -> Arc<Flow> {
        let mut f = Flow::clone(&flow(id, Some(200)));
        let body = &mut f.response.as_mut().unwrap().body;
        body.raw = vec![0; raw].into();
        body.decoded = (decoded > 0).then(|| vec![0; decoded].into());
        Arc::new(f)
    }

    fn ids(store: &FlowStore) -> Vec<FlowId> {
        store
            .list(&FlowFilter::default(), 0, usize::MAX)
            .1
            .iter()
            .map(|s| s.id)
            .collect()
    }

    #[test]
    fn evicted_pending_flow_does_not_come_back() {
        let store = FlowStore::new(3);
        for id in 1..=4 {
            store.upsert(flow(id, None));
        }
        assert_eq!(ids(&store), [2, 3, 4]);
        // Flow 1 was evicted while pending; its final version must be dropped.
        store.upsert(flow(1, Some(200)));
        assert_eq!(ids(&store), [2, 3, 4]);
        assert!(store.get(1).is_none());
        // Retained flows still update in place.
        store.upsert(flow(2, Some(200)));
        assert_eq!(store.get(2).unwrap().response.as_ref().unwrap().status, 200);
        assert_eq!(ids(&store), [2, 3, 4]);
    }

    #[test]
    fn eviction_watermarks_are_per_id_range() {
        let store = FlowStore::new(2);
        let synthetic = SYNTHETIC_ID_BASE;
        // Evict proxy flows only.
        store.upsert(flow(10, None));
        store.upsert(flow(11, None));
        store.upsert(flow(12, None));
        // A UI-created flow has a far higher id and is simply new.
        store.upsert(flow(synthetic, Some(200)));
        assert_eq!(ids(&store), [12, synthetic]);
        // Evicting a synthetic flow must not block proxy flows with lower ids
        // in the other range, and vice versa.
        store.upsert(flow(synthetic + 1, Some(200)));
        store.upsert(flow(synthetic + 2, Some(200)));
        assert_eq!(ids(&store), [synthetic + 1, synthetic + 2]);
        store.upsert(flow(synthetic, Some(404)));
        assert_eq!(ids(&store), [synthetic + 1, synthetic + 2]);
        store.upsert(flow(13, Some(200)));
        assert!(store.get(13).is_some());
        store.upsert(flow(11, Some(200)));
        assert!(store.get(11).is_none());
    }

    #[test]
    fn clear_forgets_evictions() {
        let store = FlowStore::new(1);
        store.upsert(flow(1, None));
        store.upsert(flow(2, None));
        store.clear();
        store.upsert(flow(1, Some(200)));
        assert_eq!(ids(&store), [1]);
    }

    #[test]
    fn mark_lost_pending_only_touches_pending_flows() {
        let store = FlowStore::new(10);
        store.upsert(flow(1, None));
        store.upsert(flow(2, Some(200)));
        let marked = store.mark_lost_pending();
        assert_eq!(marked.len(), 1);
        assert_eq!(
            store.get(1).unwrap().error.as_deref(),
            Some(LOST_UPDATE_ERROR)
        );
        assert!(store.get(2).unwrap().error.is_none());
        // The real final update still wins.
        store.upsert(flow(1, Some(200)));
        assert!(store.get(1).unwrap().error.is_none());
        assert!(store.mark_lost_pending().is_empty());
    }

    #[test]
    fn byte_budget_evicts_oldest_first() {
        let store = FlowStore::with_limits(100, 100);
        store.upsert(sized_flow(1, 40, 0));
        store.upsert(sized_flow(2, 30, 10));
        assert_eq!(store.bytes(), 80);
        // 80 + 50 > 100: flow 1 goes, flow 2 survives (40 + 50 fits).
        store.upsert(sized_flow(3, 50, 0));
        assert_eq!(ids(&store), [2, 3]);
        assert_eq!(store.bytes(), 90);
        // Needs both older flows gone.
        store.upsert(sized_flow(4, 80, 0));
        assert_eq!(ids(&store), [4]);
        assert_eq!(store.bytes(), 80);
    }

    #[test]
    fn replacing_a_flow_updates_the_total() {
        let store = FlowStore::with_limits(100, 1000);
        store.upsert(sized_flow(1, 100, 0));
        store.upsert(sized_flow(2, 100, 0));
        store.upsert(sized_flow(1, 300, 100));
        assert_eq!(store.bytes(), 500);
        store.upsert(sized_flow(1, 10, 0));
        assert_eq!(store.bytes(), 110);
        assert_eq!(ids(&store), [1, 2]);
    }

    #[test]
    fn growing_update_evicts_older_flows() {
        let store = FlowStore::with_limits(100, 100);
        store.upsert(sized_flow(1, 30, 0));
        store.upsert(sized_flow(2, 30, 0));
        store.upsert(flow(3, None));
        store.upsert(sized_flow(3, 60, 0));
        assert_eq!(ids(&store), [2, 3]);
        assert_eq!(store.bytes(), 90);
        // The updated flow itself is never evicted, even alone over budget.
        store.upsert(sized_flow(3, 500, 0));
        assert_eq!(ids(&store), [3]);
        assert_eq!(store.bytes(), 500);
    }

    #[test]
    fn oversized_flow_is_kept_alone() {
        let store = FlowStore::with_limits(100, 100);
        store.upsert(sized_flow(1, 50, 0));
        store.upsert(sized_flow(2, 500, 0));
        assert_eq!(ids(&store), [2]);
        assert_eq!(store.bytes(), 500);
        // The next flow evicts it in turn.
        store.upsert(sized_flow(3, 10, 0));
        assert_eq!(ids(&store), [3]);
        assert_eq!(store.bytes(), 10);
    }

    #[test]
    fn clear_resets_the_total() {
        let store = FlowStore::with_limits(100, 1000);
        store.upsert(sized_flow(1, 100, 50));
        assert_eq!(store.bytes(), 150);
        store.clear();
        assert_eq!(store.bytes(), 0);
        store.upsert(sized_flow(2, 7, 0));
        assert_eq!(store.bytes(), 7);
    }

    #[test]
    fn mark_lost_pending_keeps_the_total() {
        let store = FlowStore::with_limits(100, 1000);
        let mut pending = Flow::clone(&flow(1, None));
        pending.request.body.raw = vec![0; 25].into();
        store.upsert(Arc::new(pending));
        store.upsert(sized_flow(2, 10, 5));
        assert_eq!(store.bytes(), 40);
        assert_eq!(store.mark_lost_pending().len(), 1);
        assert_eq!(store.bytes(), 40);
    }

    #[test]
    fn flow_evicted_by_bytes_does_not_come_back() {
        let store = FlowStore::with_limits(100, 100);
        store.upsert(sized_flow(1, 60, 0));
        store.upsert(sized_flow(2, 60, 0));
        assert_eq!(ids(&store), [2]);
        store.upsert(sized_flow(1, 10, 0));
        assert_eq!(ids(&store), [2]);
        assert!(store.get(1).is_none());
        assert_eq!(store.bytes(), 60);
    }
}

//! Breakpoints: requests held until the UI continues or aborts them.
//!
//! The proxy calls [`Breakpoints::hold`], which parks the request on a
//! `oneshot` channel. The UI lists the held requests, sends back an edit
//! through [`Breakpoints::resolve`], and the proxy resumes. A request nobody
//! answers is released unchanged after the timeout so the app never hangs.

use std::{
    collections::HashMap,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use crate::flow::FlowId;

/// How long a request stays held when nobody answers.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A request waiting at a breakpoint, as the UI sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeldRequest {
    /// The flow id, which is also the breakpoint id.
    pub id: FlowId,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The body as UTF-8 text; `None` when it is not valid UTF-8 and so cannot
    /// be edited as text.
    pub body: Option<String>,
    pub body_size: usize,
    /// How long the request is held before it continues on its own.
    #[serde(default)]
    pub timeout_secs: u64,
}

/// Changes to apply to a held request. Fields left out stay as they were.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RequestEdit {
    pub method: Option<String>,
    pub url: Option<String>,
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Continue(RequestEdit),
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Continued,
    Aborted,
    TimedOut,
    /// The client went away while the request was held.
    Dropped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BreakpointEvent {
    Held { request: HeldRequest },
    Released { id: FlowId, outcome: Outcome },
}

struct Slot {
    request: HeldRequest,
    reply: oneshot::Sender<Decision>,
}

pub struct Breakpoints {
    slots: Mutex<HashMap<FlowId, Slot>>,
    events: broadcast::Sender<BreakpointEvent>,
    timeout_ms: AtomicU64,
}

impl Default for Breakpoints {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            events: broadcast::channel(256).0,
            timeout_ms: AtomicU64::new(DEFAULT_TIMEOUT.as_millis() as u64),
        }
    }
}

impl Breakpoints {
    pub fn set_timeout(&self, timeout: Duration) {
        self.timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.load(Ordering::Relaxed))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BreakpointEvent> {
        self.events.subscribe()
    }

    /// The requests currently held, oldest id first.
    pub fn pending(&self) -> Vec<HeldRequest> {
        let mut held: Vec<_> = self
            .lock()
            .values()
            .map(|slot| slot.request.clone())
            .collect();
        held.sort_by_key(|r| r.id);
        held
    }

    /// Answers a held request. Returns false when it is no longer held, e.g.
    /// it already timed out.
    pub fn resolve(&self, id: FlowId, decision: Decision) -> bool {
        let Some(slot) = self.lock().remove(&id) else {
            return false;
        };
        let outcome = match decision {
            Decision::Continue(_) => Outcome::Continued,
            Decision::Abort => Outcome::Aborted,
        };
        if slot.reply.send(decision).is_err() {
            return false;
        }
        let _ = self.events.send(BreakpointEvent::Released { id, outcome });
        true
    }

    /// Holds `request` until it is resolved or the timeout passes. A timeout
    /// continues the request unchanged.
    pub async fn hold(&self, mut request: HeldRequest) -> Decision {
        let id = request.id;
        let timeout = self.timeout();
        request.timeout_secs = timeout.as_secs();
        let (reply, answer) = oneshot::channel();
        self.lock().insert(
            id,
            Slot {
                request: request.clone(),
                reply,
            },
        );
        let _ = self.events.send(BreakpointEvent::Held { request });

        // If the client disconnects the future is dropped here, and the guard
        // clears the slot so the UI does not show a request nobody waits for.
        let mut guard = Release {
            owner: self,
            id,
            armed: true,
        };
        let decision = tokio::time::timeout(timeout, answer).await;
        guard.armed = false;
        match decision {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                self.forget(id, Outcome::Dropped);
                Decision::Continue(RequestEdit::default())
            }
            Err(_) => {
                self.forget(id, Outcome::TimedOut);
                Decision::Continue(RequestEdit::default())
            }
        }
    }

    fn forget(&self, id: FlowId, outcome: Outcome) {
        if self.lock().remove(&id).is_some() {
            let _ = self.events.send(BreakpointEvent::Released { id, outcome });
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<FlowId, Slot>> {
        self.slots.lock().expect("breakpoint state poisoned")
    }
}

struct Release<'a> {
    owner: &'a Breakpoints,
    id: FlowId,
    armed: bool,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.owner.forget(self.id, Outcome::Dropped);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn held(id: FlowId) -> HeldRequest {
        HeldRequest {
            id,
            method: "GET".into(),
            url: "http://x/".into(),
            headers: vec![],
            body: Some(String::new()),
            body_size: 0,
            timeout_secs: 0,
        }
    }

    #[tokio::test]
    async fn resolve_releases_a_held_request() {
        let bp = Arc::new(Breakpoints::default());
        let mut events = bp.subscribe();
        let task = tokio::spawn({
            let bp = bp.clone();
            async move { bp.hold(held(7)).await }
        });
        assert!(matches!(
            events.recv().await.unwrap(),
            BreakpointEvent::Held { .. }
        ));
        assert_eq!(bp.pending().len(), 1);
        let edit = RequestEdit {
            body: Some("changed".into()),
            ..RequestEdit::default()
        };
        assert!(bp.resolve(7, Decision::Continue(edit.clone())));
        assert_eq!(task.await.unwrap(), Decision::Continue(edit));
        assert!(bp.pending().is_empty());
        assert!(!bp.resolve(7, Decision::Abort));
    }

    #[tokio::test]
    async fn unanswered_requests_continue_after_the_timeout() {
        let bp = Breakpoints::default();
        assert_eq!(bp.timeout(), DEFAULT_TIMEOUT);
        bp.set_timeout(Duration::from_millis(30));
        let decision = bp.hold(held(1)).await;
        assert_eq!(decision, Decision::Continue(RequestEdit::default()));
        assert!(bp.pending().is_empty());
    }

    #[tokio::test]
    async fn a_dropped_hold_clears_its_slot() {
        let bp = Arc::new(Breakpoints::default());
        let task = tokio::spawn({
            let bp = bp.clone();
            async move { bp.hold(held(3)).await }
        });
        while bp.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        assert!(bp.pending().is_empty());
    }
}

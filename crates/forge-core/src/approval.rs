//! Approval gate (S2 §3): the bridge between an agent waiting on an
//! `Ask` decision and the UI answering it. The agent loop parks the tool
//! call in `request`; the frontend resolves it via `Runtime::approve`.

use std::collections::HashMap;
use tokio::sync::{oneshot, Mutex};

#[derive(Default)]
pub struct ApprovalGate {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park the call and wait for the UI's answer. `notify` runs once the
    /// request is registered (the agent loop uses it to emit the
    /// `ApprovalRequested` event). A dropped responder (frontend gone)
    /// counts as a denial.
    pub async fn request(&self, call_id: &str, notify: impl FnOnce()) -> bool {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(call_id.to_string(), tx);
        notify();
        match rx.await {
            Ok(approved) => approved,
            Err(_) => false,
        }
    }

    /// Answer a pending request. Returns false when no request with that
    /// id is pending.
    pub async fn respond(&self, call_id: &str, approved: bool) -> bool {
        match self.pending.lock().await.remove(call_id) {
            Some(tx) => tx.send(approved).is_ok(),
            None => false,
        }
    }

    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_resolves_with_response() {
        let gate = ApprovalGate::new();
        let gate2 = std::sync::Arc::new(gate);
        let g = gate2.clone();
        let waiter = tokio::spawn(async move {
            g.request("c1", || {}).await
        });
        // Give the waiter a moment to register, then answer.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(gate2.pending_count().await, 1);
        assert!(gate2.respond("c1", true).await);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn respond_without_request_is_false() {
        let gate = ApprovalGate::new();
        assert!(!gate.respond("ghost", true).await);
    }

    #[tokio::test]
    async fn dropped_responder_counts_as_denial() {
        let gate = ApprovalGate::new();
        let g = std::sync::Arc::new(gate);
        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.request("c1", || {}).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Remove the sender without answering (simulates frontend exit).
        g.pending.lock().await.remove("c1");
        assert!(!waiter.await.unwrap());
    }
}

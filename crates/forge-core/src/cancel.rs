//! Cooperative cancellation for a running turn.
//!
//! The UI (or a headless driver) keeps the `CancelHandle` for the turn it
//! started; the agent loop holds a `CancelToken` and polls it at safe
//! points, racing it against the provider stream and tool execution. A
//! cancelled turn still produces protocol-valid history: every started
//! tool call gets a recorded result.

use tokio::sync::watch;

/// Cloneable read side of the cancellation signal.
#[derive(Clone, Debug)]
pub struct CancelToken {
    rx: watch::Receiver<bool>,
}

/// Write side held by whoever may cancel the turn.
#[derive(Clone, Debug)]
pub struct CancelHandle {
    tx: watch::Sender<bool>,
}

impl Default for CancelHandle {
    fn default() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx }
    }
}

impl CancelHandle {
    /// Snapshot of the read side to hand to the agent loop.
    pub fn token(&self) -> CancelToken {
        CancelToken {
            rx: self.tx.subscribe(),
        }
    }

    /// Signal cancellation. Idempotent.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }
}

impl CancelToken {
    /// Non-blocking check for use between awaits.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves as soon as cancellation is requested. If the handle was
    /// dropped without cancelling, this resolves too — the caller then
    /// observes `is_cancelled() == false` and simply continues.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_propagates_to_tokens() {
        let handle = CancelHandle::default();
        let token = handle.token();
        assert!(!token.is_cancelled());

        let t2 = token.clone();
        let waiter = tokio::spawn(async move {
            t2.cancelled().await;
        });

        handle.cancel();
        waiter.await.unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn dropped_handle_does_not_hang_cancelled() {
        let token = {
            let handle = CancelHandle::default();
            handle.token()
        };
        // Handle dropped without cancelling: cancelled() must resolve.
        tokio::time::timeout(std::time::Duration::from_secs(1), token.cancelled())
            .await
            .expect("cancelled() must resolve when the handle is dropped");
        assert!(!token.is_cancelled());
    }
}

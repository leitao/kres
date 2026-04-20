//! Shutdown signalling.
//!
//! Thin wrapper around `tokio_util::sync::CancellationToken` so
//! downstream crates don't need to learn tokio-util directly.
//!
//! Usage:
//! - Each Task owns a `Shutdown` that forks from the manager's root.
//! - The agent loops `select!` on their work futures AND
//!   `shutdown.cancelled()`.
//! - `/stop`, `/clear`, `--turns` reached → call `shutdown.cancel()`.
//!
//! bugs.md#C2: has no shutdown signal — detached
//! daemon threads keep burning API calls. The Rust version routes
//! every long-running future through a cancellation check.

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Shutdown {
    token: CancellationToken,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Produce a child token: cancelling the parent cancels the child,
    /// but the child can be cancelled independently without affecting
    /// siblings.
    pub fn child(&self) -> Self {
        Self {
            token: self.token.child_token(),
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Future that resolves when the token is cancelled. Clone-cheap;
    /// await in `tokio::select!` arms.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Convert to the underlying tokio_util token for callers that
    /// already speak that API.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_fires_future() {
        let s = Shutdown::new();
        let s2 = s.clone();
        let fut = tokio::spawn(async move { s2.cancelled().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!fut.is_finished());
        s.cancel();
        tokio::time::timeout(Duration::from_millis(200), fut)
            .await
            .expect("cancellation should wake the future")
            .unwrap();
        assert!(s.is_cancelled());
    }

    #[tokio::test]
    async fn parent_cancel_cancels_children() {
        let parent = Shutdown::new();
        let c1 = parent.child();
        let c2 = parent.child();
        parent.cancel();
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancel_does_not_propagate_up() {
        let parent = Shutdown::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancel_does_not_affect_siblings() {
        let parent = Shutdown::new();
        let a = parent.child();
        let b = parent.child();
        a.cancel();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_after_cancel() {
        let s = Shutdown::new();
        s.cancel();
        // Already cancelled — `cancelled()` should resolve without a
        // wake.
        tokio::time::timeout(Duration::from_millis(50), s.cancelled())
            .await
            .expect("should resolve immediately");
    }
}

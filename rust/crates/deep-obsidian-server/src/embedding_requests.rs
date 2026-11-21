//! Carry request abandonment across async tool work and blocking search workers.
use deep_obsidian_index::embedding_scheduler::{self, CancellationToken};

tokio::task_local! { static REQUEST: CancellationToken; }

pub(crate) async fn scope<T>(future: impl std::future::Future<Output = T>) -> T {
    struct CancelOnDrop(CancellationToken);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let token = CancellationToken::default();
    let _guard = CancelOnDrop(token.clone());
    REQUEST.scope(token, future).await
}

pub(crate) fn spawn_blocking<F, T>(operation: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let check = REQUEST.try_with(CancellationToken::check).ok();
    tokio::task::spawn_blocking(move || embedding_scheduler::with_cancellation(check, operation))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dropping_request_cancels_its_blocking_worker_context() {
        let (check_tx, check_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(scope(async move {
            spawn_blocking(move || {
                check_tx
                    .send(embedding_scheduler::current_cancellation().unwrap())
                    .ok();
            })
            .await
            .unwrap();
            std::future::pending::<()>().await;
        }));
        let check = check_rx.await.unwrap();
        assert!(!check());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(check());
        assert!(embedding_scheduler::current_cancellation().is_none());
    }
}

//! Blocking work handed to Tokio's blocking pool: the image store's file hashing and `stat`s
//! (ADR 0087), never run on a runtime worker that drives other futures, since a disk can be tens
//! of gigabytes. The pool's threads end with the work; nothing outlives the call's future.

/// Runs `work` on Tokio's blocking pool and returns what it returned; `Err(None)` when the work
/// panicked or the runtime is shutting down.
///
/// # Errors
/// `Some(e)` when `work` failed with `e`, `None` when it did not complete.
pub async fn blocking<T: Send + 'static, E: Send + 'static>(
    work: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, Option<E>> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| None)?
        .map_err(Some)
}

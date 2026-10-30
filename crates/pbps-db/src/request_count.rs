//! Test-only PostgreSQL request counts (DEC-2121.1).
//!
//! One driver query or execute invocation is one request, including a failed
//! invocation and a batch containing several statements. This measures the
//! batching regression's unit of work, not SQL statements or wire messages.

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

tokio::task_local! {
    static REQUESTS: Counter;
}

/// Counts PostgreSQL requests made by this future on its current task.
///
/// All connections called by the future share its count, including startup
/// queries on newly opened connections. Separate and nested scopes do not
/// share counts. A lifecycle supervisor may explicitly inherit this scope;
/// unrelated spawned work stays unmeasured. A task scope avoids counting
/// setup or unrelated tests that happen to use the same server.
pub async fn scope<F: Future>(future: F) -> (F::Output, u64) {
    REQUESTS
        .scope(Counter(Arc::new(AtomicU64::new(0))), async {
            let answer = future.await;
            (
                answer,
                REQUESTS.with(|count| count.0.load(Ordering::Relaxed)),
            )
        })
        .await
}

/// Opaque test measurement shared only with an explicitly owned child task.
#[derive(Clone)]
pub struct Counter(Arc<AtomicU64>);

/// Captures the current test measurement without opening a new one.
pub fn current() -> Option<Counter> {
    REQUESTS.try_with(Clone::clone).ok()
}

/// Continues the parent's measurement on a supervisor (DEC-1519.1).
pub async fn inherit<F: Future>(counter: Option<Counter>, future: F) -> F::Output {
    match counter {
        Some(counter) => REQUESTS.scope(counter, future).await,
        None => future.await,
    }
}

pub(crate) fn record() {
    // Requests outside a measurement must not require a measurement owner.
    let _ = REQUESTS.try_with(|count| count.0.fetch_add(1, Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::{current, inherit, record, scope};

    #[tokio::test]
    async fn concurrent_futures_keep_their_counts_across_yields() {
        let (left, right) = tokio::join!(
            scope(async {
                record();
                tokio::task::yield_now().await;
                record();
            }),
            scope(async {
                tokio::task::yield_now().await;
                record();
            }),
        );
        assert_eq!(left.1, 2);
        assert_eq!(right.1, 1);
    }

    #[tokio::test]
    async fn nested_scopes_and_unmeasured_work_do_not_pollute_a_count() {
        record();
        let (inner, outer) = scope(async {
            record();
            let ((), inner) = scope(async {
                record();
                tokio::task::yield_now().await;
                record();
            })
            .await;
            record();
            inner
        })
        .await;
        assert_eq!((inner, outer), (2, 2));
        record();
        assert_eq!(scope(async {}).await.1, 0);
    }

    #[tokio::test]
    async fn an_owned_supervisor_inherits_only_its_explicit_measurement() {
        let ((), requests) = scope(async {
            record();
            let counter = current();
            tokio::spawn(inherit(counter, async {
                record();
                tokio::task::yield_now().await;
                record();
            }))
            .await
            .unwrap();
            tokio::spawn(async {
                record();
            })
            .await
            .unwrap();
            // A nested measurement still has its own authority.
            assert_eq!(
                scope(async {
                    record();
                })
                .await
                .1,
                1
            );
        })
        .await;
        assert_eq!(requests, 3);
    }
}

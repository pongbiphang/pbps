//! Test-only PostgreSQL request counts (DEC-2121.1).
//!
//! One driver query or execute invocation is one request, including a failed
//! invocation and a batch containing several statements. This measures the
//! batching regression's unit of work, not SQL statements or wire messages.

use std::cell::Cell;
use std::future::Future;

tokio::task_local! {
    static REQUESTS: Cell<u64>;
}

/// Counts PostgreSQL requests made by this future on its current task.
///
/// All connections called by the future share its count, including startup
/// queries on newly opened connections. Separate and nested scopes do not
/// share counts. Spawned tasks need their own scope; the resolver SQL measured
/// by the live guard runs in the caller's task. A task scope avoids counting
/// setup or unrelated tests that happen to use the same server.
pub async fn scope<F: Future>(future: F) -> (F::Output, u64) {
    REQUESTS
        .scope(Cell::new(0), async {
            let answer = future.await;
            (answer, REQUESTS.with(Cell::get))
        })
        .await
}

pub(crate) fn record() {
    // Requests outside a measurement must not require a measurement owner.
    let _ = REQUESTS.try_with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
mod tests {
    use super::{record, scope};

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
}

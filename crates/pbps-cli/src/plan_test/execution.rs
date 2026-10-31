//! Bounded owned execution for D2b2 (DEC-1165.10), without command dispatch or replay policy.
//! The same cancellation observation survives successful phases; cleanup has
//! its own allowance and does not consume the work cancellation signal.
use super::{
    report::{Phase, TestData},
    revision::Assembled,
    session::Session,
};
use crate::{deploy::test_run, test_scratch};
use anyhow::Context as _;
use pbps_config::test_manifest::Timeouts;
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    time::{Duration, Instant},
};
use tokio::time::Instant as Deadline;

pub(crate) fn ms(n: std::num::NonZeroU32) -> Duration {
    Duration::from_millis(u64::from(n.get()))
}
pub(crate) fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

pub(crate) struct Control {
    end: Deadline,
    timeouts: Timeouts,
    cancel: Pin<Box<dyn Future<Output = std::io::Result<()>>>>,
}
impl Control {
    pub(crate) fn new(
        timeouts: Timeouts,
        signal: impl Future<Output = std::io::Result<()>> + 'static,
    ) -> Self {
        let mut signal = Box::pin(signal);
        let mut observed = false;
        Self {
            end: Deadline::now() + ms(timeouts.run_ms),
            timeouts,
            cancel: Box::pin(poll_fn(move |cx| {
                if observed {
                    return std::task::Poll::Ready(Ok(()));
                }
                let result = signal.as_mut().poll(cx);
                observed = result.is_ready();
                result
            })),
        }
    }
    fn deadline(&self, allowance: Duration) -> Deadline {
        self.end.min(Deadline::now() + allowance)
    }
    pub(crate) async fn bounded<T>(
        &mut self,
        allowance: Duration,
        future: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let end = self.deadline(allowance);
        if Deadline::now() >= end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "test phase deadline exceeded",
            )
            .into());
        }
        tokio::select! {
            biased;
            signal = &mut self.cancel => Err(std::io::Error::other(format!("test cancelled: {signal:?}")).into()),
            result = tokio::time::timeout_at(end, future) => result.context("test phase deadline exceeded")?,
        }
    }
    pub(crate) async fn start<'d>(
        &mut self,
        dir: &'d test_scratch::StateDir,
        image: &str,
        data: &mut TestData,
        phase: usize,
    ) -> Option<test_scratch::TestScratch<'d>> {
        let started = Instant::now();
        let result = test_scratch::TestScratch::start_cancelled(
            dir,
            pbps_db::Driver::Postgres,
            image,
            pbps_config::resolver::PullPolicy::IfMissing,
            Some(self.deadline(ms(self.timeouts.startup_ms))),
            ms(self.timeouts.cleanup_ms),
            &mut self.cancel,
        )
        .await;
        data.phases[phase].elapsed_ms += elapsed(started);
        match result {
            Ok(scratch) => {
                data.run_id = Some(scratch.run_id().into());
                Some(scratch)
            }
            Err(error) => {
                data.startup_failure(phase, &error);
                None
            }
        }
    }
    pub(crate) async fn provision<'s>(
        &mut self,
        scratch: &'s test_scratch::TestScratch<'s>,
        data: &mut TestData,
        phase: usize,
    ) -> anyhow::Result<Session<'s>> {
        let started = Instant::now();
        let result = self
            .bounded(
                ms(self.timeouts.startup_ms),
                test_run::boxed_future(|| async {
                    data.acquired_image = Some(scratch.acquired_image().await?);
                    let mut conn =
                        pbps_db::Conn::connect(pbps_db::Driver::Postgres, scratch.connection())
                            .await?;
                    let rows = conn.query("SELECT version() AS version").await?;
                    data.engine_build = Some(
                        rows.first()
                            .context("engine build is unreadable")?
                            .try_get::<&str>("version")?
                            .context("missing engine build")?
                            .into(),
                    );
                    Session::new(scratch).await
                }),
            )
            .await;
        data.phases[phase].record(started, "test.infrastructure", &result);
        result
    }
    pub(crate) async fn bootstrap(
        &mut self,
        session: &Session<'_>,
        input: &Assembled,
        phase: &mut Phase,
    ) -> anyhow::Result<i64> {
        let started = Instant::now();
        let result = self
            .bounded(
                ms(self.timeouts.step_ms),
                test_run::boxed_future(|| test_run::bootstrap(session, input, phase)),
            )
            .await;
        phase.record(started, "test.plan", &result);
        result
    }
    pub(crate) async fn plan(
        &mut self,
        session: &Session<'_>,
        input: &Assembled,
        phase: &mut Phase,
    ) -> anyhow::Result<Box<test_run::Planned>> {
        let started = Instant::now();
        let result = self
            .bounded(
                ms(self.timeouts.step_ms),
                test_run::boxed_future(|| test_run::plan(session, input, phase)),
            )
            .await;
        phase.record(started, "test.plan", &result);
        result
    }
    pub(crate) async fn apply(
        &mut self,
        session: &Session<'_>,
        input: &Assembled,
        planned: &test_run::Planned,
        phase: &mut Phase,
        approved: &str,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        let progress = test_run::Progress::default();
        let result = self
            .bounded(
                ms(self.timeouts.step_ms),
                test_run::boxed_future(|| {
                    test_run::apply(session, input, planned, phase, approved, &progress)
                }),
            )
            .await;
        // Dropping the apply future on a timeout/cancellation skips its tail.
        // Progress is borrowed, so its accepted/started facts and findings survive.
        test_run::collect_progress(phase, &progress);
        phase.record(started, "test.apply", &result);
        result
    }
    pub(crate) async fn grants(
        &mut self,
        session: &Session<'_>,
        input: &Assembled,
        phase: &mut Phase,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        let result = self
            .bounded(
                ms(self.timeouts.step_ms),
                test_run::boxed_future(|| session.grants(&input.loaded.schema)),
            )
            .await;
        phase.record(started, "test.infrastructure", &result);
        result
    }
    pub(crate) async fn finish(&self, scratch: test_scratch::TestScratch<'_>, data: &mut TestData) {
        let mut phase = Phase::new("cleanup", "owned resources");
        let started = Instant::now();
        match tokio::time::timeout(ms(self.timeouts.cleanup_ms), scratch.finish()).await {
            Ok(Ok(())) => {
                data.cleanup_confirmed = true;
                phase.outcome = super::report::Status::Passed;
            }
            result => {
                phase.outcome = super::report::Status::Unanswerable;
                phase.findings.push(crate::output::Finding::error(
                    "test.cleanup",
                    format!(
                        concat!(
                            "owned cleanup was not confirmed: {:?}; ",
                            "run pbps test --cleanup"
                        ),
                        result
                    ),
                ));
            }
        }
        phase.elapsed_ms = elapsed(started);
        data.phases.push(phase);
    }
}

#[cfg(test)]
mod tests;

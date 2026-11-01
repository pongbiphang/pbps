//! Request cancellation cannot own the last cleanup obligation (DEC-1519.1).

use super::{BindingRequest, Error, ProduceError, Run, Standard, Tokens, Unsealed};
use pbps_db::{Conn, Driver};
use tokio::sync::oneshot;

// Connection strings and the fingerprint key stay private, without Debug.
// The task owns every input: the caller may drop its future and its schemas.
pub(super) struct Request {
    pub driver: Driver,
    pub scratch: String,
    pub target: String,
    pub planned_identity: String,
    pub bootstrap: pbps_diff::order::DiffPlan,
    pub base: pbps_model::Schema,
    pub desired: pbps_model::Schema,
    pub base_ids: pbps_model::IdsFile,
    pub desired_ids: pbps_model::IdsFile,
    pub dependencies: pbps_model::dependency::Dependencies,
    pub hints: pbps_model::Hints,
    pub key: pbps_db::fingerprint::EnvironmentFingerprintKey,
    pub scope: super::super::ScopeRequest,
    pub standard: Standard,
    pub externals: Option<String>,
    pub provenance: super::Provenance,
}

// Even a result already sent into the channel needs an owner. A receiver
// dropped before observing it must not silently lose a cleanup failure.
struct Reply<D = pbps_pg::Postgres>(Option<Result<Unsealed<D>, ProduceError>>);

impl<D> Reply<D> {
    fn receive(mut self) -> Result<Unsealed<D>, ProduceError> {
        self.0.take().expect("a supervisor reply is received once")
    }
}

impl<D> Drop for Reply<D> {
    fn drop(&mut self) {
        if let Some(Err(ProduceError::Cleanup(names))) = self.0.take() {
            super::recovery::cancelled_cleanup(names);
        }
    }
}

impl Request {
    pub async fn start<D: super::super::PlanningDialect>(
        self,
    ) -> Result<Unsealed<D>, ProduceError> {
        let (sender, receiver) = oneshot::channel();
        #[cfg(test)]
        let context = TestContext::capture();
        // Dropping a JoinHandle detaches it. Runtime shutdown can still stop
        // this owner before release; durable recovery covers that boundary.
        tokio::spawn(async move {
            let work = self.execute::<D>();
            #[cfg(test)]
            let result = context.run(work).await;
            #[cfg(not(test))]
            let result = work.await;
            let _ = sender.send(Reply(Some(result)));
        });
        receiver
            .await
            .map_err(|_| ProduceError::Run(Error::Read(
                "the resolver supervisor stopped before returning its result; inspect scratch recovery findings".into(),
            )))?
            .receive()
    }

    async fn execute<D: super::super::PlanningDialect>(self) -> Result<Unsealed<D>, ProduceError> {
        let Self {
            driver,
            scratch,
            target,
            planned_identity,
            bootstrap,
            base,
            desired,
            base_ids,
            desired_ids,
            dependencies,
            hints,
            key,
            scope,
            standard,
            externals,
            provenance,
        } = self;
        let binding = BindingRequest {
            bootstrap: &bootstrap,
            base: &base,
            desired: &desired,
            dependencies: &dependencies,
        };
        let base = pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        };
        let desired = pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        // Preserve endpoint/identity checks and the keeper-before-write order.
        let scratch = super::super::engine::endpoint(driver, &scratch).await?;
        let mut target = Conn::connect(driver, &target).await.map_err(|error| {
            super::unusable(format!("the target could not be connected: {error}"))
        })?;
        let tokens = Tokens::generate();
        let mut admin = super::super::engine::connect_scratch(driver, &scratch, &tokens).await?;
        let (provisioning, state) = super::super::engine::scratch(
            driver,
            &mut target,
            &mut admin,
            &tokens,
            &planned_identity,
            &scratch,
            &scope,
            &standard,
            externals.as_deref(),
            &provenance,
        )
        .await?;
        let mut run = Run {
            driver,
            planned_identity: &planned_identity,
            key: &key,
            request: &scope,
            tokens: &tokens,
            externals: externals.as_deref(),
            provenance: &provenance,
            state,
        };
        // A dropped SQL future can leave a claim or a transaction half-way
        // through acquisition. Complete resolution before releasing on the
        // same checked connection, even when nobody awaits the result.
        let result = run
            .resolve::<D>(
                provisioning,
                &mut target,
                &mut admin,
                &binding,
                base,
                desired,
                &hints,
            )
            .await;
        // Neither in-flight SQL nor release is raced against caller cancellation.
        let cleanup = run.state.release(&mut admin).await;
        drop(run);
        match (result, cleanup) {
            (_, Err(names)) => Err(ProduceError::Cleanup(names)),
            (Err(error), Ok(())) => Err(ProduceError::Run(error)),
            (Ok((ordered, sealing, opening)), Ok(())) => Ok(Unsealed {
                key,
                ordered,
                sealing,
                opening,
            }),
        }
    }
}

// Tokio task locals do not inherit into the supervisor. Keep existing
// regression injection on the task that actually executes the production run.
#[cfg(test)]
struct TestContext {
    requests: Option<pbps_db::request_count::Counter>,
    after_staging: Option<String>,
    fill_failure: Option<(String, String)>,
    scratch_build: Option<String>,
}

#[cfg(test)]
impl TestContext {
    fn capture() -> Self {
        Self {
            requests: pbps_db::request_count::current(),
            after_staging: super::AFTER_STAGING.try_with(Clone::clone).ok().flatten(),
            fill_failure: super::FILL_FAILURE2134
                .try_with(Clone::clone)
                .ok()
                .flatten(),
            scratch_build: super::SCRATCH_BUILD.try_with(Clone::clone).ok().flatten(),
        }
    }

    async fn run<T>(self, work: impl Future<Output = T>) -> T {
        pbps_db::request_count::inherit(
            self.requests,
            super::AFTER_STAGING.scope(
                self.after_staging,
                super::FILL_FAILURE2134.scope(
                    self.fill_failure,
                    super::SCRATCH_BUILD.scope(self.scratch_build, work),
                ),
            ),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_cleanup_reply_is_owned_until_the_caller_observes_it() {
        let names = vec!["role pbps_run_1519_reply".to_owned()];
        // Cancellation before delivery, and after a completed send, both
        // retain exact names. Observing the error keeps the ordinary API path.
        for delivered in [false, true] {
            let (sender, receiver) = oneshot::channel();
            let reply = Reply::<pbps_pg::Postgres>(Some(Err(ProduceError::Cleanup(names.clone()))));
            if delivered {
                assert!(sender.send(reply).is_ok());
                drop(receiver);
            } else {
                drop(receiver);
                drop(sender.send(reply));
            }
            let reported = super::super::recovery::take();
            assert!(reported.cancelled.contains(&names));
        }
        let (sender, receiver) = oneshot::channel();
        assert!(
            sender
                .send(Reply::<pbps_pg::Postgres>(Some(Err(
                    ProduceError::Cleanup(names.clone())
                ))))
                .is_ok()
        );
        let result = receiver
            .await
            .unwrap_or_else(|_| panic!("reply sender vanished"))
            .receive();
        assert!(matches!(result, Err(ProduceError::Cleanup(ref left)) if left == &names));
        assert!(!super::super::recovery::take().cancelled.contains(&names));
    }
}

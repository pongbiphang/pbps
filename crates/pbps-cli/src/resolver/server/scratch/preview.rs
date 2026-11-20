//! Target-less rehearsal uses the resolver's acquisition, never its evidence.

use super::super::{Error, PlanningDialect, ProduceError, ScopeRequest};
use super::Provenance;
use crate::resolver::contract::{self, ResolverEngine};
use pbps_db::Conn;

/// Installed before acquisition so terminal signals allow owned cleanup.
pub struct Cancellation {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
}

impl Cancellation {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(windows)]
            interrupt: tokio::signal::windows::ctrl_c()?,
        })
    }

    pub async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
        #[cfg(windows)]
        self.interrupt.recv().await;
        #[cfg(not(any(unix, windows)))]
        std::future::pending::<()>().await;
    }
}

/// Acquisition is shielded from cooperative cancellation: every creation is
/// recorded before it is sent, and finishes before cleanup uses its session.
/// The workload connection is dropped before release, including on signals.
///
/// An engine without a target-less preview is refused by name before any
/// connection is opened (DEC-2798.5): the scratch server is the first thing
/// this touches, and it must never be reached with another engine's string.
pub async fn with_server<D: PlanningDialect, T>(
    connection: &str,
    externals: Option<&str>,
    cancellation: &mut Cancellation,
    run: impl AsyncFnOnce(&mut Conn) -> anyhow::Result<T>,
) -> Result<T, ProduceError> {
    contract::preview_of::<D>()
        .map_err(|unsupported| ProduceError::Acquire(unsupported.to_string()))?;
    let mut admin = <D::Resolver as ResolverEngine>::preview_connect(connection).await?;
    let request = ScopeRequest::default();
    let provenance = Provenance {
        entry: "dev".into(),
        scratch: pbps_model::resolver::ScratchSource::Server {},
        outside_writes: pbps_model::resolver::OutsideWrites::Refused,
        same_instance: pbps_model::resolver::SameInstance::NotApplicable,
    };
    let (provisioning, mut scratch) = <D::Resolver as ResolverEngine>::preview_scratch(
        &mut admin,
        connection,
        &request,
        &provenance,
    )
    .await
    .map_err(ProduceError::Run)?;
    let acquired =
        <D::Resolver as ResolverEngine>::preview_owner(&mut scratch, provisioning, &mut admin)
            .await;
    let result = match acquired {
        Err(error) => Err(ProduceError::Run(Error::Unusable(format!(
            "{error}; supply an empty scratch database owned by its account, or an account allowed to create scratch resources"
        )))),
        Ok(mut owner) => {
            tokio::select! {
                biased;
                _ = cancellation.wait() => Err(ProduceError::Acquire("dev rehearsal cancelled".into())),
                result = async {
                    if let Some(script) = externals {
                        <D::Resolver as ResolverEngine>::preview_externals(&mut owner, script).await?;
                    }
                    run(&mut owner).await
                } => result.map_err(|error: anyhow::Error| ProduceError::Run(Error::Unusable(
                    format!("dev rehearsal: {error:#}; use an empty scratch database owned by the scratch account")
                ))),
            }
        }
    };
    match <D::Resolver as ResolverEngine>::release(&mut scratch, &mut admin).await {
        Err(names) => Err(ProduceError::Cleanup(names)),
        Ok(()) => result,
    }
}

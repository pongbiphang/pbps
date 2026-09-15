//! A direct native-Linux target connection, retaining its TLS and process
//! leases together. Apply can re-establish this without invoking Docker.

use super::{SocketOwnerLease, UnqualifiedProcess};
use pbps_db::resolver::{BackendProcess, InstanceObservation};
use pbps_db::transport::PeerVerifiedConn;
use std::sync::{Arc, Weak};

#[path = "target_engine.rs"]
mod engine;

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;

pub struct NativeTarget {
    current: Option<BoundTarget>,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeTargetError {
    #[error("the native target socket cannot be bound to the selected readable service")]
    SocketOwner,
    #[error("the native target socket and service must run the supported engine executable")]
    Executable,
    #[error(
        "native target inspection requires the main engine service process, not a backend child"
    )]
    ServiceRoot,
    #[error("the engine's required read-only instance identity could not be read")]
    EngineIdentity,
    #[error("the engine's reported process does not match the connected native backend")]
    ProcessMapping,
    #[error("the native target process binding changed or became unreadable")]
    Binding(#[from] UnqualifiedProcess),
}

struct BoundTarget {
    connection: PeerVerifiedConn,
    lease: Arc<SocketOwnerLease>,
    identity: InstanceObservation,
}

/// Weak ownership prevents scratch from keeping a discarded target binding
/// alive. Socket continuity still comes from the retained kernel handles.
pub(crate) struct TargetWitness {
    lease: Weak<SocketOwnerLease>,
    connection: pbps_db::transport::ConnectionId,
}

impl TargetWitness {
    pub(crate) fn check(&self) -> Result<(), UnqualifiedProcess> {
        self.lease
            .upgrade()
            .ok_or(UnqualifiedProcess)?
            .check_socket()
    }

    pub(crate) fn connection_id(&self) -> pbps_db::transport::ConnectionId {
        self.connection
    }
}

impl NativeTarget {
    pub(crate) fn witness(&self) -> Result<TargetWitness, UnqualifiedProcess> {
        let current = self.current.as_ref().ok_or(UnqualifiedProcess)?;
        Ok(TargetWitness {
            lease: Arc::downgrade(&current.lease),
            connection: current.connection.id(),
        })
    }
    pub(crate) fn driver(&self) -> Result<pbps_db::Driver, UnqualifiedProcess> {
        self.current
            .as_ref()
            .map(|state| state.connection.driver())
            .ok_or(UnqualifiedProcess)
    }

    pub(crate) fn connection_id(
        &self,
    ) -> Result<pbps_db::transport::ConnectionId, UnqualifiedProcess> {
        self.current
            .as_ref()
            .map(|state| state.connection.id())
            .ok_or(UnqualifiedProcess)
    }
    pub(crate) fn service(&self) -> Result<&super::ProcessLease, UnqualifiedProcess> {
        self.current
            .as_ref()
            .map(|state| state.lease.service())
            .ok_or(UnqualifiedProcess)
    }
    /// A configured service PID scopes read-only process inspection; the
    /// actual connected socket must belong to that live engine's process tree.
    /// A PID label alone cannot authorize a proxy or a different backend.
    pub async fn establish(
        mut connection: PeerVerifiedConn,
        service_pid: u32,
    ) -> Result<Self, NativeTargetError> {
        let lease = SocketOwnerLease::capture(&connection, service_pid)
            .map_err(|_| NativeTargetError::SocketOwner)?;
        let expected = engine::native_executable(connection.driver());
        if lease
            .service()
            .executable_path()
            .file_name()
            .is_none_or(|name| name != expected)
            || lease
                .owner()
                .executable_path()
                .file_name()
                .is_none_or(|name| name != expected)
        {
            return Err(NativeTargetError::Executable);
        }
        if lease.service().has_same_executable_parent()? {
            return Err(NativeTargetError::ServiceRoot);
        }
        let identity = engine::identity(&mut connection)
            .await
            .map_err(|_| NativeTargetError::EngineIdentity)?;
        correlate(&lease, &identity).map_err(|_| NativeTargetError::ProcessMapping)?;
        lease.check(&connection)?;
        Ok(Self {
            current: Some(BoundTarget {
                connection,
                lease: Arc::new(lease),
                identity,
            }),
        })
    }

    pub fn identity(&self) -> Result<&InstanceObservation, UnqualifiedProcess> {
        self.current
            .as_ref()
            .map(|current| &current.identity)
            .ok_or(UnqualifiedProcess)
    }

    pub async fn check(&mut self) -> Result<(), UnqualifiedProcess> {
        // Taking the complete binding before the first await makes failure or
        // cancellation terminal. A later change-and-restore cannot revive it.
        let mut bound = self.current.take().ok_or(UnqualifiedProcess)?;
        bound.lease.check(&bound.connection)?;
        if bound.lease.service().has_same_executable_parent()? {
            return Err(UnqualifiedProcess);
        }
        let current = engine::identity(&mut bound.connection)
            .await
            .map_err(|_| UnqualifiedProcess)?;
        if current != bound.identity {
            return Err(UnqualifiedProcess);
        }
        correlate(&bound.lease, &current)?;
        bound.lease.check(&bound.connection)?;
        self.current = Some(bound);
        Ok(())
    }

    pub async fn same_instance(&mut self, other: &mut Self) -> Result<bool, UnqualifiedProcess> {
        self.check().await?;
        other.check().await?;
        // Different databases/credentials and DNS aliases do not change the
        // service process. Conversely, cloned disk IDs do not merge two
        // independently established native instances into one identity.
        self.current
            .as_ref()
            .ok_or(UnqualifiedProcess)?
            .lease
            .service()
            .same_process(
                other
                    .current
                    .as_ref()
                    .ok_or(UnqualifiedProcess)?
                    .lease
                    .service(),
            )
    }
}

fn correlate(
    lease: &SocketOwnerLease,
    identity: &InstanceObservation,
) -> Result<(), UnqualifiedProcess> {
    match identity.process {
        BackendProcess::NativePid(pid) if pid.get() != lease.owner().namespace_pid() => {
            Err(UnqualifiedProcess)
        }
        BackendProcess::NativePid(_) | BackendProcess::RuntimeOnly => Ok(()),
    }
}

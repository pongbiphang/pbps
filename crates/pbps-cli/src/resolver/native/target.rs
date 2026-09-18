//! A direct native-Linux target connection, retaining its TLS and process
//! leases together. Apply can re-establish this without invoking Docker.

use super::executables;
use super::{SocketOwnerLease, UnqualifiedProcess};
use pbps_db::resolver::environment::{DatabaseRecipe, EnvironmentFacts};
use pbps_db::resolver::{BackendProcess, InstanceObservation};
use pbps_db::transport::PeerVerifiedConn;
use pbps_pg::resolver::authorization::AuthorizationContext;
use std::sync::{Arc, Weak};

#[path = "target_engine.rs"]
mod engine;

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;

pub struct NativeTarget {
    current: Option<BoundTarget>,
}

/// What reading the target's analysis-scope facts refused. A catalog read
/// and a kernel read fail differently and a caller needs to tell them apart.
#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("the target binding changed or became unreadable while reading its environment")]
    Binding,
    #[error("the target's analysis-scope catalog facts could not be read: {0}")]
    Catalog(pbps_db::DbError),
    #[error("the target engine's executable content could not be read")]
    Executables,
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
/// Names which reading of the target binding refused.
///
/// `UnqualifiedProcess` is one value for every step here, which is a fine
/// answer for a caller and no answer at all for anyone reading a fixture
/// whose containers are already gone. `docker/session.rs` reports its
/// startup stages the same way, and for the same reason.
fn stage(name: &'static str) -> impl Fn(UnqualifiedProcess) -> UnqualifiedProcess {
    move |error| {
        #[cfg(test)]
        eprintln!("native target check stage={name}");
        let _ = name;
        error
    }
}

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

    /// The `CREATE DATABASE` recipe that reproduces the target's encoding and
    /// locale on the scratch server, read before any scratch database exists.
    /// Catalog-only: no executables and no visibility, because a recipe needs
    /// neither.
    pub async fn database_recipe(&mut self) -> Result<DatabaseRecipe, EnvironmentError> {
        self.check().await.map_err(|_| EnvironmentError::Binding)?;
        let bound = self.current.as_mut().ok_or(EnvironmentError::Binding)?;
        // SQL Server's scratch database creation does not consume a recipe yet
        // (reproducing its collation is #611), and its scope reader is not
        // implemented, so it takes the neutral recipe rather than erroring.
        if bound.connection.driver() != pbps_db::Driver::Postgres {
            return Ok(DatabaseRecipe::neutral());
        }
        let catalog = engine::environment(&mut bound.connection, &[], &[])
            .await
            .map_err(EnvironmentError::Catalog)?;
        self.check().await.map_err(|_| EnvironmentError::Binding)?;
        DatabaseRecipe::from_catalog(&catalog)
            .map_err(|error| EnvironmentError::Catalog(pbps_db::DbError::BadRow(error.to_string())))
    }

    /// The analysis-scope facts of the target as its own deployer sees them,
    /// and that deployer's authorization context over `authorization_schemas`:
    /// both read over the connection in one catalog snapshot, so the sealed
    /// scope never holds half of a change committed between them (finding on
    /// #688). The content of the engine executable and the native libraries
    /// its extensions name is read from the connected backend by the native
    /// observer — kernel state, outside any snapshot. Bracketed by the
    /// ordinary binding check, so a facts read is of the same qualified
    /// backend as everything else (ADR-0016 §5, §23; SPEC §9.3.3).
    pub async fn scope_facts(
        &mut self,
        schemas: &[String],
        write_path_extras: &[String],
        authorization_schemas: &[String],
    ) -> Result<(EnvironmentFacts, AuthorizationContext), EnvironmentError> {
        self.check().await.map_err(|_| EnvironmentError::Binding)?;
        let bound = self.current.as_mut().ok_or(EnvironmentError::Binding)?;
        let (catalog, authorization) = engine::scope_facts(
            &mut bound.connection,
            schemas,
            write_path_extras,
            authorization_schemas,
        )
        .await
        .map_err(EnvironmentError::Catalog)?;
        let required = executables::required_libraries(&catalog);
        let library_path = catalog
            .settings
            .get("dynamic_library_path")
            .map(|fact| fact.value.clone())
            .unwrap_or_else(|| "$libdir".to_owned());
        // The backend, not the postmaster: `session_preload_libraries` and
        // `local_preload_libraries` are loaded into the connected backend, so
        // hashing the service process would miss them (finding on #610).
        let set = executables::executables(bound.lease.owner(), &required, &library_path)
            .map_err(|_| EnvironmentError::Executables)?;
        self.check().await.map_err(|_| EnvironmentError::Binding)?;
        Ok((
            EnvironmentFacts {
                catalog,
                executables: set,
            },
            authorization,
        ))
    }

    pub async fn check(&mut self) -> Result<(), UnqualifiedProcess> {
        // Taking the complete binding before the first await makes failure or
        // cancellation terminal. A later change-and-restore cannot revive it.
        let mut bound = self.current.take().ok_or(UnqualifiedProcess)?;
        bound
            .lease
            .check(&bound.connection)
            .map_err(stage("lease"))?;
        if bound
            .lease
            .service()
            .has_same_executable_parent()
            .map_err(stage("service-root-unreadable"))?
        {
            return Err(stage("service-root")(UnqualifiedProcess));
        }
        let current = engine::identity(&mut bound.connection)
            .await
            .map_err(|_| stage("identity-read")(UnqualifiedProcess))?;
        if current != bound.identity {
            return Err(stage("identity-changed")(UnqualifiedProcess));
        }
        correlate(&bound.lease, &current).map_err(stage("correlate"))?;
        bound
            .lease
            .check(&bound.connection)
            .map_err(stage("lease-after"))?;
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

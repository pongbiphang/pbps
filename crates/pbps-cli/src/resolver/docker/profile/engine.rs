//! Engine routing for the fixed, source-free Linux bootstrap.
//!
//! SQL reconstruction remains in the engine crates. These commands create
//! only the disposable engine installation; they consume no declarations.

use pbps_db::Driver;

/// The only measured PostgreSQL image storage layouts. This selects fixed
/// provisioning commands; the running engine and its content are checked
/// separately after startup.
#[derive(Clone, Copy)]
pub(in crate::resolver::docker) enum PostgresLayout {
    Root,
    Data,
}

#[derive(Clone, Copy)]
pub(in crate::resolver::docker) enum Layout {
    Postgres(PostgresLayout),
    Mssql,
}

impl Layout {
    fn storage_path(self) -> &'static str {
        match self {
            Self::Postgres(PostgresLayout::Root) => "/var/lib/postgresql",
            Self::Postgres(PostgresLayout::Data) => "/var/lib/postgresql/data",
            Self::Mssql => "/var/opt/mssql",
        }
    }
}

pub(in crate::resolver::docker) fn workload_limits_for(
    layout: Layout,
) -> crate::resolver::native::ExecutionProfile {
    let storage_bytes = match layout {
        Layout::Postgres(_) => 268435456,
        Layout::Mssql => 1073741824,
    };
    crate::resolver::native::ExecutionProfile {
        memory: 3221225472,
        nano_cpus: 2000000000,
        pids: 512,
        storage_path: layout.storage_path(),
        storage_bytes,
    }
}

pub(in crate::resolver::docker) fn control_limits_for(
    layout: Layout,
) -> crate::resolver::native::ExecutionProfile {
    let mut limits = workload_limits_for(layout);
    limits.memory = 134217728;
    limits.nano_cpus = 500000000;
    limits.pids = 32;
    limits
}

// Existing PG18/SQL Server fixture helpers deliberately keep their original
// fixed profiles. Production always carries the selected layout explicitly.
#[cfg(test)]
pub(in crate::resolver::docker) fn workload_limits(
    driver: Driver,
) -> crate::resolver::native::ExecutionProfile {
    workload_limits_for(match driver {
        Driver::Postgres => Layout::Postgres(PostgresLayout::Root),
        Driver::Mssql => Layout::Mssql,
    })
}

#[cfg(test)]
pub(in crate::resolver::docker) fn control_limits(
    driver: Driver,
) -> crate::resolver::native::ExecutionProfile {
    control_limits_for(match driver {
        Driver::Postgres => Layout::Postgres(PostgresLayout::Root),
        Driver::Mssql => Layout::Mssql,
    })
}

pub(in crate::resolver::docker) fn private_channel_profile(
    driver: Driver,
) -> crate::resolver::native::PrivateChannelProfile {
    use crate::resolver::native::{PrivateChannelProfile, WorkloadPrivileges};
    match driver {
        Driver::Postgres => PrivateChannelProfile {
            executable: "postgres",
            privileges: WorkloadPrivileges {
                uid: 999,
                gid: 999,
                capabilities: 0,
            },
            port: 5432,
        },
        Driver::Mssql => PrivateChannelProfile {
            executable: "sqlservr",
            privileges: WorkloadPrivileges {
                uid: 10001,
                gid: 0,
                capabilities: 0x400,
            },
            port: 1433,
        },
    }
}

pub(in crate::resolver::docker) fn control_program(driver: Driver) -> &'static str {
    // Waiting for this one fixed line permits attach after container start.
    // Once connected, EOF ends both directions; there is no backend reconnect.
    match driver {
        Driver::Postgres => {
            "read -r ready; test \"$ready\" = pbps-control-v1; for attempt in {1..60}; do if { exec 3<>/dev/tcp/127.0.0.1/5432; } 2>/dev/null; then break; fi; sleep 1; done; test -e /proc/self/fd/3; cat <&3 & reader=$!; cat <&0 >&3 & writer=$!; wait -n; kill \"$reader\" \"$writer\" 2>/dev/null || true; wait || true"
        }
        Driver::Mssql => {
            "read -r ready; test \"$ready\" = pbps-control-v1; for attempt in {1..60}; do if { exec 3<>/dev/tcp/127.0.0.1/1433; } 2>/dev/null; then break; fi; sleep 1; done; test -e /proc/self/fd/3; cat <&3 & reader=$!; cat <&0 >&3 & writer=$!; wait -n; kill \"$reader\" \"$writer\" 2>/dev/null || true; wait || true"
        }
    }
}

pub(in crate::resolver::docker) fn login(
    driver: Driver,
    password: String,
) -> pbps_db::transport::StreamLogin {
    let (user, database) = match driver {
        Driver::Postgres => ("postgres", "postgres"),
        Driver::Mssql => ("sa", "master"),
    };
    pbps_db::transport::StreamLogin {
        user: user.into(),
        password,
        database: database.into(),
    }
}

pub(in crate::resolver::docker) async fn identity(
    connection: &mut pbps_db::transport::StreamConn,
) -> Result<pbps_db::resolver::InstanceObservation, pbps_db::DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::instance_identity(connection).await,
        Driver::Mssql => pbps_mssql::resolver::instance_identity(connection).await,
    }
}

pub(super) struct Bootstrap {
    pub privileges: crate::resolver::native::WorkloadPrivileges,
    pub storage_path: &'static str,
    pub storage_options: &'static str,
    pub program: String,
    pub environment: Vec<String>,
}

pub(super) fn bootstrap(layout: Layout, password: &str) -> Bootstrap {
    let driver = match layout {
        Layout::Postgres(_) => Driver::Postgres,
        Layout::Mssql => Driver::Mssql,
    };
    let privileges = private_channel_profile(driver).privileges;
    match layout {
        Layout::Postgres(postgres) => {
            let storage = layout.storage_path();
            let bin = match postgres {
                PostgresLayout::Root => "/usr/lib/postgresql/18/bin",
                PostgresLayout::Data => "/usr/lib/postgresql/16/bin",
            };
            Bootstrap {
            privileges,
            storage_path: storage,
            storage_options: "rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700",
            program: format!(
                "umask 077; printf '%s' \"$PBPS_BOOTSTRAP_PASSWORD\" > {storage}/password; unset PBPS_BOOTSTRAP_PASSWORD; {bin}/initdb -D {storage}/run-data --auth-local=scram-sha-256 --auth-host=scram-sha-256 --pwfile={storage}/password >/dev/null 2>&1; rm {storage}/password; {bin}/postgres -D {storage}/run-data -c listen_addresses=127.0.0.1 -c unix_socket_directories= >/dev/null 2>&1 & engine=$!; until test \"$(awk 'NR == 8 {{print $1}}' {storage}/run-data/postmaster.pid 2>/dev/null)\" = ready; do kill -0 \"$engine\"; sleep 0.1; done; printf 'pbps-engine-ready-v1\\n'; wait \"$engine\""
            ),
            environment: vec![
                "PATH=/usr/bin:/bin".into(),
                "LANG=C.UTF-8".into(),
                format!("PBPS_BOOTSTRAP_PASSWORD={password}"),
            ],
            }
        }
        Layout::Mssql => Bootstrap {
            privileges,
            storage_path: "/var/opt/mssql",
            storage_options: "rw,nosuid,nodev,noexec,size=1073741824,uid=10001,gid=0,mode=700",
            program: "/opt/mssql/bin/sqlservr >/dev/null 2>&1 & engine=$!; until grep -q 'SQL Server is now ready for client connections' /var/opt/mssql/log/errorlog 2>/dev/null; do kill -0 \"$engine\"; sleep 0.1; done; printf 'pbps-engine-ready-v1\\n'; wait \"$engine\"".into(),
            environment: vec![
                "PATH=/usr/bin:/bin".into(),
                "ACCEPT_EULA=Y".into(),
                "MSSQL_MEMORY_LIMIT_MB=2048".into(),
                format!("MSSQL_SA_PASSWORD={password}"),
            ],
        },
    }
}

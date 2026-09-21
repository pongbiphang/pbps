//! Engine routing for the fixed, source-free Linux bootstrap.
//!
//! SQL reconstruction remains in the engine crates. These commands create
//! only the disposable engine installation; they consume no declarations.

use pbps_db::Driver;

pub(in crate::resolver::docker) fn workload_limits(
    driver: Driver,
) -> crate::resolver::native::ExecutionProfile {
    let (storage_path, storage_bytes) = match driver {
        Driver::Postgres => ("/var/lib/postgresql", 268435456),
        Driver::Mssql => ("/var/opt/mssql", 1073741824),
    };
    crate::resolver::native::ExecutionProfile {
        memory: 3221225472,
        nano_cpus: 2000000000,
        pids: 512,
        storage_path,
        storage_bytes,
    }
}

pub(in crate::resolver::docker) fn control_limits(
    driver: Driver,
) -> crate::resolver::native::ExecutionProfile {
    let mut limits = workload_limits(driver);
    limits.memory = 134217728;
    limits.nano_cpus = 500000000;
    limits.pids = 32;
    limits
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
    pub program: &'static str,
    pub environment: Vec<String>,
}

pub(super) fn bootstrap(driver: Driver, password: &str) -> Bootstrap {
    let privileges = private_channel_profile(driver).privileges;
    match driver {
        Driver::Postgres => Bootstrap {
            privileges,
            storage_path: "/var/lib/postgresql",
            storage_options: "rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700",
            program: "umask 077; printf '%s' \"$PBPS_BOOTSTRAP_PASSWORD\" > /var/lib/postgresql/password; unset PBPS_BOOTSTRAP_PASSWORD; /usr/lib/postgresql/18/bin/initdb -D /var/lib/postgresql/run-data --auth-local=scram-sha-256 --auth-host=scram-sha-256 --pwfile=/var/lib/postgresql/password >/dev/null 2>&1; rm /var/lib/postgresql/password; /usr/lib/postgresql/18/bin/postgres -D /var/lib/postgresql/run-data -c listen_addresses=127.0.0.1 -c unix_socket_directories= >/dev/null 2>&1 & engine=$!; until test \"$(awk 'NR == 8 {print $1}' /var/lib/postgresql/run-data/postmaster.pid 2>/dev/null)\" = ready; do kill -0 \"$engine\"; sleep 0.1; done; printf 'pbps-engine-ready-v1\\n'; wait \"$engine\"",
            environment: vec![
                "PATH=/usr/bin:/bin".into(),
                "LANG=C.UTF-8".into(),
                format!("PBPS_BOOTSTRAP_PASSWORD={password}"),
            ],
        },
        Driver::Mssql => Bootstrap {
            privileges,
            storage_path: "/var/opt/mssql",
            storage_options: "rw,nosuid,nodev,noexec,size=1073741824,uid=10001,gid=0,mode=700",
            program: "/opt/mssql/bin/sqlservr >/dev/null 2>&1 & engine=$!; until grep -q 'SQL Server is now ready for client connections' /var/opt/mssql/log/errorlog 2>/dev/null; do kill -0 \"$engine\"; sleep 0.1; done; printf 'pbps-engine-ready-v1\\n'; wait \"$engine\"",
            environment: vec![
                "PATH=/usr/bin:/bin".into(),
                "ACCEPT_EULA=Y".into(),
                "MSSQL_MEMORY_LIMIT_MB=2048".into(),
                format!("MSSQL_SA_PASSWORD={password}"),
            ],
        },
    }
}

//! Fixed launch recipes. A recipe is not a measured runtime admission.

use super::{CandidateImage, Error};
use crate::resolver::native::{FILE_DESCRIPTOR_LIMIT, FORWARDER_PRIVILEGES, WorkloadPrivileges};
use pbps_db::Driver;
use serde_json::{Value, json};

pub(super) mod engine;

#[cfg(test)]
#[path = "profile/launch_tests.rs"]
mod launch_tests;

pub(super) const OWNER_LABEL: &str = "io.pbps.resolver.owner";
pub(super) const PROFILE: &str = "linux-amd64-v1";
pub(super) const LIFETIME_SECS: u64 = 600;

// Derived from Moby v28.3.3's Apache-2.0 default allowlist. Keep its deny-by-
// default and argument/capability restrictions. In particular, deleting only
// connect would leave the compat socketcall entry point available. The process
// inspection calls could steal a control socket or modify another process.
// sendto/sendmsg/sendmmsg allow ordinary responses but mask out MSG_FASTOPEN:
// that flag can connect implicitly without passing through connect(2).
const SECCOMP: &str = include_str!("linux-amd64-v1.json");

pub(crate) struct Launch {
    pub body: Value,
}

impl Launch {
    #[cfg(test)]
    pub fn new(image: &CandidateImage, driver: Driver, owner: &str) -> Result<Self, Error> {
        let password = format!("Pbps!{:032x}", rand::random::<u128>());
        Self::with_password(image, driver, owner, &password)
    }

    pub(super) fn with_password(
        image: &CandidateImage,
        driver: Driver,
        owner: &str,
        password: &str,
    ) -> Result<Self, Error> {
        if image.identity.os != "linux"
            || image.identity.architecture != "amd64"
            || image.identity.variant.is_some()
        {
            return Err(Error::UnsupportedLaunch);
        }
        let bootstrap = engine::bootstrap(driver, password);
        let mut tmpfs = serde_json::Map::new();
        tmpfs.insert(
            "/tmp".into(),
            json!("rw,nosuid,nodev,noexec,size=67108864,mode=1777"),
        );
        tmpfs.insert(
            bootstrap.storage_path.into(),
            json!(bootstrap.storage_options),
        );
        let body = json!({
            "Image": image.identity.image_id,
            "User": "0:0",
            "Entrypoint": ["/usr/bin/timeout"],
            "Cmd": guarded_command(bootstrap.privileges, LIFETIME_SECS, bootstrap.program)?,
            "Env": isolated_environment(image, bootstrap.environment)?,
            "Healthcheck": {"Test": ["NONE"]},
            "Labels": { OWNER_LABEL: owner, "io.pbps.resolver.profile": PROFILE },
            "WorkingDir": "/",
            "OpenStdin": false,
            "Tty": false,
            "NetworkDisabled": true,
            "HostConfig": {
                "NetworkMode": "none",
                "ReadonlyRootfs": true,
                "AutoRemove": true,
                "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
                "CapDrop": ["ALL"],
                "CapAdd": bootstrap_capabilities(bootstrap.privileges),
                "SecurityOpt": ["no-new-privileges", format!("seccomp={SECCOMP}")],
                "Memory": 3221225472u64,
                "MemorySwap": 3221225472u64,
                "NanoCpus": 2000000000u64,
                "PidsLimit": 512,
                "ShmSize": 67108864,
                "Tmpfs": tmpfs,
                "LogConfig": {"Type": "none", "Config": {}},
                "CgroupnsMode": "private",
                "MaskedPaths": ["/proc/asound", "/proc/acpi", "/proc/interrupts", "/proc/kcore", "/proc/keys", "/proc/latency_stats", "/proc/timer_list", "/proc/timer_stats", "/proc/sched_debug", "/proc/scsi", "/sys"],
                "ReadonlyPaths": ["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"],
                "Ulimits": [{"Name":"nofile", "Soft":FILE_DESCRIPTOR_LIMIT, "Hard":FILE_DESCRIPTOR_LIMIT}]
            }
        });
        Ok(Self { body })
    }

    /// `lifetime_secs` is the root guard's deadline. The Docker profile passes
    /// its own run lifetime; a supplied server's run has a longer one, and a
    /// forwarder that died before the run did would end the run for nothing.
    pub(crate) fn control(
        image: &CandidateImage,
        driver: Driver,
        owner: &str,
        workload: &str,
        lifetime_secs: u64,
    ) -> Result<Self, Error> {
        if workload.len() != 64
            || !workload
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Profile);
        }
        let mut launch = Self::with_password(image, driver, owner, "")?;
        // Only this fixed trusted forwarder may initiate a connection. It has
        // its own PID/mount namespace and shares only the run's route-less
        // network namespace. Stdin becomes protocol bytes, never shell code.
        let mut policy: Value = serde_json::from_str(SECCOMP).map_err(|_| Error::Profile)?;
        policy["syscalls"]
            .as_array_mut()
            .ok_or(Error::Profile)?
            .push(json!({"names":["connect"],"action":"SCMP_ACT_ALLOW"}));
        let body = &mut launch.body;
        body["Cmd"] = json!(guarded_command(
            FORWARDER_PRIVILEGES,
            lifetime_secs,
            engine::control_program(driver)
        )?);
        body["Env"] = json!(isolated_environment(
            image,
            vec!["PATH=/usr/bin:/bin".into(), "LANG=C.UTF-8".into()]
        )?);
        body["OpenStdin"] = json!(true);
        body["StdinOnce"] = json!(true);
        body["AttachStdin"] = json!(true);
        body["AttachStdout"] = json!(true);
        body["AttachStderr"] = json!(true);
        body["NetworkDisabled"] = json!(false);
        body["HostConfig"]["NetworkMode"] = json!(format!("container:{workload}"));
        body["HostConfig"]["SecurityOpt"] =
            json!(["no-new-privileges", format!("seccomp={policy}")]);
        body["HostConfig"]["CapAdd"] = json!(bootstrap_capabilities(FORWARDER_PRIVILEGES));
        body["HostConfig"]["Memory"] = json!(134217728);
        body["HostConfig"]["MemorySwap"] = json!(134217728);
        body["HostConfig"]["NanoCpus"] = json!(500000000);
        body["HostConfig"]["PidsLimit"] = json!(32);
        Ok(launch)
    }

    pub(super) fn reserved(
        image: &CandidateImage,
        driver: Driver,
        owner: &str,
        password: &str,
    ) -> Result<Self, Error> {
        let mut launch = Self::with_password(image, driver, owner, password)?;
        let program = launch.body["Cmd"]
            .as_array_mut()
            .ok_or(Error::Profile)?
            .last_mut()
            .ok_or(Error::Profile)?;
        *program = json!(format!(
            "IFS= read -r probe; test \"$probe\" = pbps-bootstrap-probe-v1; printf 'pbps-bootstrap-ready-v1\\n'; IFS= read -r start; test \"$start\" = pbps-bootstrap-start-v1; {}",
            program.as_str().ok_or(Error::Profile)?
        ));
        for key in [
            "OpenStdin",
            "StdinOnce",
            "AttachStdin",
            "AttachStdout",
            "AttachStderr",
        ] {
            launch.body[key] = json!(true);
        }
        Ok(launch)
    }

    /// Docker's reported configuration must match the submitted recipe. This
    /// is an early rejection check, not proof that the kernel enforces it.
    /// Runtime qualification and live negative controls remain necessary.
    pub fn check_configuration(&self, observed: &Value) -> Result<(), Error> {
        let config = &observed["Config"];
        for key in [
            "User",
            "Entrypoint",
            "Cmd",
            "WorkingDir",
            "OpenStdin",
            "Tty",
            "NetworkDisabled",
        ] {
            // Docker omits NetworkDisabled when false (Moby Config omitempty).
            let omitted_false =
                key == "NetworkDisabled" && config.get(key).is_none() && self.body[key] == false;
            if config[key] != self.body[key] && !omitted_false {
                return Err(Error::RuntimeChanged);
            }
        }
        if config["Healthcheck"]["Test"] != json!(["NONE"]) {
            return Err(Error::RuntimeChanged);
        }
        // Each inherited variable was explicitly cleared before applying the
        // fixed bootstrap values. Extra entries must not reintroduce startup
        // hooks or loader configuration before the admission gate.
        let environment = config["Env"].as_array().ok_or(Error::RuntimeChanged)?;
        if environment.len()
            != self.body["Env"]
                .as_array()
                .ok_or(Error::RuntimeChanged)?
                .len()
        {
            return Err(Error::RuntimeChanged);
        }
        for expected in self.body["Env"].as_array().ok_or(Error::RuntimeChanged)? {
            let expected = expected.as_str().ok_or(Error::RuntimeChanged)?;
            let name = expected.split('=').next().ok_or(Error::RuntimeChanged)?;
            let matching: Vec<_> = environment
                .iter()
                .filter_map(Value::as_str)
                .filter(|entry| entry.split('=').next() == Some(name))
                .collect();
            if matching != [expected] {
                return Err(Error::RuntimeChanged);
            }
        }
        for key in ["StdinOnce", "AttachStdin", "AttachStdout", "AttachStderr"] {
            if self
                .body
                .get(key)
                .is_some_and(|expected| config[key] != *expected)
            {
                return Err(Error::RuntimeChanged);
            }
        }
        let host = &observed["HostConfig"];
        for (key, value) in self.body["HostConfig"]
            .as_object()
            .ok_or(Error::RuntimeChanged)?
        {
            if host[key] != *value {
                return Err(Error::RuntimeChanged);
            }
        }
        if host["Privileged"] != false || host["PublishAllPorts"] != false {
            return Err(Error::RuntimeChanged);
        }
        for key in [
            "Binds",
            "Mounts",
            "Devices",
            "DeviceRequests",
            "DeviceCgroupRules",
            "PortBindings",
            "VolumesFrom",
            "Links",
            "ExtraHosts",
        ] {
            match &host[key] {
                Value::Null => (),
                Value::Array(values) if values.is_empty() => (),
                Value::Object(values) if values.is_empty() => (),
                Value::Bool(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Array(_)
                | Value::Object(_) => return Err(Error::RuntimeChanged),
            }
        }
        for key in ["PidMode", "IpcMode", "UTSMode", "UsernsMode"] {
            let value = host[key].as_str().ok_or(Error::RuntimeChanged)?;
            if !(value.is_empty() || (key == "IpcMode" && value == "private")) {
                return Err(Error::RuntimeChanged);
            }
        }
        // Image-declared volumes must not silently introduce unbounded disk
        // storage. The supported writable paths are all explicit tmpfs mounts.
        for mount in observed["Mounts"].as_array().ok_or(Error::RuntimeChanged)? {
            if mount["Type"] != "tmpfs"
                || mount["Destination"]
                    .as_str()
                    .is_none_or(|path| self.body["HostConfig"]["Tmpfs"].get(path).is_none())
            {
                return Err(Error::RuntimeChanged);
            }
        }
        Ok(())
    }
}

fn bootstrap_capabilities(privileges: WorkloadPrivileges) -> Vec<&'static str> {
    // timeout forks setpriv before the child changes identity. The parent
    // retains this bootstrap ceiling and needs KILL for the different UID;
    // the child irreversibly drops to the separate workload ceiling (531).
    let mut capabilities = vec!["SETUID", "SETGID", "SETPCAP", "KILL"];
    if privileges.capabilities == 0x400 {
        capabilities.push("NET_BIND_SERVICE");
    }
    capabilities
}

fn guarded_command(
    privileges: WorkloadPrivileges,
    lifetime_secs: u64,
    program: &str,
) -> Result<Vec<String>, Error> {
    let capabilities = match privileges.capabilities {
        0 => "-all",
        // The tested SQL Server executable carries this file capability;
        // removing it from the bounding set makes exec fail with EPERM.
        0x400 => "-all,+net_bind_service",
        _ => return Err(Error::Profile),
    };
    Ok(vec![
        "--signal=KILL".into(),
        format!("{lifetime_secs}s"),
        "/usr/bin/setpriv".into(),
        format!("--reuid={}", privileges.uid),
        format!("--regid={}", privileges.gid),
        "--clear-groups".into(),
        format!("--bounding-set={capabilities}"),
        format!("--inh-caps={capabilities}"),
        format!("--ambient-caps={capabilities}"),
        "/bin/bash".into(),
        "-ec".into(),
        program.into(),
    ])
}

fn isolated_environment(
    image: &CandidateImage,
    overrides: Vec<String>,
) -> Result<Vec<String>, Error> {
    let keys = image
        .environment_keys
        .as_ref()
        .ok_or(Error::UnsupportedLaunch)?;
    // Docker API entries without '=' remove inherited variables. Empty values
    // are different: e.g. SQL Server treats MSSQL_PID= as an invalid edition.
    // Unset loader/shell hooks before even the root lifetime guard executes.
    let mut environment: std::collections::BTreeMap<String, Option<String>> =
        keys.iter().map(|key| (key.clone(), None)).collect();
    for entry in overrides {
        let (key, value) = entry.split_once('=').ok_or(Error::Profile)?;
        environment.insert(key.to_owned(), Some(value.to_owned()));
    }
    Ok(environment
        .into_iter()
        .map(|(key, value)| match value {
            Some(value) => format!("{key}={value}"),
            None => key,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_inherited_environment_cannot_select_a_launch_profile() {
        let mut image: CandidateImage =
            serde_json::from_value::<super::super::ImageInspect>(json!({
                "Id": format!("sha256:{}", "a".repeat(64)),
                "Os": "linux", "Architecture": "amd64"
            }))
            .unwrap()
            .try_into()
            .unwrap();
        assert!(matches!(
            Launch::new(&image, Driver::Postgres, "owner"),
            Err(Error::UnsupportedLaunch)
        ));
        image.environment_keys = Some(vec!["BASH_ENV".into(), "LD_PRELOAD".into(), "PATH".into()]);
        for driver in [Driver::Postgres, Driver::Mssql] {
            let launch = Launch::reserved(&image, driver, "owner", "synthetic-password").unwrap();
            let env = launch.body["Env"].as_array().unwrap();
            assert!(env.contains(&json!("BASH_ENV")));
            assert!(env.contains(&json!("LD_PRELOAD")));
            assert!(env.contains(&json!("PATH=/usr/bin:/bin")));
            let control =
                Launch::control(&image, driver, "owner", &"a".repeat(64), LIFETIME_SECS).unwrap();
            assert!(
                control.body["Env"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("BASH_ENV"))
            );
        }
    }

    #[test]
    fn native_and_compat_network_and_process_bypasses_are_not_allowed() {
        let policy: Value = serde_json::from_str(SECCOMP).unwrap();
        assert_eq!(policy["defaultAction"], "SCMP_ACT_ERRNO");
        for rule in policy["syscalls"].as_array().unwrap() {
            if rule["action"] != "SCMP_ACT_ALLOW" {
                continue;
            }
            for forbidden in [
                "connect",
                "socketcall",
                "ptrace",
                "process_vm_readv",
                "process_vm_writev",
                "pidfd_getfd",
                "unshare",
                "io_uring_setup",
            ] {
                assert!(
                    !rule["names"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(forbidden)),
                    "{forbidden} must not bypass the run's channel boundary"
                );
            }
        }
    }
}

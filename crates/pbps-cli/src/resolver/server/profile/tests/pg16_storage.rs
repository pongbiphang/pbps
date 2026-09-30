use super::*;

const STORAGE: &str = "/var/lib/postgresql/data";

fn pg16() -> &'static ServerProfile {
    supported("linux-dedicated-pg16-v1", Driver::Postgres)
        .expect("the measured PG16 layout is explicitly implemented")
}

// Complete Docker 29.8.1 kernel table from the primary #1302 child-storage
// measurement. Only overlay backing paths are shortened; no row is dropped.
// This pins a recorded layout, not a claim that this unit test measured Docker.
const PG16_KERNEL: &str = r#"1568 1469 0:161 / / ro,relatime - overlay overlay rw,lowerdir=/l/a,upperdir=/u/diff,workdir=/u/work
1570 1568 0:170 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
1571 1568 0:171 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1572 1571 0:172 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=5,mode=620,ptmxmode=666
1573 1568 0:173 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro
1574 1573 0:23 /system.slice/docker-1e0bf73d8279307e8ae6ddca5f5119f69b8ba1bbc1573aa1bb2f9684d23f17e8.scope /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw,nsdelegate
1575 1571 0:168 / /dev/mqueue rw,nosuid,nodev,noexec,relatime - mqueue mqueue rw
1576 1571 0:174 / /dev/shm rw,nosuid,nodev,noexec,relatime - tmpfs shm rw,size=65536k
1577 1568 0:175 / /run rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k,mode=755
1578 1568 0:176 / /tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k
1579 1568 8:48 /var/lib/pbps-resolver-fixture/containers/1e0bf73d8279307e8ae6ddca5f5119f69b8ba1bbc1573aa1bb2f9684d23f17e8/hostname /etc/hostname ro,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro,data=ordered
1580 1568 8:48 /var/lib/pbps-resolver-fixture/containers/1e0bf73d8279307e8ae6ddca5f5119f69b8ba1bbc1573aa1bb2f9684d23f17e8/hosts /etc/hosts ro,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro,data=ordered
1581 1568 8:48 /var/lib/pbps-resolver-fixture/containers/1e0bf73d8279307e8ae6ddca5f5119f69b8ba1bbc1573aa1bb2f9684d23f17e8/resolv.conf /etc/resolv.conf ro,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro,data=ordered
1582 1568 0:177 / /var/tmp rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=65536k
1583 1568 0:178 / /var/lib/postgresql/data rw,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,size=262144k,mode=700,uid=999,gid=999
1496 1570 0:170 /bus /proc/bus ro,nosuid,nodev,noexec,relatime - proc proc rw
1530 1570 0:170 /fs /proc/fs ro,nosuid,nodev,noexec,relatime - proc proc rw
1531 1570 0:170 /irq /proc/irq ro,nosuid,nodev,noexec,relatime - proc proc rw
1532 1570 0:170 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw
1533 1570 0:170 /sysrq-trigger /proc/sysrq-trigger ro,nosuid,nodev,noexec,relatime - proc proc rw
1534 1570 0:179 / /proc/acpi ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1
1535 1570 0:171 /null /proc/interrupts rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1536 1570 0:171 /null /proc/kcore rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1537 1570 0:171 /null /proc/keys rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1538 1570 0:171 /null /proc/latency_stats rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1539 1570 0:179 / /proc/scsi ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1
1540 1570 0:171 /null /proc/timer_list rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1541 1573 0:179 / /sys/firmware ro,relatime - tmpfs tmpfs ro,size=4k,nr_inodes=1
"#;

#[test]
fn the_measured_pg16_child_layout_qualifies_only_its_exact_private_storage() {
    assert_eq!(contained(&rows(PG16_KERNEL), pg16()), Ok(()));
    for measured in [DOCKER, PODMAN] {
        assert_eq!(contained(&rows(measured), postgres()), Ok(()));
        assert!(
            contained(&rows(measured), pg16())
                .unwrap_err()
                .starts_with("/var/lib/postgresql ("),
            "the child layout cannot accept the old writable parent"
        );
    }
    assert!(
        contained(&rows(PG16_KERNEL), postgres())
            .unwrap_err()
            .starts_with("/var/lib/postgresql/data ("),
        "a measured child is not an implementation of the old parent layout"
    );
    assert!(supported("linux-dedicated-pg16-v1", Driver::Mssql).is_none());
    let mssql = supported("linux-dedicated-v1", Driver::Mssql).unwrap();
    assert_eq!(
        contained(
            &rows(&DOCKER.replace("/var/lib/postgresql ", "/var/opt/mssql ")),
            mssql
        ),
        Ok(()),
        "adding a PG layout does not change the SQL Server storage boundary"
    );
}

#[test]
fn every_pg16_storage_weakening_remains_a_named_refusal() {
    // The daemon record and the kernel table answer separate questions. An
    // empty readable Mounts array is supported; unreadable is never empty.
    let mut complete = podman_record();
    complete["HostConfig"]["Tmpfs"] = serde_json::json!({
        "/tmp": "rw,nosuid,nodev,noexec,size=67108864,mode=1777",
        "/run": "rw,nosuid,nodev,noexec,size=67108864,mode=755",
        "/var/tmp": "rw,nosuid,nodev,noexec,size=67108864,mode=1777",
        "/var/lib/postgresql/data": "rw,nosuid,nodev,noexec,size=268435456,mode=700,uid=999,gid=999"
    });
    assert_eq!(configuration(&complete), Ok(()));
    let mut absent = complete.clone();
    absent.as_object_mut().unwrap().remove("Mounts");
    assert_eq!(configuration(&absent), Err("Mounts"));
    for mounts in [
        Value::Null,
        serde_json::json!(false),
        serde_json::json!(17),
        serde_json::json!("tmpfs"),
        serde_json::json!({}),
        serde_json::json!([{}]),
        serde_json::json!([null]),
        serde_json::json!([{"Type": null}]),
        serde_json::json!([{"Type": 16}]),
        serde_json::json!([{"Type": "volume", "Name": "anonymous", "Destination": STORAGE}]),
        serde_json::json!([{"Type": "volume", "Name": "host-backed", "Destination": STORAGE}]),
        serde_json::json!([{"Type": "bind", "Source": "/host/data", "Destination": STORAGE}]),
    ] {
        let mut record = complete.clone();
        record["Mounts"] = mounts.clone();
        assert_eq!(configuration(&record), Err("Mounts"), "{mounts}");
    }
    for (key, value) in [
        (
            "Binds",
            serde_json::json!(["/host/data:/var/lib/postgresql/data"]),
        ),
        ("VolumesFrom", serde_json::json!(["another-container"])),
        ("ReadonlyRootfs", serde_json::json!(false)),
    ] {
        let mut record = complete.clone();
        record["HostConfig"][key] = value;
        assert_eq!(configuration(&record), Err(key));
    }
    for changed in ["kind", "root", "rw", "nosuid", "nodev", "noexec"] {
        let mut table = rows(PG16_KERNEL);
        let storage = table.iter_mut().find(|row| row.target == STORAGE).unwrap();
        match changed {
            "kind" => storage.kind = "ext4".into(),
            "root" => storage.root = "/existing-subtree".into(),
            flag => {
                storage.options.remove(flag);
            }
        }
        assert!(
            contained(&table, pg16())
                .unwrap_err()
                .starts_with(&format!("{STORAGE} (")),
            "{changed} must be refused as the actual storage row"
        );
    }
    for target in [
        "/var/lib/postgresql",
        "/var/lib/postgresql/data/extra",
        "/extra",
        "/usr/bin",
    ] {
        let mut table = rows(PG16_KERNEL);
        table.push(entry(&format!(
            "9999 1568 0:999 / {target} rw,nosuid,nodev,noexec - tmpfs tmpfs rw"
        )));
        assert!(
            contained(&table, pg16())
                .unwrap_err()
                .starts_with(&format!("{target} (")),
            "an extra writable row is not certified by an allowed path prefix"
        );
    }
    let mut table = rows(PG16_KERNEL);
    table.push(entry(
        PG16_KERNEL
            .lines()
            .find(|line| line.split_whitespace().nth(4) == Some(STORAGE))
            .unwrap(),
    ));
    assert_eq!(
        contained(&table, pg16()),
        Err(format!("{STORAGE} is mounted more than once"))
    );
    let mut table = rows(PG16_KERNEL);
    table.retain(|row| row.target != STORAGE);
    assert_eq!(
        contained(&table, pg16()),
        Err(format!("{STORAGE} is not mounted"))
    );
    let mut table = rows(PG16_KERNEL);
    let root = table.iter_mut().find(|row| row.target == "/").unwrap();
    root.options.remove("ro");
    root.options.insert("rw".into());
    assert!(contained(&table, pg16()).unwrap_err().starts_with("/ ("));
}

#[test]
fn the_pg16_layout_requires_a_reported_pg16_engine_major() {
    let pg16 = pg16();
    for reported in [160_000, 160_015, 169_999] {
        assert!(
            pg16.accepts_postgres_version(reported),
            "readable PG16 version {reported}"
        );
    }
    // This is the exact production decision admission calls. PG18's image
    // would fail the child-layout mounts first, so this is not a native
    // wrong-major measurement or a connection/result override.
    for reported in [
        180_000,
        150_015,
        159_999,
        170_000,
        179_999,
        0,
        -1,
        i64::MIN,
        i64::MAX,
    ] {
        assert!(
            !pg16.accepts_postgres_version(reported),
            "the PG16 child layout must refuse reported version {reported}"
        );
    }
    for legacy in [
        postgres(),
        supported("linux-dedicated-v1", Driver::Mssql).unwrap(),
    ] {
        for reported in [150_015, 160_015, 170_000, 180_006] {
            assert!(
                legacy.accepts_postgres_version(reported),
                "legacy profiles add no version restriction"
            );
        }
    }
}

use super::*;
use crate::plan_test::execution::{Control, ms};
use crate::plan_test::report::{ManifestWrite, Phase, Repin, Status, TestData};
use pbps_config::test_manifest::CommitId;

async fn compared(original: &Snapshot, proposed: &Snapshot, control: &mut Control) -> TestData {
    let mut data = TestData::for_repin();
    data.phases = vec![
        Phase::offline("compare_rows", "all managed tables"),
        Phase::offline("compare_allocators", "every sequence"),
    ];
    data.repin = Some(Repin {
        fixture: "sample".into(),
        path: "tests/sample.sql".into(),
        from: CommitId::parse(&"1".repeat(40)).unwrap(),
        to: CommitId::parse(&"2".repeat(40)).unwrap(),
        original_inputs: Vec::new(),
        proposed_inputs: Vec::new(),
        sides: Vec::new(),
        tables: Vec::new(),
        allocators: Vec::new(),
        manifest: ManifestWrite {
            status: "not_attempted".into(),
            old_sha256: "0".repeat(64),
            new_sha256: None,
            reason: None,
        },
    });
    crate::plan_test::repin::compare(
        &mut data,
        0,
        original,
        proposed,
        control,
        ms(pbps_config::test_manifest::Timeouts::default().run_ms),
    )
    .await;
    data
}

fn rows(columns: &[&str], data: &[(&[Option<&str>], u64)]) -> Rows {
    Rows {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        rows: data
            .iter()
            .map(|(row, n)| (row.iter().map(|c| c.map(str::to_owned)).collect(), *n))
            .collect(),
    }
}

fn snapshot(table: Rows, allocators: &[(&str, &str)]) -> Snapshot {
    Snapshot {
        tables: BTreeMap::from([("app.t".to_owned(), table)]),
        allocators: allocators
            .iter()
            .map(|(name, next)| {
                (
                    (*name).to_owned(),
                    Allocator {
                        name: (*name).to_owned(),
                        used_by: Some("app.t.id".into()),
                        next: (*next).to_owned(),
                    },
                )
            })
            .collect(),
    }
}

async fn equal(a: &Snapshot, b: &Snapshot) -> bool {
    let (tables, allocators) = compare(a, b).await;
    tables.iter().all(|t| t.equal) && allocators.iter().all(|x| x.equal)
}

#[tokio::test]
async fn identical_rows_and_allocators_are_equal() {
    let a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    let b = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    assert!(equal(&a, &b).await);
}

#[tokio::test]
async fn equal_rows_with_a_different_next_value_are_not_equal() {
    // The measured counterexample: (1,kept),(2,discarded) then DELETE 2, against
    // (1,kept) alone. Their rows are equal; the next ids are 3 and 2.
    let a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    let b = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "2")]);
    let (tables, allocators) = compare(&a, &b).await;
    assert!(tables.iter().all(|t| t.equal));
    assert!(!allocators[0].equal);
    assert_eq!(allocators[0].original_next.as_deref(), Some("3"));
    assert_eq!(allocators[0].proposed_next.as_deref(), Some("2"));
}

#[tokio::test]
async fn null_empty_duplicate_case_and_space_are_all_differences() {
    let base = || rows(&["v"], &[(&[Some("a")], 1)]);
    let none = snapshot(base(), &[]);
    for other in [
        rows(&["v"], &[(&[None], 1)]),
        rows(&["v"], &[(&[Some("")], 1)]),
        rows(&["v"], &[(&[Some("a")], 2)]),
        rows(&["v"], &[(&[Some("A")], 1)]),
        rows(&["v"], &[(&[Some("a ")], 1)]),
        rows(&["v"], &[]),
        rows(&["v", "w"], &[(&[Some("a"), None], 1)]),
    ] {
        assert!(!equal(&none, &snapshot(other, &[])).await);
    }
}

#[tokio::test]
async fn a_table_or_sequence_only_one_side_has_is_a_difference() {
    let a = snapshot(rows(&["id"], &[]), &[("app.s", "1")]);
    let mut b = snapshot(rows(&["id"], &[]), &[]);
    assert!(!equal(&a, &b).await);
    b.tables.insert("app.u".into(), rows(&["id"], &[]));
    b.allocators.insert(
        "app.s".into(),
        Allocator {
            name: "app.s".into(),
            used_by: None,
            next: "1".into(),
        },
    );
    assert!(!equal(&a, &b).await);
}

#[tokio::test]
async fn shown_differences_are_bounded_but_the_equality_test_is_not() {
    let mut big = Rows {
        columns: vec!["n".into()],
        rows: BTreeMap::new(),
    };
    for i in 0..(SHOWN + 5) {
        big.rows.insert(vec![Some(i.to_string())], 1);
    }
    let a = snapshot(big, &[]);
    let b = snapshot(rows(&["n"], &[]), &[]);
    let (tables, _) = compare(&a, &b).await;
    assert!(!tables[0].equal);
    assert_eq!(tables[0].differences.len(), SHOWN);
    assert!(tables[0].truncated);
    assert_eq!(tables[0].original_rows, (SHOWN + 5) as u64);
}

fn owned(name: &str, owner: Option<&str>, next: &str) -> (String, Allocator) {
    let key = owner.unwrap_or(name).to_owned();
    let allocator = Allocator {
        name: name.to_owned(),
        used_by: owner.map(str::to_owned),
        next: next.to_owned(),
    };
    (key, allocator)
}

#[tokio::test]
async fn a_renamed_owner_keeps_its_allocator_but_an_unowned_name_does_not_move() {
    let mut a = snapshot(rows(&["id"], &[]), &[]);
    let mut b = snapshot(rows(&["id"], &[]), &[]);
    a.allocators
        .extend([owned("app.t_id_seq", Some("app.u.id"), "3")]);
    b.allocators
        .extend([owned("app.u_id_seq", Some("app.u.id"), "3")]);
    assert!(
        equal(&a, &b).await,
        "the same owning column, a different generated name"
    );
    let (_, allocators) = compare(&a, &b).await;
    assert_eq!(allocators[0].sequence, "app.t_id_seq -> app.u_id_seq");
    // The next value still decides, and an unowned sequence is matched by name.
    b.allocators
        .extend([owned("app.u_id_seq", Some("app.u.id"), "4")]);
    assert!(!equal(&a, &b).await);
    let mut c = snapshot(rows(&["id"], &[]), &[]);
    let mut d = snapshot(rows(&["id"], &[]), &[]);
    c.allocators.extend([owned("app.free", None, "1")]);
    d.allocators.extend([owned("app.other", None, "1")]);
    assert!(!equal(&c, &d).await);
}

#[tokio::test]
async fn comparison_reports_keep_raw_names_and_escape_each_message_once() {
    let mut control = Control::new(Default::default(), std::future::pending());
    for (table, sequence, shown_table, shown_sequence) in [
        ("app.t", "app.s", "app.t", "app.s"),
        ("app.t\\\n", "app.s\\\t", r"app.t\\\n", r"app.s\\\t"),
    ] {
        let owner = format!("{table}.id");
        let make = |value, next| Snapshot {
            tables: BTreeMap::from([(table.into(), rows(&["id"], &[(&[Some(value)], 1)]))]),
            allocators: BTreeMap::from([owned(sequence, Some(&owner), next)]),
        };
        for different in [false, true] {
            let a = make("1", "3");
            let b = make(
                if different { "2" } else { "1" },
                if different { "4" } else { "3" },
            );
            let data = compared(&a, &b, &mut control).await;
            let json = serde_json::to_value(&data).unwrap();
            assert_eq!(json["repin"]["tables"][0]["table"], table);
            assert_eq!(json["repin"]["allocators"][0]["sequence"], sequence);
            assert_eq!(json["repin"]["allocators"][0]["used_by"], owner);
            assert_eq!(json["repin"]["tables"][0]["equal"], !different);
            assert_eq!(json["repin"]["allocators"][0]["equal"], !different);
            let summary = data.summary(true);
            let message = |id| &summary.iter().find(|f| f.id == id).unwrap().message;
            assert_eq!(
                message("test.repin.table"),
                &format!(
                    "`{shown_table}`: {}; original 1 rows, proposed 1 rows; {} difference(s)",
                    if different { "different" } else { "equal" },
                    if different { 2 } else { 0 }
                )
            );
            assert_eq!(
                message("test.repin.allocator"),
                &format!(
                    "`{shown_sequence}` (used by {shown_table}.id): original next 3, proposed next {}: {}",
                    if different { "4" } else { "3" },
                    if different { "different" } else { "equal" }
                )
            );
            if different {
                assert_eq!(
                    data.phases[0].findings[0].message,
                    format!(
                        "`{shown_table}` differs: original 1 rows, proposed 1 rows, 2 differing row(s) shown; \
                     the edited fixture does not leave the original data"
                    )
                );
                assert_eq!(
                    data.phases[1].findings[0].message,
                    format!(
                        "sequence `{shown_sequence}` (used by {shown_table}.id) hands out 3 next in the original and 4 in the \
                     proposed; a later fixture would get different values"
                    )
                );
            } else {
                assert!(data.phases.iter().all(|p| p.findings.is_empty()));
            }
            assert!(
                summary
                    .iter()
                    .chain(data.phases.iter().flat_map(|p| &p.findings))
                    .all(|f| !f.message.chars().any(char::is_control))
            );
            assert_eq!(serde_json::to_value(&data).unwrap(), json);
            assert!(data.phases.iter().all(|p| matches!(
                (p.outcome, different),
                (Status::Failed, true) | (Status::Passed, false)
            )));
            assert_eq!(
                data.finish().result,
                if different {
                    crate::output::Outcome::Findings
                } else {
                    crate::output::Outcome::Ok
                }
            );
        }
    }
}

fn assert_unanswerable_comparison(data: TestData, cause: &str) {
    assert!(matches!(data.phases[0].outcome, Status::Unanswerable));
    assert!(matches!(data.phases[1].outcome, Status::Skipped));
    assert_eq!(data.phases[0].findings.len(), 1);
    assert_eq!(data.phases[0].findings[0].id, "test.repin.unestablished");
    assert!(data.phases[0].findings[0].message.contains(cause));
    let repin = data.repin.as_ref().unwrap();
    assert!(repin.tables.is_empty());
    assert!(repin.allocators.is_empty());
    assert_eq!(repin.manifest.status, "not_attempted");
    let report = data.finish();
    assert_eq!(report.result, crate::output::Outcome::Unanswerable);
    assert_eq!(
        serde_json::to_value(report).unwrap()["result"],
        "unanswerable"
    );
}

#[tokio::test(start_paused = true)]
async fn an_expired_comparison_is_unanswerable_even_for_matching_or_empty_snapshots() {
    for empty in [false, true] {
        let limits = pbps_config::test_manifest::Timeouts {
            run_ms: 1.try_into().unwrap(),
            ..Default::default()
        };
        let mut control = Control::new(limits, std::future::pending());
        tokio::time::advance(std::time::Duration::from_millis(2)).await;
        let mut a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
        if empty {
            a.tables.clear();
            a.allocators.clear();
        }
        let data = compared(&a, &a, &mut control).await;
        assert_unanswerable_comparison(data, "deadline");
    }
}

#[tokio::test]
async fn an_interrupt_between_tables_or_allocators_is_unanswerable() {
    use std::{cell::Cell, future::poll_fn, rc::Rc, task::Poll};

    for allocators_only in [false, true] {
        let polls = Rc::new(Cell::new(0));
        let observed = polls.clone();
        let mut control = Control::new(
            Default::default(),
            poll_fn(move |_| {
                observed.set(observed.get() + 1);
                // The first poll permits one object to be compared. Its yield
                // lets the same Control observe the interrupt on the next poll.
                if observed.get() == 1 {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            }),
        );
        let mut a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[]);
        a.tables.insert("app.u".into(), rows(&["id"], &[]));
        if allocators_only {
            a.tables.clear();
            a.allocators
                .extend([owned("app.s", None, "3"), owned("app.t", None, "4")]);
        }
        let data = compared(&a, &a, &mut control).await;
        assert_unanswerable_comparison(data, "cancelled");
        assert_eq!(polls.get(), 2);
    }
}

#[tokio::test]
async fn an_interrupt_before_an_empty_comparison_is_unanswerable() {
    let mut control = Control::new(Default::default(), std::future::ready(Ok(())));
    let a = Snapshot {
        tables: BTreeMap::new(),
        allocators: BTreeMap::new(),
    };
    let data = compared(&a, &a, &mut control).await;
    assert_unanswerable_comparison(data, "cancelled");
}

#[tokio::test]
async fn raw_names_preserve_equal_different_and_missing_object_sets() {
    let mut a = Snapshot {
        tables: BTreeMap::new(),
        allocators: BTreeMap::new(),
    };
    let mut b = Snapshot {
        tables: BTreeMap::new(),
        allocators: BTreeMap::new(),
    };
    let expected = BTreeMap::from([
        ("app.ordinary".to_owned(), true),
        ("app.t\\\n".to_owned(), false),
        (r"app.t\\\n".to_owned(), false),
        ("app.proposed_only".to_owned(), false),
    ]);
    for name in ["app.ordinary", "app.t\\\n", r"app.t\\\n"] {
        a.tables
            .insert(name.into(), rows(&["id"], &[(&[Some("1")], 1)]));
        insert(&mut a.allocators, owned(name, None, "3").1);
    }
    for (name, value, next) in [
        ("app.ordinary", "1", "3"),
        ("app.t\\\n", "2", "4"),
        ("app.proposed_only", "1", "3"),
    ] {
        b.tables
            .insert(name.into(), rows(&["id"], &[(&[Some(value)], 1)]));
        insert(&mut b.allocators, owned(name, None, next).1);
    }
    let (tables, allocators) = compare(&a, &b).await;
    assert_eq!(
        tables
            .into_iter()
            .map(|t| (t.table, t.equal))
            .collect::<BTreeMap<_, _>>(),
        expected
    );
    assert_eq!(
        allocators
            .into_iter()
            .map(|a| (a.sequence, a.equal))
            .collect::<BTreeMap<_, _>>(),
        expected
    );
    for (old, new, owner) in [
        ("app.old", "app.new", "app.t.id"),
        ("app.old\\\n", "app.new\\\t", "app.t\\\n.id"),
    ] {
        a.allocators.clear();
        b.allocators.clear();
        insert(&mut a.allocators, owned(old, Some(owner), "3").1);
        insert(&mut b.allocators, owned(new, Some(owner), "3").1);
        let (_, allocators) = compare(&a, &b).await;
        assert_eq!(allocators.len(), 1);
        assert!(allocators[0].equal);
        assert_eq!(allocators[0].sequence, format!("{old} -> {new}"));
        assert_eq!(allocators[0].used_by.as_deref(), Some(owner));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs docker and the pinned PostgreSQL scratch image"]
async fn pg18_snapshot_serializes_raw_table_and_sequence_names() -> anyhow::Result<()> {
    use crate::deploy::test_run::boxed_future;
    use crate::plan_test::execution::Control;
    use anyhow::ensure;
    use std::time::Duration;

    let dir = crate::adopt::Stage::new("snapshot2648")?;
    let registry = crate::test_scratch::StateDir::open(dir.path().join("runs"))?;
    let image = std::env::var("PBPS_TEST_PG_SCRATCH_IMAGE")?;
    let mut control = Control::new(Default::default(), std::future::pending());
    let mut data = TestData::for_repin();
    data.phases
        .push(Phase::on(Driver::Postgres, "provision", "raw names"));
    let scratch = boxed_future(|| control.start(&registry, &image, &mut data, 0))
        .await
        .with_context(|| {
            format!(
                "scratch start failed: {}",
                crate::output::human(&data.phases[0].findings)
            )
        })?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let session = control.provision(&scratch, &mut data, 0).await?;
        let mut conn = session.admin().await?;
        conn.execute(
            "CREATE SCHEMA snap; CREATE TABLE snap.\"t\\\n\" (id integer); \
            INSERT INTO snap.\"t\\\n\" VALUES (7); CREATE SEQUENCE snap.\"s\\\t\" START WITH 3",
        )
        .await?;
        let mut schema = Schema::default();
        schema.tables.insert(
            pbps_model::TableName::new("snap", "t\\\n"),
            Default::default(),
        );
        let found = read(&session, &schema).await?;
        let data = compared(&found, &found, &mut control).await;
        Ok::<_, anyhow::Error>(serde_json::to_value(data)?)
    })
    .await;
    control.finish(scratch, &mut data).await;
    ensure!(data.cleanup_confirmed, "owned cleanup failed");
    let json = result??;
    assert_eq!(json["repin"]["tables"][0]["table"], "snap.t\\\n");
    assert_eq!(json["repin"]["tables"][0]["original_rows"], 1);
    assert_eq!(json["repin"]["tables"][0]["equal"], true);
    assert_eq!(json["repin"]["allocators"][0]["sequence"], "snap.s\\\t");
    assert_eq!(json["repin"]["allocators"][0]["original_next"], "3");
    assert_eq!(json["repin"]["allocators"][0]["equal"], true);
    Ok(())
}

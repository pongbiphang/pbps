#![cfg(test)]
//! Frozen 557d4984 function hold, retained only for equivalence checks.
use super::*;
use pbps_model::Module;

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn after_their_functions(
    cs: &mut ChangeSet,
    deps: &ModuleDeps,
    partitions: &Partitions,
) -> Result<usize, String> {
    let functions = function_creates(cs);
    let Some(last) = functions.iter().map(|(_, at)| *at).max() else {
        return Ok(0);
    };
    // Each such column, with the creates after it that its text names.
    let mut waits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, p) in cs.changes[..last].iter().enumerate() {
        // These columns belong to a new table's expansion, whose effects
        // and explicit edges permit its functions to precede the seed rows.
        if pbps_diff::order::generated::is_part(p) {
            continue;
        }
        // Unsplittable shapes keep their generation in CREATE
        // when expression replacement is unavailable (DEC-1503.1), so the
        // whole table waits. So does a new partition's own default (#1578),
        // which has no column of its own to split into an
        // `AlterColumnDefault`; one beside a DEFAULT, whose create moves
        // rows, had its default split off already (`split_moving_defaults`).
        let texts: Vec<&str> = match &p.change {
            Change::AddColumn { column, .. } => column_expression(column).into_iter().collect(),
            Change::CreateTable { table, .. } => table
                .columns
                .values()
                .filter_map(|c| match &c.value_source {
                    ValueSource::Generated(generated) => Some(generated.expression.as_str()),
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_) => None,
                })
                .chain(
                    table
                        .partition_of
                        .iter()
                        .flat_map(|of| of.columns.values())
                        .filter_map(|c| c.default.as_deref()),
                )
                .collect(),
            _ => Vec::new(),
        };
        let named: Vec<usize> = functions
            .iter()
            .filter(|(name, at)| {
                *at > i
                    && texts
                        .iter()
                        .any(|text| pbps_pg::generated::may_call(text, name))
            })
            .map(|(_, at)| *at)
            .collect();
        if !named.is_empty() {
            waits.insert(i, named);
        }
    }
    let Some(&start) = waits.keys().next() else {
        return Ok(0);
    };
    let window: Vec<&Change> = cs.changes[start..=last].iter().map(|p| &p.change).collect();
    let n = window.len();
    // What may be held back: the columns, and whatever may need one of them
    // or something already held back. Only these can change places. The rest
    // keep their order among themselves, so `needs` is asked only of pairs
    // with one side here: a schema of many modules is not lexed pair by pair.
    let mut held: BTreeSet<usize> = waits.keys().map(|at| at - start).collect();
    for i in 0..n {
        if !held.contains(&i)
            && held
                .range(..i)
                .any(|&j| needs(window[i], window[j], deps, partitions))
        {
            held.insert(i);
        }
    }
    // A create that moves rows is never held (`split_moving_defaults`):
    // what must follow the move would be let past it. One still held names a
    // column the parent adds behind a function in an own index or check,
    // which cannot be split off.
    if let Some(Change::CreateTable {
        name,
        beside_default: Some(default),
        ..
    }) = held.iter().map(|&i| window[i]).find(|c| {
        matches!(
            c,
            Change::CreateTable {
                beside_default: Some(_),
                ..
            }
        )
    }) {
        return Err(format!(
            "{name} is made beside the DEFAULT partition {default} and takes its rows, but an \
             index or check of its own names a column this plan adds after a function the \
             column calls, so the move would have to wait for that function and let past it \
             what must follow the move. Add the column in one plan and the partition in the next.",
            name = name.shown(),
            default = default.shown()
        ));
    }
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut previous_free: Option<usize> = None;
    for i in 0..n {
        if held.contains(&i) {
            preds[i].extend((0..i).filter(|&j| needs(window[i], window[j], deps, partitions)));
        } else {
            preds[i].extend(
                held.range(..i)
                    .filter(|&&j| needs(window[i], window[j], deps, partitions)),
            );
            preds[i].extend(previous_free);
            previous_free = Some(i);
        }
    }
    // A column follows the creates its text names. One that may also need the
    // column keeps both edges: that is the cycle refused below.
    for (at, named) in &waits {
        preds[at - start].extend(named.iter().map(|create| create - start));
    }
    // Kahn's algorithm, always taking the earliest ready change, so that what
    // nothing holds back keeps its place.
    let mut done = vec![false; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(i) = (0..n).find(|&i| !done[i] && preds[i].iter().all(|&j| done[j])) {
        done[i] = true;
        order.push(i);
    }
    if order.len() < n {
        return Err(cycle(cs, start, &waits, &done, &window, deps, partitions));
    }
    let mut taken: Vec<Option<PlannedChange>> = cs.changes.drain(start..=last).map(Some).collect();
    let placed: Vec<PlannedChange> = order
        .iter()
        .map(|&i| taken[i].take().expect("each change is ordered once"))
        .collect();
    cs.changes.splice(start..start, placed);
    Ok(waits.len())
}

pub(super) fn free_chain(schedule: &Schedule, start: usize, last: usize, held: &BTreeSet<usize>) {
    let free: Vec<_> = (start..=last)
        .filter(|at| !held.contains(&(at - start)))
        .collect();
    for pair in free.windows(2) {
        assert!(
            schedule
                .constraints()
                .iter()
                .any(|edge| edge.before == pair[0] && edge.after == pair[1]),
            "free-step order must be an edge, not a Keep guard or an implicit tie"
        );
    }
    assert!(
        schedule
            .constraints()
            .iter()
            .all(|edge| edge.before >= start
                && edge.before <= last
                && edge.after >= start
                && edge.after <= last),
        "the function window cannot constrain its untouched prefix or suffix"
    );
}

fn routine(name: &str, definition: &str, kind: ModuleKind) -> Change {
    Change::CreateModule {
        id: name.parse().unwrap(),
        module: Box::new(Module {
            kind,
            description: None,
            definition: definition.into(),
        }),
    }
}

#[test]
fn function_windows_match_old_bytes_for_32_uid_seeds_and_named_cycles() {
    use pbps_model::{AddedRole, Column, Provenance, Uid, UidKind};
    for salt in 0..32 {
        for case in ["called", "comment", "cycle"] {
            let mut column = Column::new("integer".parse().unwrap());
            column.value_source = ValueSource::Default(
                if case == "comment" {
                    "1 /* app.f() */"
                } else {
                    "app.f()"
                }
                .into(),
            );
            let role = |name: &str| Change::CreateRole {
                uid: Uid::derived(UidKind::Role, name, salt),
                name: name.into(),
            };
            let mut cs = ChangeSet {
                changes: vec![
                    role("prefix"),
                    Change::AddColumn {
                        uid: Uid::derived(UidKind::Column, "app.t.g", salt),
                        table: TableName::new("app", "t"),
                        name: "g".into(),
                        column: Box::new(column),
                    },
                    routine("app.free_a", "SELECT 1 AS n", ModuleKind::View),
                    routine("app.reader", "SELECT * FROM app.t", ModuleKind::View),
                    routine("app.declared", "SELECT 1 AS n", ModuleKind::View),
                    routine(
                        "app.literal",
                        "SELECT 'app.reader'::regclass AS n",
                        ModuleKind::View,
                    ),
                    routine(
                        "app.f()",
                        if case == "cycle" {
                            "() RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT g FROM app.t $$"
                        } else {
                            "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7"
                        },
                        ModuleKind::Function,
                    ),
                    routine("app.free_b", "SELECT 2 AS n", ModuleKind::View),
                    routine(
                        "app.last()",
                        "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 9",
                        ModuleKind::Function,
                    ),
                    role("suffix"),
                ]
                .into_iter()
                .map(PlannedChange::new)
                .collect(),
            };
            cs.changes[0].provenance = Some(Provenance {
                rule: "original",
                role: AddedRole::Restoration,
                source: "original payload".into(),
                reason: "must travel intact".into(),
            });
            let before = cs.clone();
            let deps: ModuleDeps = [(
                "app.declared".parse().unwrap(),
                ["app.reader".parse().unwrap()].into(),
            )]
            .into();
            let result =
                super::test_adapters::after_their_functions(&mut cs, &deps, &Partitions::default());
            if case == "cycle" {
                let why = result.unwrap_err();
                assert!(
                    why.contains("app.t.g") && why.contains("app.f()") && why.contains("two plans"),
                    "{why}"
                );
                assert_eq!(cs, before);
            } else if case == "comment" {
                assert_eq!(result, Ok(0));
                assert_eq!(cs, before);
            } else {
                assert_eq!(result, Ok(1));
                assert_eq!(cs.changes.first(), before.changes.first());
                assert_eq!(cs.changes.last(), before.changes.last());
                assert_eq!(cs.changes[0].provenance, before.changes[0].provenance);
                let at = |name: &str| {
                    cs.changes
                        .iter()
                        .position(|p| {
                            p.change
                                .module_id()
                                .is_some_and(|id| id.raw().to_string() == name)
                        })
                        .unwrap()
                };
                let column = cs
                    .changes
                    .iter()
                    .position(|p| matches!(p.change, Change::AddColumn { .. }))
                    .unwrap();
                assert!(at("app.f()") < column && column < at("app.reader"));
                assert!(
                    at("app.reader") < at("app.declared") && at("app.reader") < at("app.literal")
                );
                assert!(at("app.free_a") < at("app.free_b") && at("app.free_b") < at("app.last()"));
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn callable_columns_precede_their_readers_and_real_cycles_are_refused_on_both_servers() {
    use pbps_db::{Conn, Driver};
    use pbps_model::{Column, Uid, UidKind};
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).expect("both PostgreSQL endpoints are required");
        let database = format!("pbps_s7_column_{:016x}", rand::random::<u64>());
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
            .await
            .unwrap();
        let result: anyhow::Result<()> = async {
            let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}")).await?;
            conn.execute("CREATE SCHEMA app; CREATE TABLE app.t (id integer); INSERT INTO app.t VALUES (1)").await?;
            let column = |name: &str, expression: &str| {
                let mut column = Column::new("integer".parse().unwrap());
                column.value_source = ValueSource::Default(expression.into());
                Change::AddColumn { uid: Uid::derived(UidKind::Column, name, 0), table: TableName::new("app", "t"), name: name.into(), column: Box::new(column) }
            };
            let mut cs = ChangeSet { changes: vec![
                column("g", "app.f()"),
                routine("app.reader", "SELECT * FROM app.t", ModuleKind::View),
                routine("app.consumer", "SELECT g FROM app.reader", ModuleKind::View),
                routine("app.free", "SELECT 9 AS n", ModuleKind::View),
                routine("app.f()", "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7", ModuleKind::Function),
            ].into_iter().map(PlannedChange::new).collect() };
            assert_eq!(super::test_adapters::after_their_functions(&mut cs, &ModuleDeps::new(), &Partitions::default()), Ok(1));
            let pg = pbps_pg::Postgres::new();
            for p in &cs.changes {
                for sql in pg.emit(&p.change, p.strategy)? { conn.execute(&sql.sql).await?; }
            }
            let rows = conn.query("SELECT g FROM app.consumer").await?;
            assert_eq!(rows[0].try_get::<i32>("g")?, Some(7));
            let mut cycle = ChangeSet { changes: vec![
                column("h", "app.cycle()"),
                routine("app.cycle()", "() RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT h FROM app.t $$", ModuleKind::Function),
            ].into_iter().map(PlannedChange::new).collect() };
            let why = super::test_adapters::after_their_functions(&mut cycle, &ModuleDeps::new(), &Partitions::default()).unwrap_err();
            assert!(why.contains("app.t.h") && why.contains("app.cycle()"), "{why}");
            // Measure both directions: neither CREATE FUNCTION before its
            // column nor ADD COLUMN before its function can execute.
            conn.execute("BEGIN").await?;
            assert!(conn.execute("CREATE FUNCTION app.cycle() RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT h FROM app.t $$").await.is_err());
            conn.execute("ROLLBACK").await?;
            conn.execute("BEGIN").await?;
            assert!(conn.execute("ALTER TABLE app.t ADD COLUMN h integer DEFAULT app.cycle()").await.is_err());
            conn.execute("ROLLBACK").await?;
            Ok(())
        }.await;
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        result.unwrap();
    }
}

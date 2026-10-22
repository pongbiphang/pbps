use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_dialect::PlanDialect;
use pbps_model::{
    Change, ChangeSet, ColumnRef, ForeignKey, IdsFile, PlannedChange, TableName, Uid, UidKind,
};

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions"]
async fn row_security_filtered_counts_are_unchecked_and_column_refusals_are_exact() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let database = format!("pbps_rls1172_{stamp}");
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {database}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={database}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(check(connection, format!("owner_{stamp}"))).await
            })
            .await;
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        result.expect("policy probe/dependent fixture failed after cleanup");
    }
}

async fn check(connection: String, owner: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(&format!("BEGIN; CREATE ROLE {owner} NOBYPASSRLS;
        CREATE SCHEMA app AUTHORIZATION {owner};
        CREATE TABLE app.t (id integer PRIMARY KEY, n integer, d integer, wide integer, chk integer, refid integer, extra integer);
        INSERT INTO app.t VALUES (1,NULL,9,70000,-1,999,0),(2,1,9,1,1,1,0);
        CREATE TABLE app.parent (id integer PRIMARY KEY); INSERT INTO app.parent VALUES (1);
        ALTER TABLE app.t OWNER TO {owner}; ALTER TABLE app.parent OWNER TO {owner};
        CREATE SEQUENCE app.probe_calls;
        CREATE FUNCTION app.probe_effect() RETURNS boolean LANGUAGE sql IMMUTABLE AS $effect$ SELECT nextval('app.probe_calls') > 0 $effect$;
        GRANT USAGE ON SEQUENCE app.probe_calls TO {owner};
        CREATE POLICY probe_function ON app.t AS RESTRICTIVE USING (app.probe_effect());
        CREATE POLICY tenant_rows ON app.t USING (id=2);
        CREATE POLICY parent_rows ON app.parent USING (false);
        ALTER TABLE app.t ENABLE ROW LEVEL SECURITY; ALTER TABLE app.t FORCE ROW LEVEL SECURITY;
        ALTER TABLE app.parent FORCE ROW LEVEL SECURITY;
        SET LOCAL ROLE {owner};")).await.unwrap();
    let t = TableName::new("app", "t");
    let parent = TableName::new("app", "parent");
    let column_uid = |name: &str| Uid::derived(UidKind::Column, name, 0);
    let mut ids = IdsFile::default();
    ids.tables
        .insert(Uid::derived(UidKind::Table, "t", 0), t.clone());
    ids.tables
        .insert(Uid::derived(UidKind::Table, "parent", 0), parent.clone());
    for name in ["id", "n", "d", "wide", "chk", "refid", "extra"] {
        ids.columns.insert(column_uid(name), t.column(name));
    }
    ids.columns
        .insert(column_uid("parent.id"), parent.column("id"));
    let retype = |name: &str| Change::AlterColumnType {
        uid: column_uid(name),
        column: t.column(name),
        from: "integer".parse().unwrap(),
        to: "smallint".parse().unwrap(),
        through_external_partitions: false,
        from_nullable: true,
        to_nullable: true,
        from_collation: None,
        to_collation: None,
    };
    let changes = ChangeSet {
        changes: vec![
            Change::AlterColumnNullability {
                uid: column_uid("n"),
                column: t.column("n"),
                to_nullable: false,
                ty: "integer".parse().unwrap(),
                collation: None,
                through_external_partitions: false,
            },
            Change::AddUnique {
                table: t.clone(),
                name: "unique_d".into(),
                constraint: pbps_model::UniqueConstraint {
                    columns: vec!["d".into()],
                    storage_parameters: Default::default(),
                },
                clustered: false,
            },
            Change::AddCheck {
                table: t.clone(),
                name: "positive_chk".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "chk > 0".into(),
                },
            },
            retype("wide"),
            Change::AddForeignKey {
                table: t.clone(),
                name: "parent_fk".into(),
                constraint: Box::new(ForeignKey {
                    columns: vec!["refid".into()],
                    references_table: parent.clone(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                }),
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
    let probes = pbps_pg::Postgres::new().preflight_with(&names, &Default::default());
    let counts: Vec<_> = probes
        .probes
        .iter()
        .filter(|p| !p.description.contains("incompatible collations"))
        .collect();
    assert_eq!(counts.len(), 5, "{probes:#?}");
    for probe in &counts {
        let rows = conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", probe.sql));
        assert_eq!(
            rows[0].try_get_at::<i32>(0).unwrap(),
            None,
            "{}",
            probe.description
        );
    }
    // Sequence counters are catalog questions, even on an actively filtered table.
    conn.execute("CREATE TABLE app.numbered (id integer GENERATED BY DEFAULT AS IDENTITY); ALTER TABLE app.numbered ENABLE ROW LEVEL SECURITY; ALTER TABLE app.numbered FORCE ROW LEVEL SECURITY").await.unwrap();
    let numbered = TableName::new("app", "numbered");
    let number_uid = column_uid("numbered.id");
    let mut number_ids = IdsFile::default();
    number_ids.tables.insert(
        Uid::derived(UidKind::Table, "numbered", 0),
        numbered.clone(),
    );
    number_ids
        .columns
        .insert(number_uid.clone(), numbered.column("id"));
    let was = pbps_model::Identity {
        seed: 1,
        increment: 1,
        options: Default::default(),
    };
    let mut now = was;
    now.options.max = Some(100);
    let number_changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterColumnNumbering {
            uid: number_uid,
            column: numbered.column("id"),
            ty: "integer".parse().unwrap(),
            from: pbps_model::ValueSource::Identity(was),
            to: pbps_model::ValueSource::Identity(now),
        })],
    };
    let number_names =
        pbps_model::plan_names::PlanNames::of(&number_changes.changes, &number_ids).unwrap();
    let number_probes = pbps_pg::Postgres::new().preflight_with(&number_names, &Default::default());
    assert_eq!(number_probes.probes.len(), 1);
    assert_eq!(
        conn.query(&number_probes.probes[0].sql).await.unwrap()[0]
            .try_get_at::<i32>(0)
            .unwrap(),
        Some(0)
    );
    conn.execute("RESET ROLE").await.unwrap();
    assert_eq!(
        conn.query("SELECT is_called FROM app.probe_calls")
            .await
            .unwrap()[0]
            .try_get_at::<bool>(0)
            .unwrap(),
        Some(false),
        "a skipped probe must not execute a policy routine during planning"
    );
    conn.execute(&format!("SET LOCAL ROLE {owner}"))
        .await
        .unwrap();
    let metadata: Vec<_> = probes
        .probes
        .iter()
        .filter(|p| p.description.contains("incompatible collations"))
        .collect();
    assert_eq!(metadata.len(), 1);
    assert_eq!(
        conn.query(&metadata[0].sql).await.unwrap()[0]
            .try_get_at::<i32>(0)
            .unwrap(),
        Some(0)
    );
    conn.execute("SAVEPOINT engine_constraint").await.unwrap();
    let error = conn
        .execute("ALTER TABLE app.t ALTER COLUMN n SET NOT NULL")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("contains null values"),
        "{error}"
    );
    conn.execute("ROLLBACK TO SAVEPOINT engine_constraint; RESET ROLE")
        .await
        .unwrap();
    for probe in &counts {
        let rows = conn.query(&probe.sql).await.unwrap();
        assert!(
            rows[0].try_get_at::<i32>(0).unwrap().unwrap() > 0,
            "{}",
            probe.description
        );
    }
    let fk = counts
        .iter()
        .find(|p| p.description.contains("parent_fk"))
        .unwrap();
    // Parent-only filtering must also make the orphan count unchecked.
    conn.execute(&format!("ALTER TABLE app.t DISABLE ROW LEVEL SECURITY; ALTER TABLE app.parent ENABLE ROW LEVEL SECURITY; SET LOCAL ROLE {owner}")).await.unwrap();
    assert_eq!(
        conn.query(&fk.sql).await.unwrap()[0]
            .try_get_at::<i32>(0)
            .unwrap(),
        None
    );
    conn.execute("RESET ROLE").await.unwrap();

    // A relation-only dependency is not a dependency on every attribute.
    conn.execute("CREATE POLICY row_presence ON app.parent USING (EXISTS(SELECT FROM app.t))")
        .await
        .unwrap();
    let policy = pbps_pg::modules::policies_on_column(&mut conn, &t.column("id"))
        .await
        .unwrap();
    assert_eq!(policy.len(), 1);
    assert_eq!(policy[0].name, "tenant_rows");
    assert!(
        pbps_pg::modules::policies_on_column(&mut conn, &t.column("extra"))
            .await
            .unwrap()
            .is_empty()
    );
    for name in ["id", "extra"] {
        let drop = ChangeSet {
            changes: vec![PlannedChange::new(Change::DropColumn {
                uid: column_uid(name),
                column: ColumnRef::new(t.clone(), name),
            })],
        };
        let mut final_ids = ids.clone();
        final_ids.columns.remove(&column_uid(name));
        let result = crate::engine::check_module_dependents(
            &mut conn,
            &drop,
            &Default::default(),
            &final_ids,
            &ids,
        )
        .await;
        if name == "extra" {
            result.unwrap();
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("tenant_rows") && error.contains("arrives with #1173"),
                "{error}"
            );
        }
    }
    // The lookup follows recorded identities back through both renames.
    let renamed = TableName::new("app", "renamed");
    let table_uid = ids
        .tables
        .iter()
        .find(|(_, name)| *name == &t)
        .unwrap()
        .0
        .clone();
    let mut renamed_ids = ids.clone();
    renamed_ids
        .tables
        .insert(table_uid.clone(), renamed.clone());
    for column in renamed_ids
        .columns
        .values_mut()
        .filter(|column| column.table == t)
    {
        column.table = renamed.clone();
    }
    renamed_ids
        .columns
        .insert(column_uid("id"), renamed.column("renamed_id"));
    let mut altered = retype("id");
    if let Change::AlterColumnType { column, .. } = &mut altered {
        *column = renamed.column("renamed_id");
    }
    let renamed_change = ChangeSet {
        changes: vec![
            Change::RenameTable {
                uid: table_uid,
                from: t.clone(),
                to: renamed.clone(),
                defaults: vec![],
            },
            Change::RenameColumn {
                uid: column_uid("id"),
                table: renamed.clone(),
                from: "id".into(),
                to: "renamed_id".into(),
                table_was: Some(t.clone()),
            },
            altered,
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let error = crate::engine::check_module_dependents(
        &mut conn,
        &renamed_change,
        &Default::default(),
        &renamed_ids,
        &ids,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("tenant_rows") && error.contains("arrives with #1173"),
        "{error}"
    );
    policy_sequence_bindings(&mut conn).await;
    policy_owner_drop_releases_column(&mut conn).await;
    policy_owner_teardowns(&mut conn).await;
    conn.execute("ALTER TABLE app.t DROP COLUMN extra; ROLLBACK")
        .await
        .unwrap();
}

async fn policy_sequence_bindings(conn: &mut Conn) {
    conn.execute(
        "CREATE SEQUENCE app.unowned_policy_sequence;
         CREATE TABLE app.sequence_input (
             id integer, identity_target integer GENERATED ALWAYS AS IDENTITY,
             serial_target serial, unowned integer DEFAULT nextval('app.unowned_policy_sequence'));
         CREATE TABLE app.sequence_owner(id integer);
         CREATE POLICY identity_gate ON app.sequence_owner
             USING (nextval('app.sequence_input_identity_target_seq')>0);
         CREATE POLICY serial_gate ON app.sequence_owner
             WITH CHECK (nextval('app.sequence_input_serial_target_seq')>0);
         CREATE POLICY unowned_gate ON app.sequence_owner
             USING (nextval('app.unowned_policy_sequence')>0);",
    )
    .await
    .unwrap();
    let input = TableName::new("app", "sequence_input");
    let owner = TableName::new("app", "sequence_owner");
    for (name, policy) in [
        ("identity_target", "identity_gate"),
        ("serial_target", "serial_gate"),
    ] {
        let column = input.column(name);
        assert!(
            pbps_pg::modules::policies_on_column(conn, &column)
                .await
                .unwrap()
                .is_empty()
        );
        let found = pbps_pg::modules::policies_on_column_removal(conn, &column)
            .await
            .unwrap();
        assert_eq!(
            found,
            vec![pbps_pg::modules::PolicyDependent {
                table: owner.clone(),
                name: policy.into(),
            }]
        );
        let table_uid = Uid::derived(UidKind::Table, "sequence_input", 0);
        let column_uid = Uid::derived(UidKind::Column, name, 0);
        let mut ids = IdsFile::default();
        ids.tables.insert(table_uid, input.clone());
        ids.columns.insert(column_uid.clone(), column.clone());
        let mut final_ids = ids.clone();
        final_ids.columns.remove(&column_uid);
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::DropColumn {
                uid: column_uid,
                column,
            })],
        };
        let error = crate::engine::check_module_dependents(
            conn,
            &changes,
            &Default::default(),
            &final_ids,
            &ids,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(policy)
                && error.contains("sequence_owner")
                && error.contains("arrives with #1173"),
            "{error}"
        );
    }
    // Merely defaulting from another sequence conveys no ownership, and an
    // unrelated column is not bound just because its table owns a sequence.
    for name in ["id", "unowned"] {
        assert!(
            pbps_pg::modules::policies_on_column_removal(conn, &input.column(name))
                .await
                .unwrap()
                .is_empty()
        );
    }
    let found = pbps_pg::modules::policies_on_table(conn, &input)
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["identity_gate", "serial_gate"]
    );
    // Both engines preserve these sequence bindings through retype/rename;
    // those reads must not acquire removal-only policy release requirements.
    conn.execute(
        "ALTER TABLE app.sequence_input ALTER COLUMN identity_target TYPE bigint;
         ALTER TABLE app.sequence_input ALTER COLUMN serial_target TYPE bigint;
         ALTER TABLE app.sequence_input RENAME COLUMN identity_target TO renamed_target;",
    )
    .await
    .unwrap();
    for name in ["renamed_target", "serial_target"] {
        assert!(
            pbps_pg::modules::policies_on_column(conn, &input.column(name))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            pbps_pg::modules::policies_on_column_removal(conn, &input.column(name))
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

async fn policy_owner_drop_releases_column(conn: &mut Conn) {
    use pbps_model::{Column, Schema, Table};
    let input = TableName::new("app", "policy_input");
    let owner = TableName::new("app", "policy_owner");
    conn.execute(
        "CREATE FUNCTION app.policy_value(integer) RETURNS integer LANGUAGE sql IMMUTABLE \
        AS $value$ SELECT $1 * 2 $value$; \
        CREATE TABLE app.policy_input (id integer, target integer GENERATED ALWAYS AS (app.policy_value(id)) STORED); \
        CREATE TABLE app.policy_owner (id integer); \
        CREATE POLICY cross_table ON app.policy_owner USING \
        (EXISTS(SELECT FROM app.policy_input WHERE target > 0)); \
        SAVEPOINT policy_owner_order",
    )
    .await
    .unwrap();
    let error = conn
        .execute("ALTER TABLE app.policy_input DROP COLUMN target")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("other objects depend on it"),
        "{error}"
    );
    conn.execute(
        "ROLLBACK TO SAVEPOINT policy_owner_order; \
        DROP TABLE app.policy_owner; ALTER TABLE app.policy_input DROP COLUMN target; \
        ROLLBACK TO SAVEPOINT policy_owner_order",
    )
    .await
    .unwrap();
    let input_uid = Uid::derived(UidKind::Table, "policy_input", 0);
    let owner_uid = Uid::derived(UidKind::Table, "policy_owner", 0);
    let target_uid = Uid::derived(UidKind::Column, "policy_input.target", 0);
    let mut ids = IdsFile::default();
    ids.tables.insert(input_uid.clone(), input.clone());
    ids.tables.insert(owner_uid.clone(), owner.clone());
    ids.columns
        .insert(target_uid.clone(), input.column("target"));
    for (table, seed) in [(&input, "input.id"), (&owner, "owner.id")] {
        ids.columns
            .insert(Uid::derived(UidKind::Column, seed, 0), table.column("id"));
    }
    let column = || Column::new("integer".parse().unwrap());
    let mut before = Schema::default();
    let function = "app.policy_value(integer)".parse().unwrap();
    before.modules.insert(
        function,
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition:
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $value$ SELECT $1 * 2 $value$"
                    .into(),
        },
    );
    let mut generated = column();
    generated.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: "app.policy_value(id)".into(),
        stored: true,
    });
    before.tables.insert(
        input.clone(),
        Table {
            columns: [("id".into(), column()), ("target".into(), generated)].into(),
            ..Default::default()
        },
    );
    before.tables.insert(
        owner.clone(),
        Table {
            columns: [("id".into(), column())].into(),
            row_policies: [(
                "cross_table".into(),
                pbps_model::RowPolicy {
                    using: Some("EXISTS(SELECT FROM app.policy_input WHERE target > 0)".into()),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        },
    );
    let mut after = before.clone();
    after.modules.clear();
    after.tables.remove(&owner);
    after
        .tables
        .get_mut(&input)
        .unwrap()
        .columns
        .shift_remove("target");
    let mut final_ids = ids.clone();
    final_ids.tables.remove(&owner_uid);
    final_ids
        .columns
        .retain(|uid, column| uid != &target_uid && column.table != owner);
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropColumn {
                uid: target_uid,
                column: input.column("target"),
            }),
            PlannedChange::new(Change::DropModule {
                id: "app.policy_value(integer)".parse().unwrap(),
                kind: pbps_model::ModuleKind::Function,
            }),
            PlannedChange::new(Change::DropTable {
                uid: owner_uid,
                name: owner.clone(),
                detach_from: None,
            }),
        ],
    };
    let wrong = crate::engine::check_module_dependents(conn, &changes, &before, &final_ids, &ids)
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("cross_table"));
    let changes = pbps_diff::order::propose(changes);
    let (changes, _) = crate::engine::account_for_module_dependents(
        conn,
        changes,
        &after,
        crate::dependents::Sources {
            before: pbps_diff::Side {
                schema: &before,
                ids: &ids,
            },
            after: &final_ids,
        },
        &Default::default(),
        None,
        &pbps_pg::Postgres::new(),
        &|_| panic!("the policy-owner drop needs no module rediff"),
    )
    .await
    .unwrap();
    assert!(matches!(&changes.changes[0].change, Change::DropTable { name, .. } if name == &owner));
    assert!(matches!(
        &changes.changes[1].change,
        Change::DropColumn { .. }
    ));
    assert!(matches!(
        &changes.changes[2].change,
        Change::DropModule { .. }
    ));
    crate::engine::check_module_dependents(conn, &changes, &before, &final_ids, &ids)
        .await
        .unwrap();
    for step in &changes.changes {
        for statement in pbps_pg::Postgres::new()
            .emit(
                &pbps_diff::order::singleton(
                    step.clone(),
                    pbps_model::PlanMode::Transactional,
                    &pbps_pg::Postgres::new(),
                ),
                0,
            )
            .unwrap()
        {
            conn.execute(&statement.sql).await.unwrap();
        }
    }
    assert!(
        conn.query("SELECT target FROM app.policy_input")
            .await
            .is_err()
    );
    conn.execute("ROLLBACK TO SAVEPOINT policy_owner_order")
        .await
        .unwrap();
}

async fn policy_owner_teardowns(conn: &mut Conn) {
    use pbps_model::{Column, Schema, Table};
    let mut failures = Vec::new();
    for variant in 0..3 {
        let routine = variant != 2;
        let replace = variant == 1;
        conn.execute("SAVEPOINT policy_teardown").await.unwrap();
        let names: Vec<_> = if routine {
            vec!["routine_owner"]
        } else {
            vec!["a", "m", "z"]
        }
        .into_iter()
        .map(|name| TableName::new("app", name))
        .collect();
        let mut before = Schema::default();
        let mut ids = IdsFile::default();
        for table in &names {
            conn.execute(&format!("CREATE TABLE {} (id integer)", table.shown()))
                .await
                .unwrap();
            ids.tables
                .insert(Uid::derived(UidKind::Table, &table.name, 0), table.clone());
            ids.columns.insert(
                Uid::derived(UidKind::Column, &table.name, 0),
                table.column("id"),
            );
            before.tables.insert(
                table.clone(),
                Table {
                    columns: [("id".into(), Column::new("integer".parse().unwrap()))].into(),
                    ..Default::default()
                },
            );
        }
        let mut changes = Vec::new();
        if routine {
            conn.execute("CREATE FUNCTION app.policy_gate() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true'; CREATE POLICY calls_gate ON app.routine_owner USING (app.policy_gate())").await.unwrap();
            before.modules.insert(
                "app.policy_gate()".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true'".into(),
                },
            );
            before
                .tables
                .get_mut(&names[0])
                .unwrap()
                .row_policies
                .insert(
                    "calls_gate".into(),
                    pbps_model::RowPolicy {
                        using: Some("app.policy_gate()".into()),
                        ..Default::default()
                    },
                );
            changes.push(Change::DropModule {
                id: "app.policy_gate()".parse().unwrap(),
                kind: pbps_model::ModuleKind::Function,
            });
        } else {
            // Relation-only edges really block DROP TABLE, while a column
            // drop must not mistake them for references to every attribute.
            conn.execute("CREATE POLICY reads_a ON app.z USING (EXISTS (SELECT FROM app.a)); CREATE POLICY reads_z ON app.m USING (EXISTS (SELECT FROM app.z))").await.unwrap();
            for (owner, input, policy) in [
                (&names[2], &names[0], "reads_a"),
                (&names[1], &names[2], "reads_z"),
            ] {
                before.tables.get_mut(owner).unwrap().row_policies.insert(
                    policy.into(),
                    pbps_model::RowPolicy {
                        using: Some(format!("EXISTS (SELECT FROM {})", input.shown()).into()),
                        ..Default::default()
                    },
                );
            }
        }
        if replace {
            let id = "app.policy_gate()".parse().unwrap();
            changes.push(Change::CreateModule {
                id,
                module: Box::new(before.modules[&"app.policy_gate()".parse().unwrap()].clone()),
            });
        }
        for table in &names {
            changes.push(Change::DropTable {
                uid: Uid::derived(UidKind::Table, &table.name, 0),
                name: table.clone(),
                detach_from: None,
            });
        }
        let plan = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        let mut after = Schema::default();
        if replace {
            after.modules = before.modules.clone();
        }
        let final_ids = IdsFile::default();
        let wrong = crate::engine::check_module_dependents(conn, &plan, &before, &final_ids, &ids)
            .await
            .unwrap_err();
        assert!(
            wrong
                .to_string()
                .contains(if routine { "calls_gate" } else { "reads_a" }),
            "{wrong}"
        );
        let plan = pbps_diff::order::propose(plan);
        let result: anyhow::Result<()> = async {
            let (plan, _) = crate::engine::account_for_module_dependents(
                conn,
                plan,
                &after,
                crate::dependents::Sources {
                    before: pbps_diff::Side {
                        schema: &before,
                        ids: &ids,
                    },
                    after: &final_ids,
                },
                &Default::default(),
                None,
                &pbps_pg::Postgres::new(),
                &|_| panic!("no retained module needs a rediff"),
            )
            .await?;
            crate::engine::check_module_dependents(conn, &plan, &before, &final_ids, &ids).await?;
            crate::engine::check_drop_blockers(conn, &plan, &final_ids, &before, &ids).await?;
            let order: Vec<_> = plan
                .changes
                .iter()
                .filter_map(|p| {
                    if let Change::DropTable { name, .. } = &p.change {
                        Some(name.name.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                order,
                if routine {
                    vec!["routine_owner"]
                } else {
                    vec!["m", "z", "a"]
                }
            );
            assert!(matches!(&plan.changes[0].change, Change::DropTable { .. }));
            for step in &plan.changes {
                for statement in pbps_pg::Postgres::new().emit(
                    &pbps_diff::order::singleton(
                        step.clone(),
                        pbps_model::PlanMode::Transactional,
                        &pbps_pg::Postgres::new(),
                    ),
                    0,
                )? {
                    conn.execute(&statement.sql).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            failures.push(format!("routine={routine}, replace={replace}: {error:#}"));
        }
        conn.execute("ROLLBACK TO SAVEPOINT policy_teardown")
            .await
            .unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

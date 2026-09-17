//! What a deployment account must be able to **read**, measured rather than
//! reasoned about (issues #510, #514, #515, #516, #517).
//!
//! Every case here is one `SELECT` demand the readiness report either makes or
//! refuses to make, and each is pinned by running the statement the demand
//! exists for under the same login. They are different securables and
//! different lists, and they were four separate omissions: a foreign-key
//! parent the project does not declare but shares a schema with, a table the
//! delete count reads because the catalog says it points here, a column the
//! same plan is about to add, and the destination of a move between schemas.
//! The fifth case makes no demand: it pins that a column `DENY` is already
//! caught, so the question's ordering is not changed later on the strength of
//! a supposition that it is not (#514).

use super::*;

/// A foreign key whose target the declarations do not hold is authorized and
/// probed on that target, and sharing a schema with the declarations does not
/// change that (issue #510).
///
/// `REFERENCES ON SCHEMA::app` covers the DDL half, which is why this was
/// invisible: the account really can add the key. The probe that precedes it
/// reads the parent, and once the managed `SELECT` question narrowed from the
/// schema to the declared and recorded tables (#194), an undeclared parent in
/// a managed schema was covered by neither list.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn an_undeclared_foreign_key_parent_in_a_managed_schema_is_asked_about() {
    let mut db = TestDb::create("doctorfk510").await;
    let login = format!("pbps_fk510_{}", std::process::id());
    let as_login = least_privilege_login(&mut db, &login, "pbpsLeastPrivilege!1").await;
    // `app.parent` is in the managed schema and is not declared: somebody
    // else's table, which this project points a key at.
    db.conn
        .execute(
            "CREATE TABLE app.parent (code varchar(20) NOT NULL PRIMARY KEY); \
             INSERT INTO app.parent VALUES ('a');",
        )
        .await
        .expect("create the undeclared parent");
    // The base grants `SELECT` on the whole schema, which is exactly what a
    // login that passed this check wrongly did *not* have: narrow it to the
    // shape #194 leaves, an account that can read the tables it manages and
    // nothing else in the schema.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE SELECT ON SCHEMA::app FROM [{login}];",
            db.name
        ))
        .await
        .expect("take the schema-wide read away");

    let target: pbps_model::ObjectName = "app.parent".parse().unwrap();
    let referenced: pbps_mssql::doctor::ReferencedColumns =
        [(target.clone(), ["code".to_owned()].into_iter().collect())]
            .into_iter()
            .collect();
    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise, from the engine: the DDL half is held and the read is not.
    assert!(
        holds_at_object_scope(&mut lp, "app.parent", "REFERENCES").await,
        "the premise: the schema grant authorizes the key itself"
    );
    assert!(
        lp.query("SELECT COUNT(*) FROM app.parent;").await.is_err(),
        "the premise: the probe's own read is refused"
    );

    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &[],
        &["app".to_owned()],
        &referenced,
        &pbps_mssql::doctor::GrantTargets::default(),
        &Default::default(),
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        named_gaps(&held),
        ["SELECT on OBJECT::[app].[parent]"],
        "the read half is the gap, and it is reported where the GRANT goes: {held:?}"
    );

    // The control, and the remedy the report prints: granting it there closes
    // the gap and the probe's read runs.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT SELECT ON OBJECT::app.parent TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the read on the parent");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.query("SELECT COUNT(*) FROM app.parent;")
        .await
        .expect("the granted read runs");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &[],
        &["app".to_owned()],
        &referenced,
        &pbps_mssql::doctor::GrantTargets::default(),
        &Default::default(),
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert!(named_gaps(&held).is_empty(), "{held:?}");

    drop(lp);
    drop_login(&login).await;
    db.drop().await;
}

/// A `DENY` on one column is not covered by a grant on the object or on the
/// schema, and the readiness question sees it (issue #514).
///
/// The review this answers supposed the opposite: that `HAS_PERMS_BY_NAME`
/// keeps answering 1 at object scope under a column `DENY`, so the object
/// answer would be taken and the denial missed. It does not, on either grant
/// shape — and this pins that, so the ordering of the question is not changed
/// later on the strength of the supposition.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_column_deny_is_a_gap_under_an_object_grant_and_under_a_schema_grant() {
    for (tag, grant) in [
        ("obj", "GRANT SELECT ON OBJECT::app.t TO"),
        ("sch", "GRANT SELECT ON SCHEMA::app TO"),
    ] {
        let mut db = TestDb::create(&format!("doctordeny514{tag}")).await;
        let login = format!("pbps_deny514{tag}_{}", std::process::id());
        let as_login = least_privilege_login(&mut db, &login, "pbpsLeastPrivilege!1").await;
        db.conn
            .execute(
                "CREATE TABLE app.t (code varchar(20) NOT NULL PRIMARY KEY, \
                 label nvarchar(50) NULL); \
                 INSERT INTO app.t VALUES ('a', N'A');",
            )
            .await
            .expect("create the managed table");
        db.conn
            .execute(&format!(
                "USE [{0}]; {grant} [{login}]; \
                 GRANT INSERT, UPDATE, DELETE ON OBJECT::app.t TO [{login}]; \
                 DENY SELECT ON app.t(label) TO [{login}];",
                db.name
            ))
            .await
            .expect("grant broadly and deny one column");

        let managed = ["app.t".parse::<pbps_model::ObjectName>().unwrap()];
        let declared: pbps_mssql::doctor::DataTables =
            [(managed[0].clone(), seeded_table())].into_iter().collect();
        let mut lp = connect_live(&as_login).await.expect("connect as the login");
        // The premise, measured: neither answer survives the column `DENY`.
        assert!(
            !holds_at_object_scope(&mut lp, "app.t", "SELECT").await,
            "{tag}: a column DENY takes the object answer down with it"
        );
        assert!(
            lp.query("SELECT code, label FROM app.t;").await.is_err(),
            "{tag}: the premise: the denied column really refuses the read"
        );

        let held = pbps_mssql::doctor::permissions(
            &mut lp,
            &managed,
            &["app".to_owned()],
            &Default::default(),
            &pbps_mssql::doctor::GrantTargets::default(),
            &declared,
            &pbps_model::IdsFile::default(),
        )
        .await
        .expect("read permissions");
        assert!(
            named_gaps(&held).contains(&"SELECT on OBJECT::[app].[t]".to_owned()),
            "{tag}: the denial is reported on the table: {held:?}"
        );

        // The control: with the denial lifted, the same grant is enough and
        // the read runs.
        //
        // `GRANT`, not `REVOKE`. Measured on the pinned image: `REVOKE SELECT
        // ON app.t(label)` leaves a column-scoped row behind in
        // `sys.database_permissions` with `state_desc = 'REVOKE'`, and that
        // row still overrides the object grant — the read fails with 230 just
        // as it did under the `DENY`. Granting the column removes the row
        // outright, which is what lets the object grant reach it again.
        db.conn
            .execute(&format!(
                "USE [{0}]; GRANT SELECT ON app.t(label) TO [{login}];",
                db.name
            ))
            .await
            .expect("lift the denial");
        let mut lp = connect_live(&as_login).await.expect("reconnect");
        lp.query("SELECT code, label FROM app.t;")
            .await
            .expect("the read runs once the denial is gone");
        let held = pbps_mssql::doctor::permissions(
            &mut lp,
            &managed,
            &["app".to_owned()],
            &Default::default(),
            &pbps_mssql::doctor::GrantTargets::default(),
            &declared,
            &pbps_model::IdsFile::default(),
        )
        .await
        .expect("read permissions");
        assert!(named_gaps(&held).is_empty(), "{tag}: {held:?}");

        drop(lp);
        drop_login(&login).await;
        db.drop().await;
    }
}

/// The count before an `exact` delete reads every table the catalog says
/// points at the parent, declared or not (issue #515).
///
/// `preflight::delete_probe` finds the children in `sys.foreign_keys` at run
/// time and deliberately does not trust the declarations to list them — "a
/// foreign key someone added by hand is exactly the one that will refuse the
/// delete". So the readiness question has to find them the same way, and a
/// deployer holding everything the declarations imply passed `doctor` and met
/// error 229 inside the count.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn the_delete_count_demands_a_read_of_every_child_the_catalog_names() {
    let mut db = TestDb::create("doctorchild515").await;
    let login = format!("pbps_child515_{}", std::process::id());
    let as_login = least_privilege_login(&mut db, &login, "pbpsLeastPrivilege!1").await;
    db.conn
        .execute(&format!(
            "USE [{0}]; EXEC(N'CREATE SCHEMA other;'); \
             GRANT VIEW DEFINITION ON SCHEMA::other TO [{login}];",
            db.name
        ))
        .await
        .expect("create the unmanaged schema");
    // The declared parent, one declared child, and three tables that are not
    // the project's: one referencing it from the managed schema, one from a
    // schema the project does not manage, one whose constraint is disabled,
    // and one that does not reference it at all.
    db.conn
        .execute(
            "CREATE TABLE app.t (code varchar(20) NOT NULL PRIMARY KEY, label nvarchar(50) NULL); \
             INSERT INTO app.t VALUES ('a', N'A'); \
             CREATE TABLE app.declared_child (id varchar(20) NOT NULL PRIMARY KEY, \
               CONSTRAINT fk_declared FOREIGN KEY (id) REFERENCES app.t(code)); \
             CREATE TABLE app.unmanaged (id varchar(20) NOT NULL PRIMARY KEY, \
               secret nvarchar(50) NULL, \
               CONSTRAINT fk_unmanaged FOREIGN KEY (id) REFERENCES app.t(code)); \
             CREATE TABLE other.far (id varchar(20) NOT NULL PRIMARY KEY, \
               CONSTRAINT fk_far FOREIGN KEY (id) REFERENCES app.t(code)); \
             CREATE TABLE app.switched_off (id varchar(20) NOT NULL PRIMARY KEY, \
               CONSTRAINT fk_off FOREIGN KEY (id) REFERENCES app.t(code)); \
             ALTER TABLE app.switched_off NOCHECK CONSTRAINT fk_off; \
             CREATE TABLE app.unrelated (id varchar(20) NOT NULL PRIMARY KEY);",
        )
        .await
        .expect("create the parent and its neighbours");
    // Everything the declarations imply, and nothing on the children: the
    // parent's own DML and read, and the schema read the declared child needs
    // taken away so that a gap on it would be visible if one were reported.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE SELECT ON SCHEMA::app FROM [{login}]; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON OBJECT::app.t TO [{login}]; \
             GRANT SELECT ON OBJECT::app.declared_child TO [{login}];",
            db.name
        ))
        .await
        .expect("grant exactly the declared demands");

    let parent: pbps_model::ObjectName = "app.t".parse().unwrap();
    let child: pbps_model::ObjectName = "app.declared_child".parse().unwrap();
    let managed = [parent.clone(), child.clone()];
    let declared: pbps_mssql::doctor::DataTables =
        [(parent.clone(), seeded_table())].into_iter().collect();
    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise, from the engine: the count's own read of the undeclared
    // child is refused, and the constraint really is visible to this login.
    assert!(
        lp.query("SELECT COUNT(*) FROM app.unmanaged;")
            .await
            .is_err(),
        "the premise: the child's read is refused"
    );

    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &declared,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        named_gaps(&held),
        [
            "SELECT on OBJECT::[app].[unmanaged]",
            "SELECT on OBJECT::[other].[far]",
        ],
        "every enabled child the catalog names, and only those: {held:?}"
    );
    // The negatives, one by one: a declared child is already asked about as a
    // managed table and is not named twice; a disabled constraint is one the
    // probe skips (DECISIONS 144); a table with no key into the parent is not
    // read at all.
    for absent in [
        "[app].[declared_child]",
        "[app].[switched_off]",
        "[app].[unrelated]",
    ] {
        assert!(
            !named_gaps(&held).iter().any(|g| g.contains(absent)),
            "{absent} must not be demanded: {held:?}"
        );
    }

    // The demand is the **probe's** width, not the child's whole catalog.
    // `app.unmanaged` has a column no key names, and a grant on the key
    // column alone authorizes the count the probe really writes — measured
    // both ways below, because the two `COUNT(*)` shapes differ: the plain
    // one names a column the engine picks for itself and is refused.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT SELECT ON app.unmanaged(id) TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the key column alone");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    assert!(
        !holds_at_object_scope(&mut lp, "app.unmanaged", "SELECT").await,
        "the premise: a column grant answers 0 at object scope"
    );
    assert!(
        lp.query("SELECT COUNT(*) FROM app.unmanaged;")
            .await
            .is_err(),
        "the premise: a bare COUNT(*) names a column this grant does not cover"
    );
    lp.query(
        "SELECT COUNT(*) FROM app.unmanaged AS ch \
         WHERE EXISTS (SELECT 1 FROM app.t AS p WHERE p.code = 'a' AND p.code = ch.id);",
    )
    .await
    .expect("the count the probe writes runs under the key column alone");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &declared,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        named_gaps(&held),
        ["SELECT on OBJECT::[other].[far]"],
        "the key column is enough for the child that has one: {held:?}"
    );

    // The control, and the remedy for the one still missing: granting the
    // reads closes the gaps and the count's own statement runs.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT SELECT ON OBJECT::app.unmanaged TO [{login}]; \
             GRANT SELECT ON OBJECT::other.far TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the children's reads");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.query(
        "SELECT COUNT(*) FROM app.unmanaged AS ch \
         WHERE EXISTS (SELECT 1 FROM app.t AS p WHERE p.code = 'a' AND p.code = ch.id);",
    )
    .await
    .expect("the count the probe writes runs once the child is readable");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &declared,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert!(named_gaps(&held).is_empty(), "{held:?}");

    // And an `ensure` declaration never emits a `DELETE`, so it runs no count
    // and is asked about no child at all.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE SELECT ON OBJECT::app.unmanaged FROM [{login}]; \
             REVOKE SELECT ON app.unmanaged(id) FROM [{login}]; \
             REVOKE SELECT ON OBJECT::other.far FROM [{login}];",
            db.name
        ))
        .await
        .expect("take the children's reads away again");
    let ensured: pbps_mssql::doctor::DataTables = [(parent, ensured_table())].into_iter().collect();
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &ensured,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert!(
        held.delete_children.is_empty(),
        "an `ensure` declaration discovers no child: {held:?}"
    );
    assert!(named_gaps(&held).is_empty(), "{held:?}");

    drop(lp);
    drop_login(&login).await;
    db.drop().await;
}

/// The read that closes an apply projects the columns the **declaration**
/// names, so a column the same plan adds has to be covered before the plan
/// runs (issue #516).
///
/// The managed `SELECT` question asks about the catalog's columns, which is
/// the right list for the probes and the wrong one for this: an account
/// granted `SELECT` on each column that exists today answers 1 there, adds the
/// column itself, and its read-back fails with 230 — after the DDL has run.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_column_the_plan_adds_is_demanded_before_the_plan_runs() {
    let mut db = TestDb::create("doctorfuture516").await;
    let login = format!("pbps_future516_{}", std::process::id());
    let as_login = least_privilege_login(&mut db, &login, "pbpsLeastPrivilege!1").await;
    db.conn
        .execute(
            "CREATE TABLE app.t (code varchar(20) NOT NULL PRIMARY KEY, label nvarchar(50) NULL); \
             INSERT INTO app.t VALUES ('a', N'A');",
        )
        .await
        .expect("create the data table");
    // The narrow shape a careful DBA writes: the read granted on exactly the
    // columns that exist, and the DML on the object.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE SELECT ON SCHEMA::app FROM [{login}]; \
             GRANT INSERT, UPDATE, DELETE ON OBJECT::app.t TO [{login}]; \
             GRANT SELECT ON app.t(code) TO [{login}]; \
             GRANT SELECT ON app.t(label) TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the read column by column");

    let name: pbps_model::ObjectName = "app.t".parse().unwrap();
    let managed = [name.clone()];
    let today: pbps_mssql::doctor::DataTables =
        [(name.clone(), seeded_table())].into_iter().collect();
    let adds_a_column: pbps_mssql::doctor::DataTables =
        [(name.clone(), seeded_table_with(&["label", "extra"]))]
            .into_iter()
            .collect();

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise: the columns that exist are readable, and the object answer
    // is 0 — the shape a column-level grant leaves.
    assert!(
        !holds_at_object_scope(&mut lp, "app.t", "SELECT").await,
        "the premise: a column-level grant answers 0 at object scope"
    );
    lp.query("SELECT code, label FROM app.t;")
        .await
        .expect("the premise: today's read runs");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &today,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert!(
        named_gaps(&held).is_empty(),
        "an unchanged table is ready: {held:?}"
    );

    // The declaration that adds a column, before anything has run. The column
    // is not in the catalog, so a catalog-sourced list cannot see it; asked as
    // declared, it answers 0 and the gap is reported on the table.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &adds_a_column,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        named_gaps(&held),
        ["SELECT on OBJECT::[app].[t]"],
        "a declared column that is not there yet is not covered: {held:?}"
    );

    // And that gap is the truth rather than caution: this login can run the
    // `ALTER` itself, and then cannot read what it just added.
    lp.execute("ALTER TABLE app.t ADD extra nvarchar(50) NULL;")
        .await
        .expect("the deployer really can add the column");
    assert!(
        lp.query("SELECT code, label, extra FROM app.t;")
            .await
            .is_err(),
        "the read-back the apply closes with is refused"
    );

    // The control, and the remedy: the object-level grant is the one that
    // reaches a column added after it.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT SELECT ON OBJECT::app.t TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the read on the object");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.query("SELECT code, label, extra FROM app.t;")
        .await
        .expect("an object-level grantee reads the column added after its grant");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &adds_a_column,
        &pbps_model::IdsFile::default(),
    )
    .await
    .expect("read permissions");
    assert!(named_gaps(&held).is_empty(), "{held:?}");

    drop(lp);
    drop_login(&login).await;
    db.drop().await;
}

/// A managed table moving between schemas is read at its **destination** after
/// the move, and no answer about the source says the deployer can (issue
/// #517).
///
/// `ALTER SCHEMA ... TRANSFER` drops every permission on the object it moves,
/// so even an object-level grant on the source is gone by the time the read
/// runs. Current-name resolution asks about the source, which is right for the
/// probes that run before the transfer and silent about everything after it.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_cross_schema_move_demands_the_destinations_read() {
    let mut db = TestDb::create("doctormove517").await;
    let login = format!("pbps_move517_{}", std::process::id());
    let as_login = least_privilege_login(&mut db, &login, "pbpsLeastPrivilege!1").await;
    db.conn
        .execute(&format!(
            "USE [{0}]; EXEC(N'CREATE SCHEMA dest;'); \
             GRANT VIEW DEFINITION, ALTER, REFERENCES ON SCHEMA::dest TO [{login}];",
            db.name
        ))
        .await
        .expect("create the destination schema");
    db.conn
        .execute(
            "CREATE TABLE app.old_name (code varchar(20) NOT NULL PRIMARY KEY, \
             label nvarchar(50) NULL); \
             INSERT INTO app.old_name VALUES ('a', N'A');",
        )
        .await
        .expect("create the table at its recorded name");

    // The declarations call it `dest.old_name`; this environment still has it
    // under `app.old_name`, and the two ids files are what say those are one
    // table. The same shape as the pending-rename case above, one securable
    // wider: the name that moves takes its schema with it.
    let uid: Uid = "t_mv5170".parse().expect("a well-formed table uid");
    let mut recorded_ids = IdsFile::default();
    recorded_ids
        .tables
        .insert(uid.clone(), "app.old_name".parse().unwrap());
    let mut project_ids = IdsFile::default();
    project_ids
        .tables
        .insert(uid, "dest.old_name".parse().unwrap());
    pbps_mssql::state::record(
        &mut db.conn,
        &snapshot(
            pbps_model::StateKind::Apply,
            &Schema::default(),
            &recorded_ids,
        ),
    )
    .await
    .expect("record the environment's own state");

    let declared: pbps_model::ObjectName = "dest.old_name".parse().unwrap();
    let managed = [declared.clone()];
    let data: pbps_mssql::doctor::DataTables = [(declared, seeded_table())].into_iter().collect();

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise, from the engine: the source is readable — the base grants
    // `SELECT` on `SCHEMA::app` — and the destination is not.
    assert!(
        holds_at_object_scope(&mut lp, "app.old_name", "SELECT").await,
        "the premise: the source read is held"
    );
    assert!(
        !holds_at_object_scope(&mut lp, "dest.old_name", "SELECT").await,
        "the premise: nothing is held at the destination"
    );

    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &managed,
        &["app".to_owned(), "dest".to_owned()],
        &Default::default(),
        &pbps_mssql::doctor::GrantTargets::default(),
        &data,
        &project_ids,
    )
    .await
    .expect("read permissions");
    // Every data demand, not only the read: the transfer drops the writes too,
    // and a report naming the read alone would go green once that was granted
    // while the first row still failed.
    let mut at_destination: Vec<String> = named_gaps(&held)
        .into_iter()
        .filter(|g| g.ends_with("SCHEMA::[dest]"))
        .collect();
    at_destination.sort();
    // `SELECT` is named twice at the destination, by the probes and by the
    // row read-back, which is the same "one permission, two reasons" shape
    // the ledger's own `SELECT` has. The reasons differ; the securable and
    // the remedy do not.
    let reasons: Vec<&str> = pbps_mssql::doctor::missing(&held)
        .iter()
        .filter(|g| g.permission == "SELECT" && g.securable() == "SCHEMA::[dest]")
        .map(|g| g.why)
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert_ne!(reasons[0], reasons[1], "{reasons:?}");
    at_destination.dedup();
    assert_eq!(
        at_destination,
        [
            "DELETE on SCHEMA::[dest]",
            "INSERT on SCHEMA::[dest]",
            "SELECT on SCHEMA::[dest]",
            "UPDATE on SCHEMA::[dest]",
        ],
        "{held:?}"
    );
    // And the source is still asked about: the probes read the table where it
    // is now, before the transfer runs.
    assert!(
        held.managed_tables
            .contains_key(&pbps_mssql::doctor::Securable::Object(
                "app.old_name".parse().unwrap()
            )),
        "the source object stays asked about: {held:?}"
    );

    // And the gap is the truth: this login can run the transfer, and then
    // cannot read the table it just moved.
    //
    // `CONTROL` on the source object is what `ALTER SCHEMA ... TRANSFER`
    // wants on top of `ALTER` on the destination, and this list deliberately
    // does not demand it — that over-demand is #352's question, not this
    // one. Granted here so the statement under test can run at all.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT CONTROL ON OBJECT::app.old_name TO [{login}];",
            db.name
        ))
        .await
        .expect("grant what the transfer itself needs");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.execute("ALTER SCHEMA dest TRANSFER app.old_name;")
        .await
        .expect("the deployer really can move it");
    assert!(
        lp.query("SELECT COUNT(*) FROM dest.old_name;")
            .await
            .is_err(),
        "the transfer drops the object's permissions with it"
    );

    // And the writes go with it, which is why the read alone is not the
    // remedy: granted `SELECT` there, the read runs and the first row does
    // not.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT SELECT ON SCHEMA::dest TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the destination read");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.query("SELECT COUNT(*) FROM dest.old_name;")
        .await
        .expect("the destination read runs once it is granted");
    assert!(
        lp.execute("INSERT INTO dest.old_name VALUES ('b', N'B');")
            .await
            .is_err(),
        "the read grant does not carry the row the declaration writes"
    );

    // The whole remedy, and the control.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT INSERT, UPDATE, DELETE ON SCHEMA::dest TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the destination writes");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.execute("INSERT INTO dest.old_name VALUES ('b', N'B');")
        .await
        .expect("the row the declaration writes runs once the writes are granted");

    drop(lp);
    drop_login(&login).await;
    db.drop().await;
}

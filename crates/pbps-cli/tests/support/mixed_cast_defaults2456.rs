//! Catalog-backed row expectations across current and earlier type moves.
use super::*;
use std::path::Path;

fn change_kind(change: &pbps_model::Change) -> String {
    format!("{change:?}")
        .split([' ', '{', '('])
        .next()
        .unwrap()
        .to_owned()
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

fn declare(d: &Demo, ty: &str, default: &str, kept: &str, rows: &str, from: Option<&str>) {
    let rename = from
        .map(|name| format!("renamed_from: {}\n", quoted(name)))
        .unwrap_or_default();
    std::fs::write(
        d.dir.join("schema/choice.type.yml"),
        format!("enum: {}\nvalues: [sad, ok, happy]\n{rename}", quoted(ty)),
    )
    .unwrap();
    d.table(&format!(
        "table: app.moods\ncolumns:\n  m: {{type: {}, nullable: false}}\n  next: {{type: {}, default: {}}}\n  kept: {{type: {}, default: {}}}\n  note: {{type: text}}\nprimary_key: [m]\ndata:\n  mode: exact\n  rows:\n{rows}",
        quoted(ty), quoted(ty), quoted(default), quoted(ty), quoted(kept)
    ));
}

fn plan(d: &Demo, db: &str, path: &Path) -> pbps_model::SavedPlan {
    succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn apply(d: &Demo, db: &str, path: &Path) -> Output {
    approved_apply(d, db, path, &["--allow", "rename,data-update,data-delete"])
}

fn snapshot(db: &str) -> pbps_model::StateSnapshot {
    pbps_model::StateSnapshot::read_json(&text_of(
        db,
        "SELECT state_json::text FROM public.__pbps_state ORDER BY id DESC LIMIT 1",
    ))
    .unwrap()
}

fn initial(server: &str, ty: &str, default: &str, other: bool) -> (OwnDatabase, Demo, PathBuf) {
    assert!(
        pbps_pg::rows::is_constant(default),
        "literal fixture: {default}"
    );
    let own = OwnDatabase::new(server, "mixed2456");
    let db = own.connection();
    on_server(
        db,
        "CREATE SCHEMA app; CREATE SCHEMA moved; CREATE SCHEMA \"App\"; CREATE SCHEMA \"Moved\"",
    );
    let d = Demo::new("mixed2456");
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
    declare(
        &d,
        ty,
        default,
        default,
        "    sad: {note: a}\n    ok: {note: b}\n",
        None,
    );
    if other {
        std::fs::write(
            d.dir.join("schema/other.type.yml"),
            "enum: app.feeling\nvalues: [other]\n",
        )
        .unwrap();
    }
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let path = d.dir.join("mixed.plan.json");
    plan(&d, db, &path);
    succeeds(apply(&d, db, &path));
    assert!(plan(&d, db, &path).changes.is_empty());
    (own, d, path)
}

fn authored(state: &pbps_model::StateSnapshot, column: &str) -> String {
    state.declared.expressions.defaults[&"app.moods".parse::<pbps_model::TableName>().unwrap()]
        [column]
        .clone()
}

fn catalog(state: &pbps_model::StateSnapshot, column: &str) -> String {
    let source = &state.schema.tables[&"app.moods".parse::<pbps_model::TableName>().unwrap()]
        .columns[column]
        .value_source;
    let pbps_model::ValueSource::Default(text) = source else {
        panic!("default absent: {source:?}")
    };
    text.clone()
}

fn assert_rows(db: &str, next: &str, count: i64) {
    assert_eq!(scalar(db, "SELECT count(*) FROM app.moods"), count);
    assert_eq!(
        text_of(db, "SELECT note FROM app.moods WHERE m::text='ok'"),
        "c"
    );
    assert_eq!(
        text_of(db, "SELECT next::text FROM app.moods WHERE m::text='ok'"),
        next
    );
    assert_eq!(
        text_of(db, "SELECT kept::text FROM app.moods WHERE m::text='ok'"),
        "sad"
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn mixed2456_rename_update_delete_use_catalog_defaults_and_converge() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        for (from, to, default) in [
            ("app.mood", "app.feeling", "'sad'::text::app.mood"),
            ("app.mood", "moved.feeling", "'sad'::varchar(3)::app.mood"),
            (
                "\"App\".\"Mood\"",
                "\"Moved\".\"select\"",
                "'sad'::\"App\".\"Mood\"::text::\"App\".\"Mood\"",
            ),
        ] {
            let (own, d, path) = initial(&server, from, default, false);
            let db = own.connection();
            let before = snapshot(db);
            let new_default = default.replace(from, to);
            declare(
                &d,
                to,
                &new_default,
                default,
                "    ok: {note: c}\n",
                Some(from),
            );
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let planned = plan(&d, db, &path);
            let kinds: Vec<_> = planned
                .changes
                .changes
                .iter()
                .map(|c| change_kind(&c.change))
                .collect();
            for kind in ["RenameType", "UpdateRow", "DeleteRow"] {
                assert!(kinds.iter().any(|k| k == kind), "{kinds:?}");
            }
            // Before cells and unchanged cells bind in the approved snapshot,
            // whereas the newly written next default remains after-side text.
            for step in &planned.changes.changes {
                if let pbps_model::Change::UpdateRow {
                    columns, unchanged, ..
                } = &step.change
                {
                    let (from, to) = &columns["next"];
                    assert_eq!(from, &pbps_model::Cell::Default(catalog(&before, "next")));
                    assert_eq!(to, &pbps_model::Cell::Default(new_default.clone()));
                    assert_eq!(
                        unchanged["kept"],
                        pbps_model::Cell::Default(catalog(&before, "kept"))
                    );
                }
            }
            succeeds(apply(&d, db, &path));
            assert_rows(db, "sad", 1);
            assert!(plan(&d, db, &path).changes.is_empty());
            assert_eq!(authored(&snapshot(db), "kept"), default);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn mixed2456_historical_authored_names_survive_later_update_delete_and_insert() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let default = "'sad'::app.mood::text::app.mood -- retained\n";
        let (own, d, path) = initial(&server, "app.mood", default, false);
        let db = own.connection();
        let original = authored(&snapshot(db), "next");
        declare(
            &d,
            "app.feeling",
            default,
            default,
            "    sad: {note: a}\n    ok: {note: b}\n",
            Some("app.mood"),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let renamed = plan(&d, db, &path);
        assert_eq!(renamed.changes.changes.len(), 1);
        assert!(matches!(
            renamed.changes.changes[0].change,
            pbps_model::Change::RenameType { .. }
        ));
        succeeds(apply(&d, db, &path));
        let renamed_state = snapshot(db);
        assert_eq!(
            authored(&renamed_state, "next").as_bytes(),
            original.as_bytes()
        );
        assert!(catalog(&renamed_state, "next").contains("app.feeling"));
        declare(
            &d,
            "app.feeling",
            default,
            default,
            "    ok: {note: c}\n    happy: {note: inserted}\n",
            Some("app.mood"),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let rows = plan(&d, db, &path);
        assert!(
            !rows
                .changes
                .changes
                .iter()
                .any(|c| matches!(c.change, pbps_model::Change::RenameType { .. }))
        );
        for kind in ["UpdateRow", "DeleteRow", "InsertRow"] {
            assert!(
                rows.changes
                    .changes
                    .iter()
                    .any(|c| change_kind(&c.change) == kind)
            );
        }
        // Offline snapshot planning uses the same catalog provenance.
        let base = d.dir.join("base.json");
        std::fs::write(&base, serde_json::to_vec(&renamed_state).unwrap()).unwrap();
        let offline = d.dir.join("offline.json");
        succeeds(d.run(&[
            "plan",
            "--no-dev",
            "--base",
            base.to_str().unwrap(),
            "--out",
            offline.to_str().unwrap(),
        ]));
        let preview: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(offline).unwrap()).unwrap();
        assert_eq!(preview.changes, rows.changes);
        succeeds(apply(&d, db, &path));
        assert_rows(db, "sad", 2);
        assert_eq!(
            text_of(db, "SELECT next::text FROM app.moods WHERE m::text='happy'"),
            "sad"
        );
        assert_eq!(
            authored(&snapshot(db), "next").as_bytes(),
            original.as_bytes()
        );
        assert!(plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn mixed2456_outside_edits_to_updated_deleted_and_null_cells_refuse_and_rollback() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        for sql in [
            "UPDATE app.moods SET next='ok' WHERE m='ok'",
            "UPDATE app.moods SET next='ok' WHERE m='sad'",
            "UPDATE app.moods SET next=NULL WHERE m='ok'",
        ] {
            let default = "'sad'::text::app.mood";
            let (own, d, path) = initial(&server, "app.mood", default, false);
            let db = own.connection();
            declare(
                &d,
                "app.feeling",
                "'sad'::text::app.feeling",
                default,
                "    ok: {note: c}\n",
                Some("app.mood"),
            );
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            plan(&d, db, &path);
            on_server(db, sql);
            let refused = apply(&d, db, &path);
            assert_ne!(code(&refused), 0, "{sql}");
            assert!(
                !stderr(&refused).contains("does not exist"),
                "{}",
                stderr(&refused)
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*) FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='app' AND t.typname='mood'"
                ),
                1
            );
            assert_eq!(scalar(db, "SELECT count(*) FROM app.moods"), 2);
            assert_eq!(text_of(db, "SELECT note FROM app.moods WHERE m='ok'"), "b");
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*) FROM public.__pbps_state WHERE kind='apply'"
                ),
                1
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn mixed2456_rename_chains_reused_names_and_new_defaults_keep_their_source_uid() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let default = "'sad'::text::app.mood";
        let (own, d, path) = initial(&server, "app.mood", default, true);
        let db = own.connection();
        std::fs::write(
            d.dir.join("schema/other.type.yml"),
            "enum: app.last\nrenamed_from: app.feeling\nvalues: [other]\n",
        )
        .unwrap();
        // Absorb each identity decision in its own source revision, as the
        // existing rename-chain fixture does, without applying either yet.
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        declare(
            &d,
            "app.feeling",
            "'ok'::text::app.feeling",
            default,
            "    ok: {note: c}\n    happy: {note: inserted}\n",
            Some("app.mood"),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        std::fs::write(
            d.dir.join("schema/reused.type.yml"),
            "enum: app.mood\nvalues: [replacement]\n",
        )
        .unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let changes = plan(&d, db, &path);
        assert_eq!(
            changes
                .changes
                .changes
                .iter()
                .filter(|c| matches!(c.change, pbps_model::Change::RenameType { .. }))
                .count(),
            2
        );
        succeeds(apply(&d, db, &path));
        assert_rows(db, "ok", 2);
        assert_eq!(
            text_of(db, "SELECT kept::text FROM app.moods WHERE m::text='happy'"),
            "sad"
        );
        assert_eq!(authored(&snapshot(db), "kept"), default);
        assert!(plan(&d, db, &path).changes.is_empty());
    }
}

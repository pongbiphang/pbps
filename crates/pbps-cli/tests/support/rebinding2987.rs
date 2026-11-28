//! A connected plan made without a resolver rebuilds an unchanged module for
//! an arriving name only where the engine's record does not rule the binding
//! out (#2987, DEC-2987.5).
use super::*;

const TABLES: [(&str, &str); 2] = [
    ("app.t.yml", "table: app.t\ncolumns:\n  id: {type: int}\n"),
    (
        "app.u.yml",
        "table: app.u\ncolumns:\n  id: {type: int}\n  total: {type: int}\n",
    ),
];

/// Three unchanged modules whose text mentions `total`, which then arrives as
/// a routine:
///
/// - `v_alias` only names an output column so. The engine recorded no
///   dependency on anything of that name: not rebuilt.
/// - `v_column` reads a column of that name, which column notation could turn
///   into a call: still rebuilt.
/// - `f_body()` is PL/pgSQL, whose body the engine did not bind and so
///   recorded nothing for: still rebuilt.
const MODULES: [(&str, &str); 3] = [
    (
        "app.v_alias.view.yml",
        "view: app.v_alias\ndefinition: SELECT id AS total FROM app.t\n",
    ),
    (
        "app.v_column.view.yml",
        "view: app.v_column\ndefinition: SELECT total FROM app.u\n",
    ),
    (
        "app.f_body%28%29.function.yml",
        "function: app.f_body()\npublic_execute: true\ndefinition: |-\n  () RETURNS integer LANGUAGE plpgsql AS $body$ DECLARE total integer := 0; BEGIN RETURN total; END $body$\n",
    ),
];

const ARRIVAL: (&str, &str) = (
    "app.total%28integer%29.function.yml",
    "function: app.total(integer)\npublic_execute: true\ndefinition: |-\n  (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1\n",
);

fn write(d: &Demo, files: &[(&str, &str)]) {
    for (name, text) in files {
        std::fs::write(d.dir.join("schema").join(name), text).unwrap();
    }
}

/// The unchanged modules a saved plan restates.
fn restated(plan: &std::path::Path) -> Vec<String> {
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    let mut found: Vec<String> = saved
        .changes
        .changes
        .iter()
        .filter_map(|planned| {
            if let pbps_model::Change::Module(pbps_model::ModuleChange::AlterModule {
                id, ..
            }) = &planned.change
            {
                Some(id.raw().to_string())
            } else {
                None
            }
        })
        .collect();
    found.sort();
    found
}

pub(crate) fn a_mention_the_engine_did_not_bind_is_not_rebuilt(server: &str) {
    let own = OwnDatabase::new(server, "rebinding2987");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("rebinding2987");
    write(&d, &TABLES);
    write(&d, &MODULES);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    write(&d, &[ARRIVAL]);
    // Offline there is only the text, and all three mention the name.
    let offline = d.dir.join("offline.sql");
    succeeds(d.run(&["plan", "--sql", offline.to_str().unwrap()]));
    let offline = std::fs::read_to_string(offline).unwrap();
    for module in ["v_alias", "v_column", "f_body"] {
        assert!(offline.contains(module), "{module}:\n{offline}");
    }
    d.commit();

    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    assert_eq!(restated(&plan), ["app.f_body()", "app.v_column"]);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    // The view that was left alone is the one the target already had, and
    // the routine that arrived is there beside it.
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_views WHERE schemaname = 'app' AND viewname = 'v_alias'"
        ),
        1
    );
    assert_eq!(scalar(connection, "SELECT app.total(7)::bigint"), 7);
}

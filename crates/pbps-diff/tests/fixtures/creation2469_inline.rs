//! Engine-required groups use ordinary annotated declaration planning.
#![allow(dead_code)]
use super::fixture::Fixture;
use pbps_model::*;

pub fn temporal(versioned: bool) -> Fixture {
    let mut f = Fixture::new("column", "mssql");
    let name: TableName = "app.t".parse().unwrap();
    let table = f.schema.tables.get_mut(&name).unwrap();
    let x = table.columns.shift_remove("x").unwrap();
    for (column, uid) in [("vf", "c_dddddd"), ("vt", "c_eeeeee")] {
        table.columns.insert(
            column.into(),
            Column::new("datetime2".parse().unwrap()).not_null(),
        );
        f.ids
            .columns
            .insert(uid.parse().unwrap(), name.column(column));
    }
    table.columns.insert("x".into(), x);
    table.primary_key = versioned.then(|| {
        serde_json::from_value(serde_json::json!({"name":"pk_t","columns":["anchor"]})).unwrap()
    });
    table.system_time = Some(SystemTime {
        start: "vf".into(),
        end: "vt".into(),
        hidden: false,
        versioning: versioned.then(|| SystemVersioning {
            history: "app.t_history".parse().unwrap(),
            retention: None,
        }),
    });
    f
}

pub fn partitioned() -> Fixture {
    let mut f = Fixture::new("column", "postgres");
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    table.partition_by = Some(PartitionBy {
        columns: vec!["anchor".into()],
        strategy: Default::default(),
        external: false,
    });
    f
}

pub fn identity(engine: &str) -> Fixture {
    let mut f = Fixture::new("not_null", engine);
    let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    table.columns.get_mut("x").unwrap().value_source = ValueSource::Identity(Identity::new(1, 1));
    f
}

pub fn require(mut f: Fixture, surface: &str) -> Fixture {
    f.deps.0[0].0 = surface.parse().unwrap();
    f
}

/// A held earlier demand must not swallow another demand's legal late suffix.
pub fn separated_providers(engine: &str) -> Fixture {
    let mut f = if engine == "postgres" {
        partitioned()
    } else {
        temporal(true)
    };
    let late = if engine == "postgres" {
        let table = f.schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        table.partition_by.as_mut().unwrap().columns = vec!["x".into()];
        table
            .columns
            .insert("y".into(), Column::new("integer".parse().unwrap()));
        f.ids
            .columns
            .insert("c_ffffff".parse().unwrap(), "app.t.y".parse().unwrap());
        "y"
    } else {
        "x"
    };
    f.schema.modules.insert(
        "app.after".parse().unwrap(),
        f.schema.modules[&"app.f".parse().unwrap()].clone(),
    );
    f.deps = pbps_model::object_ref::ObjectDeps(vec![
        (
            "column::app.t.anchor".parse().unwrap(),
            ["app.f".parse().unwrap()].into(),
        ),
        (
            format!("column::app.t.{late}").parse().unwrap(),
            ["app.after".parse().unwrap()].into(),
        ),
        (
            "app.after".parse().unwrap(),
            ["table::app.t".parse().unwrap()].into(),
        ),
    ]);
    f
}

pub fn separated_generation() -> Fixture {
    let mut f = separated_providers("postgres");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("anchor")
        .unwrap()
        .value_source = ValueSource::Generated(Generated {
        expression: "app.f()".into(),
        stored: true,
    });
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .checks
        .insert(
            "ck_key".into(),
            CheckConstraint {
                expression: "x >= 0".into(),
            },
        );
    f
}

//! Shared declarations for ordinary and qualified #1503 regressions.
use pbps_model::{
    Column, Generated, Module, ModuleKind, PartitionBy, Schema, Table, TableName, ValueSource,
};

pub(crate) fn shape(case: &str) -> Schema {
    let mut schema = Schema::default();
    let plain = || Column::new("integer".parse().unwrap());
    let generated = |expression: &str| {
        let mut c = plain();
        c.value_source = ValueSource::Generated(Generated {
            expression: expression.into(),
            stored: true,
        });
        c
    };
    let mut a = Table::default();
    a.columns.insert("id".into(), plain());
    a.columns.insert(
        "g".into(),
        generated(if case == "later" {
            "app.f(z)"
        } else {
            "app.f(id)"
        }),
    );
    a.columns.insert("z".into(), plain());
    if case == "partition" {
        a.partition_by = Some(PartitionBy {
            strategy: pbps_model::PartitionStrategy::Range,
            columns: vec!["z".into()],
            external: false,
        });
    }
    if case == "rows" {
        a.columns.get_mut("id").unwrap().nullable = false;
        a.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("a_pk".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        a.unique.insert(
            "a_z".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["z".into()],
                storage_parameters: Default::default(),
            },
        );
        a.checks.insert(
            "a_ck".into(),
            pbps_model::CheckConstraint {
                expression: "g = app.f(id)".into(),
            },
        );
        a.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: [(
                "1".into(),
                pbps_model::Row([("z".into(), pbps_model::Value::Int(4))].into()),
            )]
            .into(),
        });
    }
    schema.tables.insert(TableName::new("app", "a"), a);
    if case == "partition" {
        schema.tables.insert(
            TableName::new("app", "a_default"),
            Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: TableName::new("app", "a"),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Table::default()
            },
        );
    }
    schema.modules.insert(
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            // The producer must support a generation function that binds
            // only the prefix, which the old whole-table hold refused.
            definition: if case == "plain" {
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT id FROM app.a LIMIT 1; END"
            } else {
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2"
            }.into(),
        },
    );
    if ["literal", "reverse", "array", "collision"].contains(&case) {
        let mut b = Table::default();
        b.columns.insert("id".into(), plain());
        let literal = match case {
            "array" => "'app._a'::regtype::oid::integer",
            "collision" => "'app._a_1'::regtype::oid::integer",
            _ => "'app.a'::regclass::oid::integer",
        };
        b.columns.insert("r".into(), generated(literal));
        if case == "reverse" {
            b.columns.insert("g".into(), generated("app.f2(id)"));
            schema
                .tables
                .get_mut(&TableName::new("app", "a"))
                .unwrap()
                .columns
                .insert("r".into(), generated("'app.b'::regclass::oid::integer"));
            schema.modules.insert(
                "app.f2(integer)".parse().unwrap(),
                Module {
                    kind: ModuleKind::Function,
                    description: None,
                    definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
                },
            );
        }
        schema.tables.insert(TableName::new("app", "b"), b);
        if case == "collision" {
            let mut occupied = Table::default();
            occupied.columns.insert("id".into(), plain());
            schema.tables.insert(TableName::new("app", "_a"), occupied);
        }
    }
    schema.modules.insert(
        "app.v".parse().unwrap(),
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT * FROM app.a".into(),
        },
    );
    schema
}

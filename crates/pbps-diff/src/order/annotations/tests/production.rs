//! The production differ takes the general route for extended input and the
//! legacy adapters otherwise (#2469 C3a).
use super::*;
use crate::order::deployment::Sources;
use crate::order::deployment::schedule::{Declarations, Facts, schedule};
use crate::{DiffError, Side};
use pbps_dialect::MinimalDialect;
use pbps_model::object_ref::Annotations;
use pbps_model::{ComputedColumn, Hints};

fn context() -> crate::Context {
    crate::Context {
        operator: "test".into(),
        today: "2026-11-11".into(),
    }
}

fn function() -> Module {
    Module {
        kind: ModuleKind::Function,
        definition: "(a int) RETURNS int AS BEGIN RETURN a END".into(),
        description: None,
        options: Default::default(),
    }
}

/// A new table whose computed column calls a function the same plan creates.
fn world() -> (Schema, IdsFile) {
    let mut t = Table {
        columns: [
            ("id".into(), Column::new("int".parse().unwrap())),
            ("a2".into(), Column::new("int".parse().unwrap())),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    t.computed.insert(
        "c".into(),
        ComputedColumn {
            expression: "dbo.f(a2)".into(),
            persisted: false,
            not_null: false,
        },
    );
    let mut schema = Schema::default();
    schema.tables.insert("dbo.t".parse().unwrap(), t);
    schema.modules.insert("dbo.f".parse().unwrap(), function());
    let ids = crate::resolve(&schema, &IdsFile::default(), &[], &context())
        .unwrap()
        .ids;
    (schema, ids)
}

fn plan(schema: &Schema, ids: &IdsFile, annotations: Annotations) -> Result<Vec<String>, String> {
    let empty = Schema::default();
    let none = IdsFile::default();
    let before = Side {
        schema: &empty,
        ids: &none,
    };
    let hints = Hints {
        annotations,
        ..Default::default()
    };
    let planned =
        crate::diff(before, Side { schema, ids }, &MinimalDialect, &hints).map_err(|errors| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        })?;
    let scheduled = schedule(
        planned,
        &Declarations {
            schema,
            sources: Sources { before, after: ids },
            annotations: &hints.annotations,
        },
        &Facts::default(),
        &MinimalDialect,
    )
    .map_err(|e| e.to_string())?;
    Ok(scheduled
        .changes
        .changes()
        .changes
        .iter()
        .map(|p| {
            if let Change::CreateModule { id, .. } = &p.change {
                format!("module {}", id.shown())
            } else if let Change::CreateTable { name, .. } = &p.change {
                format!("table {}", name.shown())
            } else {
                "other".into()
            }
        })
        .collect())
}

fn declared(pairs: &[(&str, &[&str])]) -> Annotations {
    Annotations {
        declared: dependencies(pairs),
        ..Default::default()
    }
}

#[test]
fn a_module_only_plan_needs_no_table_identity() {
    // The contract binds every table of both states, so building it for a
    // module-only plan would fail where the plan has no recorded identities.
    // A module-only input never builds it.
    let (schema, _) = world();
    let mut views = Schema {
        tables: schema.tables.clone(),
        ..Default::default()
    };
    views.modules.insert("dbo.v".parse().unwrap(), view());
    views.modules.insert("dbo.w".parse().unwrap(), view());
    // The table is in the schema and nothing records its identity.
    let order = plan(
        &views,
        &IdsFile::default(),
        declared(&[("dbo.v", &["dbo.w"])]),
    )
    .unwrap();
    assert!(
        order.iter().position(|c| c == "module dbo.w")
            < order.iter().position(|c| c == "module dbo.v"),
        "{order:?}"
    );
}

#[test]
fn an_extended_annotation_discharges_the_deferred_computed_call() {
    let (schema, ids) = world();
    let refused = plan(&schema, &ids, Annotations::default()).unwrap_err();
    assert!(refused.contains("computed column"), "{refused}");
    let order = plan(
        &schema,
        &ids,
        declared(&[("generation::dbo.t.c", &["module::dbo.f"])]),
    )
    .unwrap();
    assert!(
        order.iter().position(|c| c == "module dbo.f")
            < order.iter().position(|c| c == "table dbo.t"),
        "{order:?}"
    );
}

#[test]
fn an_extended_annotation_that_orders_nothing_relevant_still_refuses() {
    let (schema, ids) = world();
    let why = plan(
        &schema,
        &ids,
        declared(&[("module::dbo.f", &["table::dbo.t"])]),
    );
    // `dbo.f` after the table is the order the refusal is about.
    assert!(why.is_err_and(|why| why.contains("computed column")));
}

#[test]
fn an_extended_annotation_without_identity_is_named_not_dropped() {
    let (schema, _) = world();
    let why = plan(
        &schema,
        &IdsFile::default(),
        declared(&[("generation::dbo.t.c", &["module::dbo.f"])]),
    )
    .unwrap_err();
    assert!(why.contains("dbo.t"), "{why}");
}

#[test]
fn the_deferral_exists_only_for_extended_input() {
    let (schema, ids) = world();
    // The same refusal, as the differ's own error, when no contract exists.
    let empty = Schema::default();
    let none = IdsFile::default();
    let errors = crate::diff(
        Side {
            schema: &empty,
            ids: &none,
        },
        Side {
            schema: &schema,
            ids: &ids,
        },
        &MinimalDialect,
        &Hints::default(),
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, DiffError::ComputedFunctionChanged { .. }))
    );
}

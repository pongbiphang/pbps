//! The ordering benchmark (#1965, S1b-iii): today's pipeline against the
//! graph the effects build and the operation-level verifier, on four shapes.
//!
//! Run at full size for the report on #1965, in release:
//!
//! ```text
//! PBPS_ORDER_BENCHMARK=1 cargo test --release -p pbps-diff --lib -- --nocapture order::benchmark
//! ```
//!
//! The numbers are not a gate: they depend on the host. What they say is how
//! each side grows with the plan, and whether the verifier costs little
//! enough to refuse a plan at plan time in a release build (#1776 decides
//! from it). Without the variable the same shapes run small, as an ordinary
//! test: each must plan, and the effects must find no violation in the plan
//! the pipeline orders, so a shape that stops building is caught before the
//! next report needs it.

use std::time::{Duration, Instant};

use indexmap::IndexMap;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    CheckConstraint, Column, Hints, IdsFile, Index, IndexColumn, Module, ModuleDeps, ModuleId,
    ModuleKind, Schema, Table, TableName,
};

use super::edges::{Baseline, Checks, graph};
use super::shadow::analyse;
use super::verify::verify_with;

/// The report's size, about 5,000 changes a shape.
const FULL: usize = 5_000;
/// The ordinary test's.
const SMALL: usize = 40;

fn name(s: &str) -> TableName {
    s.parse().expect("a table name")
}

fn table(columns: &[String]) -> Table {
    Table {
        columns: columns
            .iter()
            .map(|c| (c.clone(), Column::new("int".parse().expect("a type"))))
            .collect::<IndexMap<_, _>>(),
        ..Table::default()
    }
}

fn ctx() -> crate::Context {
    crate::Context {
        operator: "benchmark".into(),
        today: "2026-10-16".into(),
    }
}

/// A base and a desired state, and the annotations the desired state carries.
struct Shape {
    name: &'static str,
    base: Schema,
    desired: Schema,
    declared: ModuleDeps,
    /// For a shape no order satisfies: the annotations whose plan supplies
    /// the operations the effects are measured on.
    planned_with: Option<ModuleDeps>,
}

/// About `size` tables created, none touching another.
fn independent(size: usize) -> Shape {
    let mut desired = Schema::default();
    for i in 0..size {
        desired
            .tables
            .insert(name(&format!("app.t{i}")), table(&["id".into()]));
    }
    Shape {
        name: "independent",
        base: Schema::default(),
        desired,
        declared: ModuleDeps::new(),
        planned_with: None,
    }
}

/// About `size` changes on four tables: columns, indexes on them and
/// checks reading them, every one sharing its table with a thousand others.
fn dense(size: usize) -> Shape {
    let mut base = Schema::default();
    let mut desired = Schema::default();
    for t in 0..4 {
        let n = name(&format!("app.t{t}"));
        base.tables.insert(n.clone(), table(&["id".into()]));
        let columns: Vec<String> = std::iter::once("id".to_owned())
            .chain((0..size / 8).map(|c| format!("c{c}")))
            .collect();
        let mut d = table(&columns);
        for c in 0..size / 16 {
            d.indexes.insert(
                format!("ix{c}"),
                Index {
                    columns: vec![IndexColumn::column(format!("c{c}"))],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            d.checks.insert(
                format!("ck{c}"),
                CheckConstraint {
                    expression: format!("c{c} > 0"),
                },
            );
        }
        desired.tables.insert(n, d);
    }
    Shape {
        name: "dense",
        base,
        desired,
        declared: ModuleDeps::new(),
        planned_with: None,
    }
}

/// `size` views, each declared to depend on the one before: one chain.
fn chain(size: usize) -> Shape {
    let mut desired = Schema::default();
    let mut declared = ModuleDeps::new();
    let id = |i: usize| -> ModuleId { format!("app.v{i}").parse().expect("a module id") };
    for i in 0..size {
        desired.modules.insert(
            id(i),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT 1 AS n".into(),
            },
        );
        if i > 0 {
            declared.insert(id(i), [id(i - 1)].into_iter().collect());
        }
    }
    Shape {
        name: "chain",
        base: Schema::default(),
        desired,
        declared,
        planned_with: None,
    }
}

/// The chain with one more declaration closing it: the first view reads the
/// last. No order satisfies it. The pipeline plans a cycle's members in name
/// order (`pbps_model::module::creation_order`), and is timed doing so; the
/// effects are measured on the chain's own plan under the cycle's
/// declarations, where the verifier must find the one declaration that
/// order breaks. (The test build's observer refuses the name order itself,
/// #2083. A rename ring is no such shape: the differ refuses every rename
/// cycle, DEC-1966.1.)
fn cycle(size: usize) -> Shape {
    let chain = chain(size);
    let mut declared = chain.declared.clone();
    let id = |i: usize| -> ModuleId { format!("app.v{i}").parse().expect("a module id") };
    declared.insert(id(0), [id(size - 1)].into_iter().collect());
    Shape {
        name: "cycle",
        base: chain.base,
        desired: chain.desired,
        declared,
        planned_with: Some(chain.declared),
    }
}

/// One row of the report.
struct Measured {
    shape: &'static str,
    operations: usize,
    pipeline: Duration,
    settle_edges: usize,
    settle: Duration,
    effects: Duration,
    effect_edges: usize,
    graph: Duration,
    verify: Duration,
    violations: usize,
}

/// Measures `shape`; `reversed` runs its operations backwards instead of in
/// the pipeline's order, for the negative.
fn measure(shape: Shape, reversed: bool) -> Measured {
    let dialect = pbps_dialect::MinimalDialect;
    let base_ids = crate::resolve(&shape.base, &IdsFile::default(), &[], &ctx())
        .expect("resolve the base")
        .ids;
    let ids = crate::resolve(&shape.desired, &base_ids, &[], &ctx())
        .expect("resolve the desired state")
        .ids;
    let hints = |declared: &ModuleDeps| Hints {
        module_deps: declared.clone(),
        ..Hints::default()
    };
    let base = crate::Side {
        schema: &shape.base,
        ids: &base_ids,
    };
    let desired = crate::Side {
        schema: &shape.desired,
        ids: &ids,
    };
    // Today's pipeline: the differ and every pass after it.
    // Today's pipeline as a release build runs it: `diff` without the
    // observer a test build adds after it, which reads the effects, builds
    // the graph and verifies, the costs measured apart below. Without it a
    // declared cycle plans too, in name order, and is timed as itself.
    let started = Instant::now();
    let planned = crate::schema_diff::diff_partial_rebuilding(
        base,
        desired,
        &dialect,
        &hints(&shape.declared),
        &std::collections::BTreeSet::new(),
        crate::schema_diff::Rebinding::Candidates,
        crate::schema_diff::Screen::Text,
        None,
    );
    let pipeline = started.elapsed();
    assert!(
        planned.errors.is_empty(),
        "{}: {:?}",
        shape.name,
        planned.errors
    );
    // The operations the effects read: a cycle's are the chain's own plan,
    // which honours every declaration but the one closing it, so the
    // verifier's finding is that one alone.
    let changes = match &shape.planned_with {
        Some(acyclic) => {
            crate::schema_diff::diff_partial_rebuilding(
                base,
                desired,
                &dialect,
                &hints(acyclic),
                &std::collections::BTreeSet::new(),
                crate::schema_diff::Rebinding::Candidates,
                crate::schema_diff::Screen::Text,
                None,
            )
            .changes
        }
        None => planned.changes,
    };
    // The effects, the graph they imply, and the replay.
    let mut operations = Operations::of_plan(&changes.changes, ModuleEdits::InPlace);
    if reversed {
        let backwards: Vec<usize> = (0..operations.as_slice().len()).rev().collect();
        operations = operations.reordered(&backwards);
    }
    let names = PlanNames::of(&changes.changes, &ids).expect("plan names");
    let owners = PlanOwners::new(names, &base_ids, &ids);
    let before = Snapshot {
        schema: &shape.base,
        ids: &base_ids,
    };
    let after = Snapshot {
        schema: &shape.desired,
        ids: &ids,
    };
    let started = Instant::now();
    let shadowed = analyse(&operations, &owners, before, after, &dialect);
    let baseline = Baseline::of(before, &owners);
    let effects = started.elapsed();
    let ops = operations.as_slice();
    let started = Instant::now();
    let g = graph(
        ops,
        &shadowed.effects,
        &baseline,
        &shape.declared,
        Checks::ALL,
    );
    let graph_time = started.elapsed();
    // What the resolver does with the graph: settle it into an order.
    let started = Instant::now();
    let schedule = super::Schedule::of_graph(&g, ops);
    let settled = schedule.sequence();
    let settle = started.elapsed();
    let settle_edges = schedule.constraints().len();
    assert!(settled.is_ok(), "{}: {settled:?}", shape.name);
    let started = Instant::now();
    let violations = verify_with(
        ops,
        &shadowed.effects,
        &baseline,
        &shape.declared,
        Checks::ALL,
    );
    let verify = started.elapsed();
    Measured {
        shape: shape.name,
        operations: ops.len(),
        pipeline,
        settle_edges,
        settle,
        effects,
        effect_edges: g.edges.len(),
        graph: graph_time,
        verify,
        violations: violations.len(),
    }
}

#[test]
fn the_benchmark_shapes_plan_and_the_effects_find_only_the_cycle() {
    let size = if std::env::var_os("PBPS_ORDER_BENCHMARK").is_some() {
        FULL
    } else {
        SMALL
    };
    let ms = |d: Duration| format!("{:.1}", d.as_secs_f64() * 1000.0);
    println!(
        "| shape | operations | pipeline ms | settle edges | settle ms \
         | effects ms | effect edges | graph ms | verify ms | violations |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for shape in [independent(size), dense(size), chain(size), cycle(size)] {
        let m = measure(shape, false);
        // Only the cycle's order breaks a declaration: the one closing it.
        let cycle = m.shape == "cycle";
        assert_eq!(m.violations, usize::from(cycle), "{}: violations", m.shape);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            m.shape,
            m.operations,
            ms(m.pipeline),
            m.settle_edges,
            ms(m.settle),
            ms(m.effects),
            m.effect_edges,
            ms(m.graph),
            ms(m.verify),
            m.violations
        );
    }
}

/// The assertion above can fail: the chain run backwards puts every view
/// before the one it reads.
#[test]
fn a_chain_run_backwards_is_refused_by_the_effects() {
    let m = measure(chain(SMALL), true);
    assert_eq!(m.violations, SMALL - 1, "one per link of the chain");
}

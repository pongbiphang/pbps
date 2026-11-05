//! Ordering from the bindings a run captured (#2143 c3, DEC-2143.8): a
//! schema-bound caller the capture read whole brings its own callees, and a
//! caller it did not read keeps the lexical match.

use pbps_model::operation::Captured;
use pbps_model::resolver::{BoundSurface, ObjectIdentity, Surface, SurfaceResolution};
use pbps_model::{Change, IdsFile, Module, ModuleId, ModuleKind, Schema, TableName};
use pbps_mssql::Mssql;
use std::collections::{BTreeMap, BTreeSet};

fn id(name: &str) -> ModuleId {
    ModuleId::Named(TableName::new("app", name))
}

fn function(body: &str) -> Module {
    Module {
        kind: ModuleKind::Function,
        definition: format!("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN {body} END"),
        description: None,
        options: Default::default(),
    }
}

fn view(body: &str) -> Module {
    Module {
        kind: ModuleKind::View,
        definition: body.into(),
        description: None,
        options: pbps_model::ModuleOptions {
            schemabinding: true,
            ..Default::default()
        },
    }
}

/// One observation per module, none of them a binding change: the plan is
/// ordered by the effects alone.
fn observed(schema: &Schema) -> Vec<SurfaceResolution> {
    schema
        .modules
        .keys()
        .map(|module| {
            let bound = BoundSurface {
                object: ObjectIdentity {
                    class: "module".into(),
                    name: vec![module.shown().to_string()],
                    signature: vec![],
                },
                bindings: vec![],
                managed_inputs: BTreeSet::new(),
            };
            SurfaceResolution {
                surface: Surface::Module(module.clone()),
                current: Some(bound.clone()),
                desired: Some(bound),
            }
        })
        .collect()
}

#[allow(clippy::wildcard_enum_match_arm)]
fn plan(
    before: &Schema,
    after: &Schema,
    captured: &Captured,
) -> Result<Vec<ModuleId>, pbps_diff::resolver::Error> {
    let ids = IdsFile::default();
    let ordered = pbps_diff::resolver::plan_captured(
        pbps_diff::Side {
            schema: before,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: after,
            ids: &ids,
        },
        &Default::default(),
        &observed(before),
        &Mssql,
        captured,
    )?;
    Ok(ordered
        .changes
        .changes
        .iter()
        .filter_map(|planned| match &planned.change {
            Change::AlterModule { id, .. } | Change::CreateModule { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect())
}

/// A schema-bound function whose body reads column `app.v` of a table aliased
/// `app`, beside a view `app.v` the edit makes call the function. The lexer
/// reads `app.v` in the function as a reference to the view, so the function
/// would wait for the view while the view waits for the function.
fn alias_column() -> (Schema, Schema) {
    let mut before = Schema::default();
    before
        .modules
        .insert(id("f"), function("(SELECT TOP 1 app.v FROM dbo.t AS app)"));
    before.modules.insert(id("v"), view("SELECT 1 AS x"));
    let mut after = before.clone();
    after.modules.insert(
        id("f"),
        function("(SELECT TOP 1 app.v FROM dbo.t AS app) + 1"),
    );
    after.modules.insert(id("v"), view("SELECT app.f(1) AS x"));
    (before, after)
}

/// What the engine bound each module to at creation: the function reads no
/// module, and the view reads the function.
fn captured_alias_column() -> Captured {
    Captured {
        before: BTreeMap::new(),
        after: BTreeMap::from([
            (id("f"), BTreeSet::new()),
            (id("v"), BTreeSet::from([id("f")])),
        ]),
    }
}

#[test]
fn a_captured_caller_does_not_wait_for_the_view_its_alias_column_is_spelled_like() {
    let (before, after) = alias_column();
    let order = plan(&before, &after, &captured_alias_column())
        .expect("the function binds no module, so the pair has no cycle");
    let at = |module: &ModuleId| order.iter().position(|m| m == module).unwrap();
    assert!(
        at(&id("f")) < at(&id("v")),
        "the view binds the function, so the function is made final first: {order:?}"
    );
}

#[test]
fn a_caller_without_a_capture_keeps_the_lexical_order() {
    let (before, after) = alias_column();
    // Without a resolver the alias column still reads as a call of the view,
    // and the pair refuses by name.
    assert!(plan(&before, &after, &Captured::default()).is_err());
    // A capture that read only the view does not vouch for the function: it
    // is absent from the map, so its callees are still the lexical guess.
    let partial = Captured {
        before: BTreeMap::new(),
        after: BTreeMap::from([(id("v"), BTreeSet::from([id("f")]))]),
    };
    assert!(
        plan(&before, &after, &partial).is_err(),
        "an uncaptured caller keeps the lexical match"
    );
}

#[test]
fn a_captured_callee_the_text_does_not_name_is_still_waited_for() {
    // The body names no module (a dynamic name, a synonym the engine
    // resolved): only the capture knows what `app.f` is bound to.
    let mut before = Schema::default();
    before.modules.insert(id("f"), function("0"));
    before.modules.insert(id("g"), function("0"));
    let mut after = before.clone();
    after.modules.insert(id("f"), function("1"));
    after.modules.insert(id("g"), function("1"));
    let captured = Captured {
        before: BTreeMap::new(),
        after: BTreeMap::from([
            (id("f"), BTreeSet::from([id("g")])),
            (id("g"), BTreeSet::new()),
        ]),
    };
    let order = plan(&before, &after, &captured).unwrap();
    let at = |module: &ModuleId| order.iter().position(|m| m == module).unwrap();
    assert!(at(&id("g")) < at(&id("f")), "{order:?}");
    // The same plan with no capture binds nothing, so nothing holds `f` back.
    let lexical = plan(&before, &after, &Captured::default()).unwrap();
    assert_eq!(lexical.len(), 2);
}

/// The passes that settle the plan again after the resolver, the catalog-backed
/// requirements, read the plan's effects a second
/// time. The plan carries the capture that ordered it, so they read the same
/// bindings and do not bring the lexical edge back (review of #2680).
#[test]
fn the_passes_after_the_resolver_read_the_bindings_that_ordered_the_plan() {
    let (before, after) = alias_column();
    let ids = IdsFile::default();
    let sides = || {
        (
            pbps_diff::Side {
                schema: &before,
                ids: &ids,
            },
            pbps_diff::Side {
                schema: &after,
                ids: &ids,
            },
        )
    };
    let ordered = |captured: &Captured| {
        let (b, a) = sides();
        pbps_diff::resolver::plan_captured(
            b,
            a,
            &Default::default(),
            &observed(&before),
            &Mssql,
            captured,
        )
    };
    let settle = |plan: pbps_diff::order::CanonicalPlan<Mssql>| {
        let (b, a) = sides();
        let catalog = pbps_dialect::slots::CatalogFacts::default();
        pbps_diff::order::catalog_requirements(
            plan,
            b,
            a,
            &Mssql,
            &Default::default(),
            &catalog,
            &[],
        )
        .map(|_| ())
    };
    let captured = ordered(&captured_alias_column()).expect("planned from the capture");
    assert_eq!(settle(captured.changes), Ok(()));
    // A plan that was not ordered from a capture reads the lexical edges on
    // every pass, as it always did.
    assert!(ordered(&Captured::default()).is_err());
}

/// The plan a resolver's scratch compiles is built from nothing, and the
/// alias pair is in it: the function's text spells the view's name, the view's
/// text calls the function. Nothing runs that order, since scratch settles its
/// own with the text edges as guesses (DEC-2143.9), so it is not judged as one.
#[test]
#[allow(clippy::wildcard_enum_match_arm)]
fn a_bootstrap_of_modules_whose_texts_name_each_other_is_not_judged_as_an_order() {
    let (_, desired) = alias_column();
    let ids = IdsFile::default();
    let plan = pbps_diff::bootstrap(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    )
    .expect("the declarations have a bootstrap plan");
    let created: BTreeSet<_> = plan
        .changes()
        .changes
        .iter()
        .filter_map(|planned| match &planned.change {
            Change::CreateModule { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(created, BTreeSet::from([id("f"), id("v")]));
}

/// Negative: an ordinary plan from nothing is still judged by the debug
/// observer, so the exemption is the bootstrap's alone.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "effects contradict this order")]
fn an_ordinary_plan_of_the_same_pair_is_still_judged_as_an_order() {
    let (_, desired) = alias_column();
    let ids = IdsFile::default();
    let _ = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    );
}

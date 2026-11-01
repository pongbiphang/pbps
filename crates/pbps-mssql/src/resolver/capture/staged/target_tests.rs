use super::super::super::{
    read::{Catalog, Dependency, Module as CatalogModule, Object},
    select,
    tests::{catalog, producer},
};
use super::*;
use pbps_model::ModuleId;
use std::collections::BTreeSet;

fn schema(kind: ModuleKind, body: &str, bound: bool) -> Schema {
    let id: ModuleId = if kind == ModuleKind::View {
        "app.c"
    } else {
        "app.c()"
    }
    .parse()
    .unwrap();
    Schema {
        modules: [(
            id,
            Module {
                kind,
                definition: body.into(),
                description: None,
                options: pbps_model::ModuleOptions {
                    schemabinding: bound,
                    ..Default::default()
                },
            },
        )]
        .into(),
        ..Default::default()
    }
}

#[test]
fn desired_questions_cover_only_creation_bound_bare_from_and_join_names() {
    let mut scope = CaptureScope::default();
    seed(&mut scope, &[schema(ModuleKind::View, "SELECT * FROM [a]]b] JOIN v ON 1=1 JOIN other.w ON 1=1 JOIN db.dbo.z ON 1=1 -- FROM ignored\n", false)]).unwrap();
    let expected = ["app", "dbo"]
        .into_iter()
        .flat_map(|schema| {
            ["a]b", "v"].map(|name| CandidateSet {
                class: CandidateClass::Object,
                schema: schema.into(),
                name: name.into(),
            })
        })
        .collect();
    assert_eq!(scope.candidates, expected);
    for (kind, body, bound) in [
        (ModuleKind::View, "SELECT 'FROM v' AS s /* JOIN v */", false),
        (ModuleKind::View, "SELECT * FROM v", true),
        (ModuleKind::Procedure, "AS SELECT * FROM v", false),
        (
            ModuleKind::Function,
            "() RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM v); END",
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @r TABLE(i int) AS BEGIN SELECT * FROM v; RETURN; END",
            false,
        ),
    ] {
        let mut scope = CaptureScope::default();
        seed(&mut scope, &[schema(kind, body, bound)]).unwrap();
        assert!(scope.candidates.is_empty(), "{kind}: {body}");
    }
    let mut scope = CaptureScope::default();
    seed(
        &mut scope,
        &[schema(
            ModuleKind::Function,
            "() RETURNS TABLE AS RETURN (SELECT * FROM v)",
            false,
        )],
    )
    .unwrap();
    assert_eq!(scope.candidates.len(), 2);
}

fn observe(catalog: Catalog, scope: CaptureScope) -> CapturedInputs {
    let selected = select::select(&catalog, &scope, &producer()).unwrap();
    CapturedInputs::new(catalog, selected, scope, producer())
}

fn relay_catalog() -> Catalog {
    let mut catalog = catalog();
    catalog.objects.push(Object {
        id: 300,
        schema: 5,
        name: "relay".into(),
        kind: "V".into(),
        parent: None,
        owner: None,
    });
    catalog.modules.insert(
        300,
        CatalogModule {
            definition: Some("CREATE VIEW app.relay AS SELECT 1 AS i".into()),
            ansi_nulls: true,
            quoted_identifier: true,
            schema_bound: false,
            execute_as: None,
        },
    );
    catalog
        .objects_named
        .insert(("app".into(), "relay".into()), BTreeSet::from([300]));
    catalog.dependencies.push(Dependency {
        referencing: 200,
        referencing_column: 0,
        class: 1,
        server: None,
        database: None,
        schema: None,
        entity: "relay".into(),
        referenced: Some(300),
        referenced_column: 0,
        schema_bound: false,
        caller_dependent: false,
    });
    catalog
}

#[test]
fn expansion_returns_only_a_domain_and_the_final_pass_owns_fresh_properties() {
    let mut scope = CaptureScope::default();
    seed(
        &mut scope,
        &[schema(ModuleKind::View, "SELECT * FROM v", false)],
    )
    .unwrap();
    let partial = observe(relay_catalog(), scope);
    assert!(partial.objects().any(|id| *id == object("app", "v")));
    assert!(!partial.objects().any(|id| *id == object("app", "relay")));
    let ControlFlow::Continue(expanded) = advance(partial, &BTreeSet::new()) else {
        panic!("runtime candidates require a new full read")
    };
    assert!(expanded.candidates.iter().any(|set| set.name == "relay"));
    // Change an already-read object between passes: joining old inputs would
    // keep the old definition, although the relay itself is freshly captured.
    let mut fresh = relay_catalog();
    fresh.modules.get_mut(&200).unwrap().definition =
        Some("CREATE VIEW app.v AS SELECT 2 AS i".into());
    let ControlFlow::Break(complete) = advance(observe(fresh, expanded), &BTreeSet::new()) else {
        panic!("complete closure must settle")
    };
    assert_eq!(
        complete.inputs[&object("app", "v")].properties["definition"],
        serde_json::json!("CREATE VIEW app.v AS SELECT 2 AS i")
    );
    assert!(complete.objects().any(|id| *id == object("app", "relay")));
    assert!(
        !complete
            .bound_objects(&object("app", "v"))
            .unwrap()
            .any(|id| *id == object("app", "relay")),
        "runtime candidates must never become bindings"
    );
}

#[test]
fn absent_members_are_complete_but_hidden_candidate_definitions_refuse() {
    let mut scope = CaptureScope::default();
    seed(
        &mut scope,
        &[schema(ModuleKind::View, "SELECT * FROM missing", false)],
    )
    .unwrap();
    let complete = observe(catalog(), scope.clone());
    assert!(
        complete
            .members(scope.candidates.first().unwrap())
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        advance(complete, &BTreeSet::new()),
        ControlFlow::Break(_)
    ));
    let mut scope = CaptureScope::default();
    seed(
        &mut scope,
        &[schema(ModuleKind::View, "SELECT * FROM v", false)],
    )
    .unwrap();
    let mut hidden = catalog();
    hidden.modules.get_mut(&200).unwrap().definition = None;
    let refusal = match select::select(&hidden, &scope, &producer()) {
        Ok(_) => panic!("hidden is not absent"),
        Err(refusal) => refusal,
    };
    assert_eq!(refusal.object, Some(object("app", "v")));
    assert!(refusal.condition.contains("definition cannot be read"));
    let error = seed(
        &mut scope,
        &[schema(ModuleKind::View, "SELECT * FROM v /* secret", false)],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("c") && error.contains("cannot be lexed"),
        "{error}"
    );
    assert!(!error.contains("secret"));
}

#[test]
fn obsolete_managed_runtime_bodies_do_not_expand_the_desired_domain() {
    let scope = CaptureScope {
        retained: [object("app", "v")].into(),
        candidates: Default::default(),
    };
    let partial = observe(relay_catalog(), scope);
    let ControlFlow::Break(complete) = advance(partial, &[object("app", "v")].into()) else {
        panic!("an old managed body is not a desired name question")
    };
    assert!(complete.scope().candidates.is_empty());
    assert!(!complete.objects().any(|id| *id == object("app", "relay")));
}

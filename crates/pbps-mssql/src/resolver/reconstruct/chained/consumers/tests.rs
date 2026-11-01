use super::*;
use pbps_model::{Module, ModuleId, ModuleKind, PlannedChange};

fn module(kind: ModuleKind, name: &str, definition: &str, bound: bool) -> Draft {
    let id: ModuleId = if kind == ModuleKind::View {
        name.to_owned()
    } else {
        format!("{name}()")
    }
    .parse()
    .unwrap();
    Draft {
        operation: 7,
        metadata: Metadata {
            declaration: format!("{kind} {}", id.shown()),
            created: vec![object(id.schema(), id.name())],
            auxiliary: None,
        },
        change: PlannedChange::new(Change::CreateModule {
            id,
            module: Box::new(Module {
                kind,
                definition: definition.into(),
                description: None,
                options: pbps_model::ModuleOptions {
                    schemabinding: bound,
                    ..Default::default()
                },
            }),
        }),
    }
}

#[test]
fn only_creation_bound_module_bodies_consume_an_external() {
    let external = object("dbo", "v");
    for (kind, body, bound, expected) in [
        (ModuleKind::View, "SELECT i FROM v", false, true),
        (
            ModuleKind::Function,
            "(@x dbo.schemabinding) RETURNS TABLE AS RETURN (SELECT i FROM v)",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS TABLE RETURN (SELECT i FROM v)",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS int BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @t TABLE(i int) BEGIN INSERT @t SELECT i FROM dbo.v; RETURN; END",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM v); END",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @t TABLE(i int) WITH SCHEMABINDING AS BEGIN INSERT @t SELECT i FROM dbo.v; RETURN; END",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @t TABLE(i int) WITH SCHEMABINDING AS BEGIN INSERT @t SELECT i FROM v; RETURN; END",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS int WITH EXECUTE AS OWNER, SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @t TABLE(i int) WITH EXECUTE AS OWNER, SCHEMABINDING AS BEGIN INSERT @t SELECT i FROM dbo.v; RETURN; END",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS dbo.schemabinding AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
            false,
            false,
        ),
        (ModuleKind::View, "SELECT i FROM dbo.v", true, true),
        (ModuleKind::View, "SELECT i FROM v", true, false),
        (
            ModuleKind::Function,
            "() RETURNS TABLE AS RETURN (SELECT i FROM v)",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS TABLE WITH SCHEMABINDING AS RETURN (SELECT i FROM dbo.v)",
            false,
            true,
        ),
        (
            ModuleKind::Function,
            "() RETURNS TABLE WITH SCHEMABINDING AS RETURN (SELECT i FROM v)",
            false,
            false,
        ),
        (
            ModuleKind::Procedure,
            "AS SELECT i FROM dbo.v",
            false,
            false,
        ),
        (
            ModuleKind::Trigger,
            "AFTER INSERT AS SELECT i FROM dbo.v",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
            false,
            false,
        ),
        (
            ModuleKind::Function,
            "() RETURNS @t TABLE (i int) AS BEGIN INSERT @t SELECT i FROM dbo.v; RETURN; END",
            false,
            false,
        ),
    ] {
        assert_eq!(
            needs(&module(kind, "dbo.c", body, bound), &external, &[&external]).unwrap(),
            expected,
            "{kind}: {body}"
        );
    }
}

#[test]
fn bare_from_and_join_use_the_nearest_occupant_of_every_kind() {
    let fallback = object("dbo", "v");
    let own = object("fallback", "v");
    let unrelated = object("elsewhere", "v");
    for body in [
        "SELECT i FROM v",
        "SELECT a.i FROM dbo.a a JOIN [v] b ON a.i=b.i",
        "SELECT i FROM v()",
    ] {
        let step = module(ModuleKind::View, "fallback.c", body, false);
        assert!(
            needs(&step, &fallback, &[&fallback, &unrelated]).unwrap(),
            "{body}"
        );
        assert!(
            !needs(&step, &fallback, &[&fallback, &own, &unrelated]).unwrap(),
            "{body}"
        );
        assert!(
            needs(&step, &own, &[&fallback, &own, &unrelated]).unwrap(),
            "{body}"
        );
        assert!(
            !needs(&step, &unrelated, &[&fallback, &own, &unrelated]).unwrap(),
            "{body}"
        );
    }
    // A consumer is not a namespace occupant that exists before itself.
    let step = module(ModuleKind::View, "fallback.v", "SELECT i FROM v", false);
    assert!(needs(&step, &fallback, &[&fallback, &own]).unwrap());
}

#[test]
fn literals_comments_headers_aliases_and_cross_database_names_add_no_edge() {
    let external = object("dbo", "v");
    for body in [
        "SELECT 'FROM v JOIN dbo.v' AS s /* FROM v */ -- dbo.v\n",
        "SELECT 1 AS v FROM dbo.other",
        "SELECT v.i FROM dbo.other AS v",
        "SELECT i FROM other.v",
        "SELECT i FROM otherdb.dbo.v",
        "SELECT i FROM server.otherdb.dbo.v",
        "SELECT i FROM otherdb..v",
        "SELECT [FROM], [v] FROM dbo.other",
    ] {
        assert!(
            !needs(
                &module(ModuleKind::View, "dbo.v", body, false),
                &external,
                &[&external]
            )
            .unwrap(),
            "{body}"
        );
    }
    // RETURNS in a parameter name/comment and qualified header type do not
    // become body consumers. The body is still a qualified reference scan.
    let body = "(@returns dbo.v) RETURNS TABLE AS RETURN (SELECT 1 AS i)";
    assert!(
        !needs(
            &module(ModuleKind::Function, "dbo.f", body, false),
            &external,
            &[&external]
        )
        .unwrap()
    );
}

#[test]
fn quoted_names_and_layout_keep_the_existing_dialect_comparison() {
    for (name, body) in [
        ("a]b", "SELECT i FROM [dbo] /* gap */ . [a]]b]"),
        ("a\"b", "SELECT i FROM \"a\"\"b\""),
        ("v", "SELECT i FROM [V]"),
    ] {
        let external = object("dbo", name);
        assert!(
            needs(
                &module(ModuleKind::View, "dbo.c", body, false),
                &external,
                &[&external]
            )
            .unwrap(),
            "{body}"
        );
    }
}

#[test]
fn unreadable_source_and_unclassifiable_function_headers_refuse_by_name() {
    let external = object("dbo", "v");
    for (kind, body, reason) in [
        (
            ModuleKind::View,
            "SELECT i FROM v /* unfinished",
            "cannot be lexed",
        ),
        (ModuleKind::View, "SELECT 'unfinished", "cannot be lexed"),
        (ModuleKind::View, "SELECT i FROM [v", "cannot be lexed"),
        (ModuleKind::View, "SELECT i FROM \"v", "cannot be lexed"),
        (
            ModuleKind::Function,
            "() AS SELECT 1",
            "header cannot be classified",
        ),
        (
            ModuleKind::Function,
            "() RETURNS AS SELECT 1",
            "header cannot be classified",
        ),
        (
            ModuleKind::Function,
            "(@x int) RETURNS",
            "header cannot be classified",
        ),
    ] {
        let error = needs(
            &module(kind, "dbo.consumer2541", body, false),
            &external,
            &[&external],
        )
        .unwrap_err()
        .to_string();
        for text in ["consumer2541", "external", "dbo", "v", reason] {
            assert!(error.contains(text), "{error}");
        }
        assert!(!error.contains(body), "source must not leak: {error}");
    }
    assert!(ExternalInputs::statement_names("SELECT 1 -- an EOF comment is valid").is_ok());
    assert!(
        ExternalInputs::statement_names("SELECT 'a''b', [a]]b] /* outer /* inner */ */").is_ok()
    );
}

#[test]
fn cte_names_are_documented_over_inclusion_not_binding_evidence() {
    let external = object("dbo", "v");
    let step = module(
        ModuleKind::View,
        "dbo.c",
        "WITH v AS (SELECT 1 AS i) SELECT i FROM v",
        false,
    );
    assert!(needs(&step, &external, &[&external]).unwrap());
    let step = module(
        ModuleKind::View,
        "dbo.c",
        "WITH local AS (SELECT 1 AS i) SELECT v.i FROM local AS v",
        false,
    );
    assert!(!needs(&step, &external, &[&external]).unwrap());
}

#[test]
fn structural_foreign_keys_and_trigger_targets_survive_body_filtering() {
    let external = object("dbo", "v");
    let mut trigger = module(
        ModuleKind::Trigger,
        "dbo.tr",
        "AFTER INSERT AS SELECT 1",
        false,
    );
    let Change::CreateModule { id, .. } = &mut trigger.change.change else {
        unreachable!()
    };
    *id = "dbo.v.tr".parse().unwrap();
    assert!(needs(&trigger, &external, &[&external]).unwrap());
    assert!(!needs(&trigger, &object("other", "v"), &[&external]).unwrap());
    let mut fk = trigger;
    fk.change = PlannedChange::new(Change::AddForeignKey {
        table: pbps_model::TableName::new("dbo", "t"),
        name: "fk".into(),
        constraint: Box::new(pbps_model::ForeignKey {
            columns: vec!["i".into()],
            references_table: pbps_model::TableName::new("dbo", "v"),
            references_columns: vec!["i".into()],
            on_delete: pbps_model::ReferentialAction::NoAction,
            on_update: pbps_model::ReferentialAction::NoAction,
        }),
    });
    assert!(needs(&fk, &external, &[&external]).unwrap());
    assert!(!needs(&fk, &object("other", "v"), &[&external]).unwrap());
}

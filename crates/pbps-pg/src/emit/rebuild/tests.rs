use super::*;
use pbps_model::IndexChange;
use pbps_model::PartitionChange;

#[test]
fn physical_parts_keep_explicit_copy_analysis_and_owned_index_sql() {
    let pg = Postgres::new();
    let mut changes = fixture::changes();
    let analyze = changes.pop().unwrap();
    let mut copy = changes.pop().unwrap();
    let Change::Partition(PartitionChange::CopyTableRows(payload)) = &mut copy else {
        panic!()
    };
    payload.columns.push(pbps_model::CopyColumn {
        stages: Vec::new(),
        source: None,
        target: payload.target.name.column("new\"column"),
        cast: "text".parse().unwrap(),
        collation: Some(pbps_model::Collation::new("C")),
    });
    let recipe = fixture::index_recipe();
    let mut cases = vec![
        (copy, vec!["DO $pbps$\nDECLARE copied bigint;\nBEGIN\nINSERT INTO \"app\".\"tmp_p\" (\"k\", \"new\"\"column\") SELECT CAST(src.\"k\" AS bigint), CAST(NULL AS text) COLLATE \"C\" FROM \"app\".\"p\" AS src;\nGET DIAGNOSTICS copied = ROW_COUNT;\nIF copied > 10 THEN\nRAISE EXCEPTION USING MESSAGE = 'rebuild copy exceeds approved max_rows (10)', ERRCODE = 'P0001';\nEND IF;\nEND;\n$pbps$;".to_owned()]),
        (analyze, vec!["ANALYZE \"app\".\"tmp_p\";".into(), "ANALYZE \"app\".\"tmp_p1\";".into()]),
        (Change::Index(IndexChange::BuildPartitionedIndex(Box::new(recipe.clone()))), vec![
            "CREATE INDEX \"p_idx\" ON ONLY \"app\".\"tmp_p\" (\"k\" ASC);".into(),
            "CREATE INDEX \"p1_k_idx\" ON \"app\".\"tmp_p1\" (\"k\" ASC);".into(),
            "ALTER INDEX \"app\".\"p_idx\" ATTACH PARTITION \"app\".\"p1_k_idx\";".into(),
        ]),
    ];
    for (definition, kind) in [
        (
            serde_json::json!({"primary_key":{"name":"p_idx","columns":["k"]}}),
            "PRIMARY KEY",
        ),
        (serde_json::json!({"unique":{"columns":["k"]}}), "UNIQUE"),
    ] {
        let mut key = recipe.clone();
        key.definition = serde_json::from_value(definition).unwrap();
        let bodies = [
            format!("ALTER TABLE ONLY \"app\".\"tmp_p\" ADD CONSTRAINT \"p_idx\" {kind} (\"k\");"),
            "CREATE UNIQUE INDEX \"p1_k_idx\" ON \"app\".\"tmp_p1\" (\"k\" ASC);".into(),
            format!(
                "ALTER TABLE ONLY \"app\".\"tmp_p1\" ADD CONSTRAINT \"p1_k_idx\" {kind} USING INDEX \"p1_k_idx\";"
            ),
            "ALTER INDEX \"app\".\"p_idx\" ATTACH PARTITION \"app\".\"p1_k_idx\";".into(),
        ];
        cases.push((
            Change::Index(IndexChange::BuildPartitionedIndex(Box::new(key))),
            bodies.into(),
        ));
    }
    // An exclusion has no `USING INDEX` form: built on the parent alone, then whole on each
    // leaf under its clone's name with the parent's timing, parameters and predicate, and
    // attached (measured on 18.6, DEC-2448.7).
    let mut exclusion = recipe.clone();
    exclusion.definition = serde_json::from_value(serde_json::json!({"exclusion":{
        "method":"btree",
        "keys":[{"key":{"name":"k"},"operator":"="}],
        "include":["note"],
        "storage_parameters":{"fillfactor":"70"},
        "filter":"k > 0",
        "timing":"deferrable_immediate"
    }}))
    .unwrap();
    let clause = |name: &str| {
        format!(
            "CONSTRAINT \"{name}\" EXCLUDE USING btree (\"k\" ASC WITH OPERATOR(\"pg_catalog\".=)) \
             INCLUDE (\"note\") WITH (fillfactor = '70') WHERE (k > 0\n) DEFERRABLE INITIALLY IMMEDIATE"
        )
    };
    cases.push((
        Change::Index(IndexChange::BuildPartitionedIndex(Box::new(
            exclusion.clone(),
        ))),
        vec![
            format!(
                "ALTER TABLE ONLY \"app\".\"tmp_p\" ADD {};",
                clause("p_idx")
            ),
            format!("ALTER TABLE \"app\".\"tmp_p1\" ADD {};", clause("p1_k_idx")),
            "ALTER INDEX \"app\".\"p_idx\" ATTACH PARTITION \"app\".\"p1_k_idx\";".into(),
        ],
    ));
    for (change, bodies) in cases {
        let expected: Vec<_> = bodies
            .iter()
            .map(|s| format!("SET search_path = \"app\", \"pg_temp\";\n{s}\nSET search_path = '';"))
            .collect();
        let actual: Vec<_> = emit(&pg, &change, Strategy::default())
            .unwrap()
            .into_iter()
            .map(|s| s.sql)
            .collect();
        assert_eq!(actual, expected);
    }
    let mut invalid = recipe;
    invalid.clones.push(invalid.clones[0].clone());
    assert!(
        emit(
            &pg,
            &Change::Index(IndexChange::BuildPartitionedIndex(Box::new(invalid))),
            Strategy::default()
        )
        .is_err()
    );
    assert!(emit(&pg, &changes.remove(0), Strategy::default()).is_err());
}

#[test]
fn the_shared_emission_entry_refuses_unbalanced_rebuild_conversions() {
    let pg = Postgres::new();
    let mut copy = fixture::changes()
        .into_iter()
        .find(|c| matches!(c, Change::Partition(PartitionChange::CopyTableRows(_))))
        .unwrap();
    let Change::Partition(PartitionChange::CopyTableRows(payload)) = &mut copy else {
        unreachable!()
    };
    let column = &mut payload.columns[0];
    let input = column.source.as_ref().unwrap().column.name.clone();
    let expression_name = quote(input.raw()).unwrap();
    column.stages.push(pbps_model::CopyStage {
        input: input.clone(),
        cast: column.cast.clone(),
        collation: None,
        using: Some(format!("{expression_name}) IS NULL OR ({expression_name}")),
    });
    let why = emit(&pg, &copy, Strategy::default())
        .unwrap_err()
        .to_string();
    assert!(
        why.contains("has unbalanced parentheses") && why.contains("re-record"),
        "{why}"
    );
    let Change::Partition(PartitionChange::CopyTableRows(payload)) = &mut copy else {
        unreachable!()
    };
    payload.columns[0].stages[0].using = Some(format!("({expression_name})"));
    assert!(emit(&pg, &copy, Strategy::default()).is_ok());
}

#[test]
fn the_copy_names_the_columns_it_lists_so_the_guard_judges_the_defaults_of_the_rest() {
    let pg = Postgres::new();
    let mut changes = fixture::changes();
    let _analyze = changes.pop().unwrap();
    let copy = changes.pop().unwrap();
    let statements = emit(&pg, &copy, Strategy::default()).unwrap();
    let write = statements[0]
        .rewrites
        .as_ref()
        .expect("the copy is a row write");
    let listed: std::collections::BTreeSet<_> = [pbps_model::ColumnName::new("k")].into();
    assert_eq!(
        write.operation,
        pbps_dialect::RowOperation::Copy { columns: listed }
    );
}

/// A recipe builds its parent definition with the ordinary statement over the copied rows,
/// so the routines its key expressions and predicate call are judged as the same definition's
/// ordinary change is, for every definition a recipe carries. A definition of plain columns
/// carries nothing, and no probe evaluates a recipe (the preflight skips it).
#[test]
fn a_rebuild_recipe_carries_the_expressions_its_definition_holds() {
    let recipe = fixture::index_recipe();
    let with = |definition: serde_json::Value| {
        let mut r = recipe.clone();
        r.definition = serde_json::from_value(definition).unwrap();
        Change::Index(IndexChange::BuildPartitionedIndex(Box::new(r)))
    };
    let exclusion = |key: serde_json::Value, filter: Option<&str>| {
        with(serde_json::json!({"exclusion":{
            "method":"btree",
            "keys":[{"key":key,"operator":"="}],
            "include":[],
            "storage_parameters":{},
            "filter":filter,
            "timing":"immediate"
        }}))
    };
    let call = serde_json::json!({"expression":"app.ok(k)"});
    let column = serde_json::json!({"name":"k"});
    // Positive: an exclusion's key expression, and its predicate.
    assert!(crate::table_expressions::carries(&exclusion(
        call.clone(),
        None
    )));
    assert!(crate::table_expressions::carries(&exclusion(
        column.clone(),
        Some("app.ok(k)")
    )));
    // Positive: a plain index recipe holds the same gap, closed in the same arm.
    assert!(crate::table_expressions::carries(&with(
        serde_json::json!({"index":{"columns":[call]}})
    )));
    assert!(crate::table_expressions::carries(&with(
        serde_json::json!({"index":{"columns":[column.clone()],"filter":"app.ok(k)"}})
    )));
    // Negative: columns only, no predicate, in each definition.
    assert!(!crate::table_expressions::carries(&exclusion(
        column.clone(),
        None
    )));
    assert!(!crate::table_expressions::carries(&Change::Index(
        IndexChange::BuildPartitionedIndex(Box::new(recipe.clone()))
    )));
    for key in [
        serde_json::json!({"unique":{"columns":["k"]}}),
        serde_json::json!({"primary_key":{"name":"p_idx","columns":["k"]}}),
    ] {
        assert!(!crate::table_expressions::carries(&with(key)));
    }
    // No probe evaluates a recipe, so leaving one unjudged leaves the probes running.
    assert!(!crate::table_expressions::probed(&exclusion(call, None)));
}

/// The first statement of a recipe that holds an expression carries the recipe, so the executor
/// judges it before the ADD runs over the copied rows; a recipe of plain columns carries none.
#[test]
fn the_first_statement_of_a_recipe_with_an_expression_introduces_it() {
    let pg = Postgres::new();
    let recipe = fixture::index_recipe();
    let build = |keys: serde_json::Value| {
        let mut r = recipe.clone();
        r.definition = serde_json::from_value(serde_json::json!({"exclusion":{
            "method":"btree",
            "keys":[{"key":keys,"operator":"="}],
            "include":[],
            "storage_parameters":{},
            "timing":"immediate"
        }}))
        .unwrap();
        emit(
            &pg,
            &Change::Index(IndexChange::BuildPartitionedIndex(Box::new(r))),
            Strategy::default(),
        )
        .unwrap()
    };
    let held = build(serde_json::json!({"expression":"app.ok(k)"}));
    assert!(held[0].introduces.is_some(), "{held:?}");
    assert!(held[1..].iter().all(|s| s.introduces.is_none()));
    let plain = build(serde_json::json!({"name":"k"}));
    assert!(plain.iter().all(|s| s.introduces.is_none()), "{plain:?}");
}

/// A leaf may live in another schema than its parent, and its build evaluates the parent's
/// declared key expressions and predicate, which the apply-time vet judged as bound on the
/// parent's `search_path`. So every statement of a recipe, a leaf's included, sets the
/// parent's path and never the leaf's: an unqualified call on the leaf's path would bind to a
/// routine of the leaf's schema that nothing admitted (measured on 18.6: the decoy ran as
/// the deployer, then the ATTACH refused the mismatched index). The leaf stays named in its
/// own statements by the qualified table name, and the declared text is carried verbatim.
#[test]
fn every_recipe_statement_runs_on_the_parents_path_even_for_a_cross_schema_leaf() {
    let pg = Postgres::new();
    let recipe = fixture::index_recipe();
    assert_eq!(recipe.parent.name.schema, "app");
    let exclusion = |key: serde_json::Value, filter: Option<&str>| {
        serde_json::json!({"exclusion":{
            "method":"btree",
            "keys":[{"key":key,"operator":"="}],
            "include":[],
            "storage_parameters":{},
            "filter":filter,
            "timing":"immediate"
        }})
    };
    let call = serde_json::json!({"expression":"ok(k)"});
    let column = serde_json::json!({"name":"k"});
    let definitions = [
        // The expression arms: a key call, and a predicate call, in each definition.
        exclusion(call.clone(), None),
        exclusion(column.clone(), Some("ok(k)")),
        serde_json::json!({"index":{"columns":[call.clone()]}}),
        serde_json::json!({"index":{"columns":[column.clone()],"filter":"ok(k)"}}),
        // Negatives: the arms that hold no expression run on the parent's path as well, so
        // the rule has no exception to remember.
        exclusion(column, None),
        serde_json::json!({"index":{"columns":[{"name":"k"}]}}),
        serde_json::json!({"unique":{"columns":["k"]}}),
        serde_json::json!({"primary_key":{"name":"p_idx","columns":["k"]}}),
    ];
    for definition in definitions {
        for leaf_schema in ["leaf_schema", "app"] {
            let mut r = recipe.clone();
            r.definition = serde_json::from_value(definition.clone()).unwrap();
            r.clones[0].leaf.name.schema = leaf_schema.into();
            let statements = emit(
                &pg,
                &Change::Index(IndexChange::BuildPartitionedIndex(Box::new(r))),
                Strategy::default(),
            )
            .unwrap();
            assert!(statements.len() >= 3, "{definition}: {statements:?}");
            for s in &statements {
                assert!(
                    s.sql
                        .starts_with("SET search_path = \"app\", \"pg_temp\";\n"),
                    "{definition} leaf in {leaf_schema}: {}",
                    s.sql
                );
                assert!(
                    leaf_schema == "app" || !s.sql.contains("\"leaf_schema\", \"pg_temp\""),
                    "{definition}: {}",
                    s.sql
                );
                let commands = &s.execution.as_ref().unwrap().commands;
                assert_eq!(commands[0], "SET search_path = \"app\", \"pg_temp\";");
            }
            // The leaf is still the table built on, by its qualified name.
            let leaf = format!("\"{leaf_schema}\".\"tmp_p1\"");
            assert!(
                statements[1].sql.contains(&leaf),
                "{definition}: {}",
                statements[1].sql
            );
            // The declared text reaches the leaf's statement unchanged, a qualified call too.
            if definition.to_string().contains("ok(k)") {
                assert!(statements[1].sql.contains("ok(k)"), "{}", statements[1].sql);
            }
        }
    }
}

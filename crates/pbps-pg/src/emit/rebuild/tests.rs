use super::*;

#[test]
fn physical_parts_keep_explicit_copy_analysis_and_owned_index_sql() {
    let pg = Postgres::new();
    let mut changes = fixture::changes();
    let analyze = changes.pop().unwrap();
    let mut copy = changes.pop().unwrap();
    let Change::CopyTableRows(payload) = &mut copy else {
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
        (Change::BuildPartitionedIndex(Box::new(recipe.clone())), vec![
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
        cases.push((Change::BuildPartitionedIndex(Box::new(key)), bodies.into()));
    }
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
            &Change::BuildPartitionedIndex(Box::new(invalid)),
            Strategy::default()
        )
        .is_err()
    );
    assert!(emit(&pg, &changes.remove(0), Strategy::default()).is_err());
}

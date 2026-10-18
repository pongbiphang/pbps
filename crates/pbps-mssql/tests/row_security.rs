use pbps_dialect::Dialect;
use pbps_model::{Column, RowPolicy, Table, TableName};

#[test]
fn row_security_defaults_are_accepted_but_postgres_security_is_refused() {
    let mssql = pbps_mssql::Mssql;
    let name = TableName::new("dbo", "t");
    let plain = Table {
        columns: [("id".into(), Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    assert!(mssql.validate_table(&name, &plain).is_empty());
    for shape in 0..3 {
        let mut t = plain.clone();
        let key = match shape {
            0 => {
                t.row_security.enabled = true;
                "row_security"
            }
            1 => {
                t.row_security.forced = true;
                "row_security"
            }
            _ => {
                t.row_policies.insert("p".into(), RowPolicy::default());
                "row_policies"
            }
        };
        assert!(
            mssql
                .validate_table(&name, &t)
                .iter()
                .any(|e| e.to_string().contains(key))
        );
    }
}

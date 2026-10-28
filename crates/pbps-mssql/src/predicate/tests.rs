use super::*;
use pbps_dialect::predicate::{self, Capabilities, Parsed, Vetted};
use pbps_model::ColumnName;

fn parse(sql: &str) -> Result<Parsed<Mssql>> {
    predicate::parse(sql, Mssql)
}
fn caps(major: u16, compatibility: u16) -> Capabilities {
    Capabilities {
        major,
        compatibility,
    }
}
fn vet(sql: &str, major: u16, compatibility: u16) -> Result<Vetted<Mssql>> {
    parse(sql)?.vet(
        |n| {
            [
                "amount", "s", "path", "odd]name", r#"a"b"#, "@local", "select", "é",
            ]
            .into_iter()
            .find(|v| *v == n.raw())
            .map(ColumnName::new)
        },
        caps(major, compatibility),
    )
}
fn roundtrip(sql: &str) -> String {
    let ast = vet(sql, 17, 170).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let sql = ast.sql().unwrap();
    let again = vet(&sql, 17, 170).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(ast.parsed(), again.parsed(), "{sql}");
    sql
}
fn refused(sql: &str) -> String {
    let error = match vet(sql, 17, 170) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("admitted {sql}"),
    };
    assert!(
        error.contains("plan-time assertion (#2401, planned)"),
        "{error}"
    );
    assert!(
        error.contains("CHECK constraint for the transition"),
        "{error}"
    );
    error
}

#[test]
fn complete_input_is_required_even_after_a_valid_condition() {
    for sql in [
        "1=1); DROP TABLE x; --",
        "1=1;",
        "1=1 GO",
        "1=1 2=2",
        "1=1 /*",
        "1=1 -- harmless\nDROP TABLE x",
    ] {
        refused(sql);
    }
    assert_eq!(
        roundtrip("1=1 -- harmless\n /* outer /* inner */ end */"),
        "(1 = 1)"
    );
}
#[test]
fn only_bare_target_columns_and_allowlisted_calls_are_representable() {
    for sql in [
        "dbo.t.amount=1",
        "t.amount=1",
        "other=1",
        "[dbo].[amount]=1",
        "EXISTS(SELECT 1)",
        "amount IN (SELECT amount FROM t)",
        "(SELECT 1)=1",
        "dbo.ABS(amount)=1",
        "[ABS](amount)=1",
        "GETDATE() IS NULL",
        "RAND()=0",
        "NEWID() IS NULL",
        "SUM(amount)=1",
        "COUNT(*)=1",
        "ABS(amount) OVER ()=1",
        "CURRENT_TIMESTAMP IS NULL",
        "@x=1",
        "@@SPID=1",
        "NEXT VALUE FOR dbo.s=1",
        "amount COLLATE x='a'",
        "amount::int=1",
    ] {
        refused(sql);
    }
    roundtrip("ABS(amount)=1 AND [@local] IS NULL AND [select]=1");
    let bound = parse("AMOUNT=1")
        .unwrap()
        .vet(
            |n| {
                n.raw()
                    .eq_ignore_ascii_case("amount")
                    .then(|| ColumnName::new("amount"))
            },
            caps(17, 170),
        )
        .unwrap();
    assert_eq!(bound.sql().unwrap(), "([amount] = 1)");
    assert!(
        parse("AMOUNT=1")
            .unwrap()
            .vet(|_| None, caps(17, 170))
            .is_err()
    );
}
#[test]
fn plain_and_national_strings_and_delimited_names_keep_their_identity() {
    let sql = "[odd]]name] = N'x''; DROP TABLE t; --' OR \"a\"\"b\"='α' OR é=N'α'";
    let emitted = roundtrip(sql);
    assert!(emitted.contains("[odd]]name]"));
    assert!(emitted.contains("[a\"b]"));
    assert!(emitted.contains("'α'"));
    assert!(emitted.contains("N'α'"));
    for sql in [
        "s='unterminated",
        "[unfinished=1",
        "\"unfinished=1",
        "s='x' 'y'",
        "s=N 'x'",
        "s=E'x'",
        "s='x\0y'",
    ] {
        refused(sql);
    }
}
#[test]
fn narrowed_grammar_refuses_binary_bitwise_and_non_conditions() {
    for sql in [
        "0xAB=0xAB",
        "0X=0X",
        "1&1=1",
        "1|1=1",
        "1^1=0",
        "~1=0",
        "1<<1=2",
        "amount",
        "CAST(1 AS bit)",
        "1 AND 1",
        "1=1=1",
        "amount IS TRUE",
        "amount IN ()",
        "amount IN (1,)",
        "amount BETWEEN 1 OR 2",
        "s LIKE 'a' ESCAPE",
        "1e+=1",
        "1.2.3=1",
        "",
        "-- empty",
    ] {
        refused(sql);
    }
}
#[test]
fn precedence_and_both_case_forms_roundtrip_without_changing_grouping() {
    for sql in [
        "1=1 OR 0=1 AND NOT 2=3",
        "NOT amount IN (1,2)",
        "amount NOT BETWEEN 1+2 AND 4*5 AND s NOT LIKE 'a!_%' ESCAPE '!'",
        "(amount+2)*3%4 !< -5",
        "amount != 1 AND amount !> 2",
        "CASE amount WHEN 1 THEN 2 WHEN 3 THEN 4 ELSE 5 END=5",
        "CASE WHEN amount IS NULL THEN 0 WHEN amount>1 THEN amount ELSE -1 END=0",
        "(CASE WHEN 1=1 THEN CASE 1 WHEN 1 THEN 2 END END)=2",
    ] {
        roundtrip(sql);
    }
    assert_eq!(roundtrip("1+2*3=7"), "((1 + (2 * 3)) = 7)");
    assert_eq!(roundtrip("-1*2=-2"), "((- (1 * 2)) = (- 2))");
    assert_eq!(
        roundtrip("NOT 1=2 AND 3=3 OR 4=5"),
        "(((NOT (1 = 2)) AND (3 = 3)) OR (4 = 5))"
    );
    assert_eq!(
        roundtrip("1 !< 2 OR 1 !> 2 OR 1 != 2"),
        "(((1 >= 2) OR (1 <= 2)) OR (1 <> 2))"
    );
    for sql in [
        "CASE END=1",
        "CASE WHEN 1 THEN 2 END=2",
        "CASE 1 WHEN 1=1 THEN 2 END=2",
        "CASE WHEN 1=1 THEN 2=2 END=2",
        "CASE WHEN 1=1 THEN 2",
        "CASE WHEN 1=1 THEN other ELSE 0 END=0",
    ] {
        refused(sql);
    }
}
#[test]
fn every_function_has_its_declared_arity_and_special_syntax() {
    for f in FUNCTIONS {
        if f.form == Form::Ordinary {
            let too_few = format!(
                "{}({}) IS NULL",
                f.name,
                vec!["NULL"; f.arity.0 - 1].join(",")
            );
            refused(&too_few);
            if f.arity.1 < 255 {
                let too_many = format!(
                    "{}({}) IS NULL",
                    f.name,
                    vec!["NULL"; f.arity.1 + 1].join(",")
                );
                refused(&too_many);
            }
        }
    }
    for sql in [
        "CAST(amount,int)=1",
        "CAST(amount AS int,1)=1",
        "CONVERT(int)=1",
        "CONVERT(int,amount,amount)=1",
        "CONVERT(int,amount,1.0)=1",
        "CONVERT(int,amount,2147483648)=1",
        "DATEADD('day',1,'20260101') IS NULL",
        "DATEADD(unknown,1,'20260101') IS NULL",
        "DATEADD(tzoffset,1,'20260101') IS NULL",
        "DATEDIFF(iso_week,'20260101','20260102')=1",
        "DATEPART(day)=1",
        "DATEPART(day,'20260101',1)=1",
        "TRIM()=''",
        "TRIM('a','b')=''",
        "TRIM(LEADING s)=''",
        "TRIM(TRAILING FROM s)=''",
        "TRIM(BOTH s)=''",
        "COALESCE(amount)=1",
    ] {
        refused(sql);
    }
    for sql in [
        "CONVERT(int,amount,-1)=1",
        "CAST(amount AS DECIMAL(18,2))=1",
        "DATEPART(yy,'20260101')=2026",
        "TRIM(LEADING 'x' FROM s)=''",
        "TRIM(BOTH ' ' FROM s)=''",
        "TRIM('x' FROM s)=''",
        "ROUND(amount,1,1)=0",
        "CHARINDEX('a',s,2)=0",
    ] {
        roundtrip(sql);
    }
}
#[test]
fn cast_targets_reuse_builtin_parameter_validation_and_cast_length_defaults() {
    for sql in [
        "CAST(s AS varchar(0)) IS NULL",
        "CAST(s AS nvarchar(4001)) IS NULL",
        "CAST(s AS int(1))=1",
        "CAST(amount AS decimal(2,3))=1",
        "CAST(s AS char(max)) IS NULL",
        "CAST(s AS datetime2(8)) IS NULL",
        "CAST(s AS float(54)) IS NULL",
        "CAST(s AS xml(1)) IS NULL",
        "CAST(s AS dbo.mytype) IS NULL",
        "CAST(s AS [int]) IS NULL",
        "CAST(s AS hierarchyid) IS NULL",
        "CAST(s AS geography) IS NULL",
        "CAST(s AS geometry) IS NULL",
        "CAST(s AS rowversion) IS NULL",
        "CAST(s AS text) IS NULL",
        "CAST(s AS json) IS NULL",
        "CAST(s AS vector(3)) IS NULL",
        "CAST(s AS varchar(length)) IS NULL",
    ] {
        refused(sql);
    }
    assert_eq!(
        roundtrip("CAST(s AS varchar) IS NULL"),
        "(CAST([s] AS VARCHAR(30)) IS NULL)"
    );
    assert_eq!(
        roundtrip("CONVERT(nvarchar,s) IS NULL"),
        "(CONVERT(NVARCHAR(30), [s]) IS NULL)"
    );
    assert_eq!(
        roundtrip("CAST(s AS sysname) IS NULL"),
        "(CAST([s] AS NVARCHAR(128)) IS NULL)"
    );
    for name in [
        "bit",
        "tinyint",
        "smallint",
        "int",
        "integer",
        "bigint",
        "decimal(18,2)",
        "numeric(18,2)",
        "real",
        "float(24)",
        "float(53)",
        "money",
        "smallmoney",
        "date",
        "time(7)",
        "datetime",
        "smalldatetime",
        "datetime2",
        "datetimeoffset",
        "char",
        "national character varying(30)",
        "binary",
        "varbinary(max)",
        "uniqueidentifier",
        "xml",
        "sql_variant",
    ] {
        roundtrip(&format!("CAST(s AS {name}) IS NULL"));
    }
}
#[test]
fn engine_and_compatibility_matrix_gates_every_newer_shape() {
    for (sql, major, compatibility) in [
        ("CONCAT(s,s) IS NULL", 11, 100),
        ("DATEFROMPARTS(2026,1,1) IS NULL", 11, 100),
        ("JSON_VALUE(s,'$.x') IS NULL", 13, 100),
        ("JSON_VALUE(s,path) IS NULL", 14, 100),
        ("TRIM(s) IS NULL", 14, 100),
        ("TRIM('x' FROM s) IS NULL", 14, 100),
        ("TRIM(LEADING ' ' FROM s) IS NULL", 16, 160),
        ("TRIM(TRAILING ' ' FROM s) IS NULL", 16, 160),
        ("TRIM(BOTH ' ' FROM s) IS NULL", 16, 160),
        ("LTRIM(s,'x') IS NULL", 16, 160),
        ("RTRIM(s,'x') IS NULL", 16, 160),
        ("SUBSTRING(s,1) IS NULL", 17, 100),
    ] {
        for version in 10..=17 {
            for level in [100, 110, 120, 130, 140, 150, 160, 170] {
                assert_eq!(
                    vet(sql, version, level).is_ok(),
                    version >= major && level >= compatibility,
                    "{sql}, {version}/{level}"
                );
            }
        }
    }
    for sql in [
        "CAST(amount AS int)=1",
        "CONVERT(int,amount)=1",
        "COALESCE(amount,0)=0",
        "ISNULL(amount,0)=0",
        "NULLIF(amount,0)=1",
        "LEFT(s,1)='a'",
        "RIGHT(s,1)='a'",
        "SUBSTRING(s,1,1)=''",
        "CHARINDEX('a',s)=1",
        "REPLACE(s,'a','b')='b'",
        "LOWER(s)='a'",
        "UPPER(s)='A'",
        "LTRIM(s)=''",
        "RTRIM(s)=''",
        "LEN(s)=1",
        "ROUND(amount,0)=1",
        "FLOOR(amount)=1",
        "CEILING(amount)=1",
        "ABS(amount)=1",
        "DATEADD(day,1,'20260101') IS NULL",
        "DATEDIFF(day,'20260101','20260102')=1",
        "DATEPART(day,'20260101')=1",
    ] {
        assert!(vet(sql, 10, 100).is_ok(), "{sql}");
    }
    assert!(vet("1=1", 0, 170).is_err());
    assert!(vet("1=1", 17, 0).is_err());
    // Unreachable arms still need their own version and scope proof.
    assert!(vet("CASE WHEN 1=1 THEN 1 ELSE LEN(TRIM(s)) END=1", 13, 130).is_err());
    refused("CASE WHEN 1=1 THEN 1 ELSE other END=1");
}
#[test]
fn bounded_generated_expressions_roundtrip_with_native_literal_types_preserved() {
    for token in ["0", "123", ".5", "1.", "1.20", "1e+2", "1E-2"] {
        roundtrip(&format!("({token})=1"));
    }
    for token in ["1e", "1.2.3", "1d", "0x1"] {
        refused(&format!("({token})=1"));
    }
    let atoms = [
        "amount",
        "NULL",
        "1",
        "1.00",
        ".50",
        "1E+2",
        "'text'",
        "N'α''x'",
        "CAST(amount AS decimal(18,2))",
        "COALESCE(amount,0)",
    ];
    for (i, a) in atoms.iter().enumerate() {
        for (j, b) in atoms.iter().enumerate() {
            for cmp in ["=", "<>", "<=", ">="] {
                roundtrip(&format!(
                    "({a}) {cmp} ({b}) OR NOT ({a} IS NULL AND {b} IS NOT NULL)"
                ));
            }
            if i < 6 && j < 6 {
                for op in ["+", "-", "*", "/", "%"] {
                    roundtrip(&format!("({a}{op}{b}) BETWEEN -1 AND 10"));
                }
            }
        }
    }
}
#[test]
fn resource_limits_bound_flat_trees_parentheses_and_case_nesting() {
    let mut case = "1".to_owned();
    for _ in 0..10 {
        case = format!("CASE WHEN 1=1 THEN {case} ELSE 0 END");
    }
    roundtrip(&format!("{case}=1"));
    refused(&format!("CASE WHEN 1=1 THEN {case} END=1"));
    refused(&format!("{}1=1{}", "(".repeat(260), ")".repeat(260)));
    refused(&vec!["1=1"; 130].join(" OR "));
    refused(&" ".repeat(65537));
    refused(&format!("s='{}'", "x".repeat(65536)));
    for count in [30, 60, 100, 120] {
        roundtrip(&vec!["1=1"; count].join(" OR "));
        roundtrip(&format!("{} 1=1", vec!["NOT"; count].join(" ")));
    }
}

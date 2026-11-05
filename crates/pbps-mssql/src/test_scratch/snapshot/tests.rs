use super::*;

#[test]
fn every_value_type_is_read_as_text_by_a_conversion_that_does_not_lose_it() {
    let c = "[c]";
    // The default conversion keeps 15 digits of a float, formats a date by the
    // session's language and turns binary into characters, so these are styled.
    for (base, style) in [
        ("float", ", 3)"),
        ("real", ", 3)"),
        ("money", ", 2)"),
        ("smallmoney", ", 2)"),
        ("date", ", 23)"),
        ("datetime", ", 121)"),
        ("datetime2", ", 121)"),
        ("smalldatetime", ", 121)"),
        ("time", ", 121)"),
        ("datetimeoffset", ", 121)"),
    ] {
        let text = cell(c, base).expect(base);
        assert!(text.ends_with(style), "{base}: {text}");
    }
    for base in [
        "binary",
        "varbinary",
        "image",
        "hierarchyid",
        "geometry",
        "geography",
    ] {
        let text = cell(c, base).expect(base);
        assert!(
            text.contains("varbinary(max)") && text.ends_with(", 1)"),
            "{base}: {text}"
        );
    }
    for base in [
        "bit",
        "tinyint",
        "smallint",
        "int",
        "bigint",
        "decimal",
        "numeric",
        "char",
        "varchar",
        "nchar",
        "nvarchar",
        "text",
        "ntext",
        "xml",
        "uniqueidentifier",
    ] {
        assert!(cell(c, base).expect(base).contains(c), "{base}");
    }
}

#[test]
fn a_value_that_differs_between_any_two_databases_has_no_text() {
    // Negative: a rowversion, a variant and a type this reader does not know are
    // refused, never read through the nearest conversion.
    for base in [
        "timestamp",
        "rowversion",
        "sql_variant",
        "",
        "money2",
        "Int",
    ] {
        assert!(cell("[c]", base).is_none(), "{base}");
    }
}

#[test]
fn a_hostile_name_is_quoted_not_spliced() {
    assert_eq!(qualified("a]b", "c d").unwrap(), "[a]]b].[c d]");
    // Negative: names the server cannot hold are refused before any SQL is built.
    assert!(qualified("", "t").is_err());
    assert!(qualified("s", "a\0b").is_err());
}

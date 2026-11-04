//! Every Markdown table keeps authored values inside their original cells.
use pbps_docs::markdown::render;
use pbps_model::{
    Column, ColumnType, ComputedColumn, Extension, IdsFile, ManagedType, Permission, Role, Schema,
    Table, TableName, Tombstone, TypeRef,
};

fn documented(value: &str) -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    let mut table = Table::default();
    let mut column = Column::new(ColumnType::reference(TypeRef::new("app", value).unwrap()));
    column.description = Some(value.into());
    column.deprecated = Some(value.into());
    column.value_source = pbps_model::ValueSource::Default(value.into());
    table.columns.insert(value.into(), column);
    table.computed.insert(
        value.into(),
        ComputedColumn {
            expression: value.into(),
            persisted: true,
            not_null: false,
        },
    );
    schema
        .tables
        .insert(TableName::new("app", "customer"), table);
    schema.extensions.insert(
        value.into(),
        Extension {
            schema: value.into(),
            version: value.into(),
        },
    );
    schema.types.insert(
        "app.domain_value".parse().unwrap(),
        ManagedType::Domain {
            base: ColumnType::reference(TypeRef::new("app", value).unwrap()),
            not_null: false,
            default: Some(value.into()),
            checks: [(value.into(), value.into())].into(),
            collation: Some(
                format!("app.\"{}\"", value.replace('"', "\"\""))
                    .parse()
                    .unwrap(),
            ),
        },
    );
    schema.types.insert(
        "app.enum_value".parse().unwrap(),
        ManagedType::Enum {
            values: vec![value.into()],
        },
    );
    schema.roles.insert(
        "reporter".into(),
        Role {
            grants: [(
                format!("app.{value}").parse().unwrap(),
                [Permission::Select].into(),
            )]
            .into(),
            ..Role::default()
        },
    );
    let mut ids = IdsFile::default();
    ids.tombstones.insert(
        "c_k7x2mq".parse().unwrap(),
        Tombstone {
            was: value.into(),
            dropped_at: value.into(),
            operator: value.into(),
            reason: value.into(),
            require: None,
        },
    );
    (schema, ids)
}

fn document(value: &str) -> String {
    let (schema, ids) = documented(value);
    render(&schema, &ids, "Schema")
}

const SPECIAL: &str = "a|`b\r\nnext";

#[test]
fn column_values_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``a\\|`b next`` ~~deprecated~~ | ``app.\"a\\|`b next\"`` | yes | ``a\\|`b next`` | a\\|`b next |\n"),
        "{out}"
    );
}

#[test]
fn computed_values_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``a\\|`b next`` | ``a\\|`b next`` | persisted |\n"),
        "{out}"
    );
}

#[test]
fn extension_values_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``a\\|`b next`` | ``a\\|`b next`` | ``a\\|`b next`` |\n"),
        "{out}"
    );
}

#[test]
fn enum_labels_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(out.contains("| ``a\\|`b next`` |\n"), "{out}");
}

#[test]
fn domain_properties_stay_inside_their_cells() {
    let out = document(SPECIAL);
    for row in [
        "| Base type | ``app.\"a\\|`b next\"`` |\n",
        "| Default | ``a\\|`b next`` |\n",
        "| Collation | ``app.\"a\\|`b next\"`` |\n",
    ] {
        assert!(out.contains(row), "missing {row:?}: {out}");
    }
}

#[test]
fn domain_checks_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``a\\|`b next`` | ``a\\|`b next`` |\n"),
        "{out}"
    );
}

#[test]
fn every_grant_target_representation_stays_inside_its_cell() {
    let mut schema = Schema::default();
    let targets = [
        ("dbo.sales|report", "`dbo.sales\\|report`"),
        ("app.a|`b\r\nnext", "``app.a\\|`b next``"),
        ("schema::a|`b\nnext", "``schema::a\\|`b next``"),
        ("app.a|`b\rnext(integer)", "``app.a\\|`b next(integer)``"),
        ("sequence::app.a|`b.id", "``sequence::app.a\\|`b.id``"),
        ("app.報表", "`app.報表`"),
    ];
    schema.roles.insert(
        "reader".into(),
        Role {
            grants: targets
                .iter()
                .map(|(target, _)| (target.parse().unwrap(), [Permission::Select].into()))
                .collect(),
            ..Role::default()
        },
    );
    let out = render(&schema, &IdsFile::default(), "Schema");
    for (_, expected) in targets {
        assert!(out.contains(&format!("| {expected} | select |\n")), "{out}");
    }
}

#[test]
fn deprecated_names_and_reasons_stay_inside_their_cells() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``app.customer.a\\|`b next`` | a\\|`b next |\n"),
        "{out}"
    );
}

#[test]
fn every_graveyard_field_stays_inside_its_cell() {
    let out = document(SPECIAL);
    assert!(
        out.contains("| ``a\\|`b next`` | a\\|`b next | a\\|`b next | a\\|`b next |\n"),
        "{out}"
    );
}

#[test]
fn all_nine_tables_keep_their_row_and_column_counts() {
    let out = document(SPECIAL);
    let headers = [
        "| Column | Type | Null | Default | Description |",
        "| Computed column | Expression | Stored |",
        "| Name | Schema | Exact version |",
        "| Property | Value |",
        "| Check | Expression |",
        "| Label |",
        "| Granted on | Permissions |",
        "| Column | Reason |",
        "| Was | Dropped | By | Reason |",
    ];
    let mut width = 0;
    let mut rows = 0;
    let mut tables = 0;
    for line in out.lines().filter(|line| line.starts_with('|')) {
        if headers.contains(&line) {
            width = line.matches('|').count();
            tables += 1;
        }
        assert_eq!(
            line.replace("\\|", "").matches('|').count(),
            width,
            "{line}"
        );
        rows += 1;
    }
    assert_eq!(tables, 9, "{out}");
    assert_eq!(rows, 30, "{out}");
}

#[test]
fn code_cells_cover_fences_line_endings_backslashes_and_unicode() {
    for (value, expected) in [
        ("a|b", "`a\\|b`"),
        ("x`y", "``x`y``"),
        ("`edge`", "`` `edge` ``"),
        ("two``ticks", "```two``ticks```"),
        ("LF\nnext", "`LF next`"),
        ("CR\rnext", "`CR next`"),
        ("CRLF\r\nnext", "`CRLF next`"),
        ("row\n\n|fake|", "`row  \\|fake\\|`"),
        ("a\\|b", "`a\\\\|b`"),
        ("名|`字", "``名\\|`字``"),
        ("</td><tr>", "`</td><tr>`"),
    ] {
        let mut schema = Schema::default();
        schema.types.insert(
            "app.labels".parse().unwrap(),
            ManagedType::Enum {
                values: vec![value.into()],
            },
        );
        let out = render(&schema, &IdsFile::default(), "Schema");
        assert!(
            out.contains(&format!("| {expected} |\n")),
            "{value:?}: {out}"
        );
    }
}

#[test]
fn prose_cells_cannot_emit_a_new_row_or_close_the_table_with_html() {
    for (value, expected) in [
        ("a|b", "a\\|b"),
        ("`code`", "`code`"),
        ("LF\nnext", "LF next"),
        ("CR\rnext", "CR next"),
        ("CRLF\r\nnext", "CRLF next"),
        ("row\n\n|fake|", "row  \\|fake\\|"),
        ("a\\|b", "a\\\\|b"),
        ("名|`字", "名\\|`字"),
        ("</td></tr><tr><td>bad", "&lt;/td>&lt;/tr>&lt;tr>&lt;td>bad"),
        ("`<x`", "`&lt;x`"),
        (
            "[link](url`) </td></tr><tr><td>tail `",
            "[link](url`) &lt;/td>&lt;/tr>&lt;tr>&lt;td>tail `",
        ),
        ("`<!-- note --><?pi?>`", "`&lt;!-- note -->&lt;?pi?>`"),
        ("a < b; amount<10; <=", "a < b; amount<10; <="),
    ] {
        let (mut schema, mut ids) = documented("ordinary");
        let column = schema
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .values_mut()
            .next()
            .unwrap();
        column.description = Some(value.into());
        column.deprecated = Some(value.into());
        let stone = ids.tombstones.values_mut().next().unwrap();
        stone.dropped_at = value.into();
        stone.operator = value.into();
        stone.reason = value.into();
        let out = render(&schema, &ids, "Schema");
        for row in [
            format!(
                "| `ordinary` ~~deprecated~~ | `app.ordinary` | yes | `ordinary` | {expected} |\n"
            ),
            format!("| `app.customer.ordinary` | {expected} |\n"),
            format!("| `ordinary` | {expected} | {expected} | {expected} |\n"),
        ] {
            assert!(out.contains(&row), "{value:?}: missing {row:?}: {out}");
        }
    }
}

#[test]
fn comparisons_in_descriptions_deprecations_and_graveyards_keep_their_text() {
    for newline in ["\n", "\r", "\r\n"] {
        let value = format!("Use `amount < 10 | mask` and a < b{newline}</td></tr><tr><td>tail");
        let expected = "Use `amount < 10 \\| mask` and a < b &lt;/td>&lt;/tr>&lt;tr>&lt;td>tail";
        let (mut schema, mut ids) = documented("ordinary");
        let column = schema
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .values_mut()
            .next()
            .unwrap();
        column.description = Some(value.as_str().into());
        column.deprecated = Some(value.as_str().into());
        let stone = ids.tombstones.values_mut().next().unwrap();
        stone.dropped_at = value.clone();
        stone.operator = value.clone();
        stone.reason = value;
        let out = render(&schema, &ids, "Schema");
        for row in [
            format!(
                "| `ordinary` ~~deprecated~~ | `app.ordinary` | yes | `ordinary` | {expected} |\n"
            ),
            format!("| `app.customer.ordinary` | {expected} |\n"),
            format!("| `ordinary` | {expected} | {expected} | {expected} |\n"),
        ] {
            assert!(out.contains(&row), "{newline:?}: missing {row:?}: {out}");
        }
        assert!(!out.contains("amount &lt; 10"), "{out}");
    }
}

#[test]
fn ordinary_values_in_every_table_remain_byte_identical() {
    assert_eq!(
        document("ordinary"),
        include_str!("fixtures/markdown-cells-ordinary.md")
    );
}

#[test]
fn empty_roles_and_projects_do_not_gain_grant_tables() {
    let mut schema = Schema::default();
    let out = render(&schema, &IdsFile::default(), "Schema");
    assert!(!out.contains("## Roles"));
    schema.roles.insert("pending".into(), Role::default());
    let out = render(&schema, &IdsFile::default(), "Schema");
    assert!(out.contains("### `pending`\n\nNo grants.\n"), "{out}");
    assert!(!out.contains("| Granted on |"));
}

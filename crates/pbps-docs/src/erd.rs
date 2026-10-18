//! The Mermaid `erDiagram`.
//!
//! Mermaid is the target because GitLab and GitHub render it natively in
//! Markdown: the diagram travels in the repository as text, needs no image
//! pipeline and no hosted service, and diffs meaningfully in review.
//!
//! Foreign keys are the only source of edges. That is exactly what makes the
//! declarations worth rendering: a live database has the same constraints, but
//! only the declarations carry the descriptions beside them.

use std::fmt::Write as _;

use pbps_model::{Schema, Table, TableName};

/// Aliases do not distinguish entities: Mermaid keys both attributes and edges
/// on this token. Escape underscores and keep the segment boundary outside the
/// character encoding so different qualified names cannot collapse (DEC-1799.1).
fn entity(name: &TableName) -> String {
    let mut id = String::from("t_");
    for (i, part) in [&name.schema, &name.name].into_iter().enumerate() {
        if i != 0 {
            id.push_str("_s_");
        }
        for c in part.chars() {
            if c == '_' {
                id.push_str("__");
            } else if c.is_ascii_alphanumeric() {
                id.push(c);
            } else {
                let _ = write!(id, "_{:x}_", u32::from(c));
            }
        }
    }
    id
}

/// Mermaid entity codes survive its lexer and HTML/Markdown label rendering.
/// Escape `#` too, or a literal `#34;` in a name becomes a quote. The colon
/// must not activate its global `style`/`classDef` preprocessing (DEC-1799.1).
fn alias(name: &TableName) -> String {
    label(&name.raw().to_string())
}

/// `text` as Mermaid label text, by the codes [`alias`] uses.
fn label(text: &str) -> String {
    let mut label = String::new();
    for c in text.chars() {
        if c.is_control()
            || matches!(
                c,
                '"' | '%' | '\\' | '#' | '&' | '<' | '>' | ':' | '_' | '*'
            )
        {
            let _ = write!(label, "#{};", u32::from(c));
        } else {
            label.push(c);
        }
    }
    label
}

/// `text` inside a quoted Mermaid string: a `"` would end the string, a line
/// ending the statement, and `#` would start an entity code. Measured with
/// Mermaid 11.12.0, a raw `"` in a relationship label stops the whole diagram
/// parsing (#1874). The rest is kept as written, so an ordinary name reads in
/// the source as it is.
fn quoted(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_control() || c == '"' || c == '#' {
            let _ = write!(out, "#{};", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// A column name as a Mermaid attribute name, which is one token. Mermaid's
/// lexer takes a name only as a run of ASCII letters, digits and `_`, or
/// characters from U+00C0 to U+FFFF, not starting with a digit, and reads
/// `PK`, `FK` and `UK` as key markers first, in any case, wherever a word
/// boundary follows them. Measured with Mermaid 11.12.0: a space, a leading
/// digit, `pk`, `pk-x` and `x~y` each stop the whole diagram parsing (#1874).
/// A name that is such a token is kept as it is. In any other, each character
/// outside that set becomes `_<hex>_`, and a `_` goes in front where the result
/// would start with a digit or a key marker. Two names may then share a token,
/// which Mermaid allows within an entity; the readable name goes in the
/// attribute's comment wherever the token differs.
fn attr_name(name: &str) -> String {
    let word =
        |c: char| c == '_' || c.is_ascii_alphanumeric() || ('\u{C0}'..='\u{FFFF}').contains(&c);
    let mut token = String::new();
    for c in name.chars() {
        if word(c) {
            token.push(c);
        } else {
            let _ = write!(token, "_{:x}_", u32::from(c));
        }
    }
    let marker = token.get(..2).is_some_and(|head| {
        ["pk", "fk", "uk"]
            .iter()
            .any(|m| head.eq_ignore_ascii_case(m))
            && !token[2..]
                .chars()
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphanumeric())
    });
    if token.is_empty() || token.starts_with(|c: char| c.is_ascii_digit()) || marker {
        token.insert(0, '_');
    }
    token
}

/// The attribute's quoted comment: the readable name where the token differs
/// from it, then the description.
fn attr_comment(name: &str, token: &str, description: Option<&str>) -> String {
    let description = description.map(|d| d.replace('"', "'"));
    match (token != name, description) {
        (false, None) => String::new(),
        (false, Some(d)) => format!(" \"{d}\""),
        (true, None) => format!(" \"{}\"", label(name)),
        (true, Some(d)) => format!(" \"{} — {d}\"", label(name)),
    }
}

/// Mermaid attribute types must be single tokens: `decimal(18, 2)` would end
/// the attribute early.
fn attr_type(ty: &str) -> String {
    ty.replace(' ', "")
        .replace(['(', ')', ','], "_")
        .trim_end_matches('_')
        .to_owned()
}

/// Renders the schema as a Mermaid `erDiagram`.
///
/// Output is deterministic: `Schema::tables` is a `BTreeMap`, and everything
/// below it is either ordered or sorted, so the same declarations always
/// produce byte-identical output.
pub fn render(schema: &Schema) -> String {
    let mut s = String::from("erDiagram\n");

    for (name, table) in &schema.tables {
        let _ = writeln!(s, "    {}[\"{}\"] {{", entity(name), alias(name));
        for (col_name, col) in &table.columns {
            // The key marker is what makes the diagram readable at a glance;
            // Mermaid only has PK and FK.
            let key = key_marker(table, col_name);
            let token = attr_name(col_name);
            let description = col.description.as_ref().map(|d| d.raw().to_string());
            let comment = attr_comment(col_name, &token, description.as_deref());
            let _ = writeln!(
                s,
                "        {} {token}{key}{comment}",
                attr_type(&col.ty.to_string()),
            );
        }
        // A SQL Server computed column has no type in the model: the engine
        // infers it (DEC-1174.1). `computed` stands in its place.
        for col_name in table.computed.keys() {
            let key = key_marker(table, col_name);
            let token = attr_name(col_name);
            let comment = attr_comment(col_name, &token, None);
            let _ = writeln!(s, "        computed {token}{key}{comment}");
        }
        s.push_str("    }\n");
    }

    for (name, table) in &schema.tables {
        for (fk_name, fk) in &table.foreign_keys {
            // Only draw an edge when the target is declared too; an edge to a
            // table that is not in the documentation would render as a phantom
            // empty entity.
            if !schema.tables.contains_key(&fk.references_table) {
                continue;
            }
            // A nullable referencing column means the relationship is optional,
            // which Mermaid spells `|o` rather than `||`.
            // Unless declared `not_null`, a computed column's nullability is
            // the engine's inference, which the model does not hold: optional
            // is the drawing that holds either way.
            let optional = fk.columns.iter().any(|c| {
                table.columns.get(c).is_some_and(|c| c.nullable)
                    || table.computed.get(c).is_some_and(|c| !c.not_null)
            });
            let left = if optional { "}o" } else { "}|" };
            let _ = writeln!(
                s,
                "    {} {left}--|| {} : \"{}\"",
                entity(name),
                entity(&fk.references_table),
                quoted(fk_name)
            );
        }
    }
    s
}

fn key_marker(table: &Table, column: &str) -> &'static str {
    let is_pk = table
        .primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.iter().any(|c| c == column));
    if is_pk {
        return " PK";
    }
    if table
        .foreign_keys
        .values()
        .any(|fk| fk.columns.iter().any(|c| c == column))
    {
        return " FK";
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, ComputedColumn, ForeignKey, PrimaryKey};

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn two_table_schema(nullable_fk: bool) -> Schema {
        let mut region = Table::default();
        region
            .columns
            .insert("region_id".into(), Column::new(ty("int")).not_null());
        region.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["region_id".into()],
            storage_parameters: Default::default(),
        });

        let mut customer = Table::default();
        customer
            .columns
            .insert("id".into(), Column::new(ty("bigint")).not_null());
        let region_col = if nullable_fk {
            Column::new(ty("int"))
        } else {
            Column::new(ty("int")).not_null()
        };
        customer.columns.insert("region_id".into(), region_col);
        customer.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        customer.foreign_keys.insert(
            "fk_customer_region".into(),
            ForeignKey {
                columns: vec!["region_id".into()],
                references_table: TableName::new("dbo", "region"),
                references_columns: vec!["region_id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );

        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "region"), region);
        s.tables.insert(TableName::new("dbo", "customer"), customer);
        s
    }

    #[test]
    fn foreign_keys_become_edges_and_keys_are_marked() {
        let out = render(&two_table_schema(false));
        assert!(out.starts_with("erDiagram\n"));
        assert!(out.contains("bigint id PK"), "{out}");
        assert!(out.contains("int region_id FK"), "{out}");
        assert!(
            out.contains("t_dbo_s_customer }|--|| t_dbo_s_region : \"fk_customer_region\""),
            "{out}"
        );
    }

    /// A nullable foreign key column is an optional relationship, and drawing it
    /// as mandatory would misdescribe the data.
    #[test]
    fn a_nullable_foreign_key_is_drawn_as_optional() {
        assert!(render(&two_table_schema(true)).contains("}o--||"));
        assert!(render(&two_table_schema(false)).contains("}|--||"));
    }

    #[test]
    fn colliding_names_keep_separate_attributes_and_foreign_key_endpoints() {
        let names = [
            (
                "sales",
                "order_item",
                "t_sales_s_order__item",
                "sales.order#95;item",
            ),
            (
                "sales_order",
                "item",
                "t_sales__order_s_item",
                "sales#95;order.item",
            ),
            ("public", "客戶", "t_public_s__5ba2__6236_", "public.客戶"),
            ("public", "訂單", "t_public_s__8a02__55ae_", "public.訂單"),
        ];
        let mut schema = Schema::default();
        for (i, (namespace, name, _, _)) in names.iter().enumerate() {
            let next = (i + 1) % names.len();
            let mut table = Table::default();
            table
                .columns
                .insert(format!("column_{i}"), Column::new(ty("bigint")).not_null());
            table.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec![format!("column_{i}")],
                storage_parameters: Default::default(),
            });
            table.foreign_keys.insert(
                format!("fk_{i}"),
                ForeignKey {
                    columns: vec![format!("column_{i}")],
                    references_table: TableName::new(names[next].0, names[next].1),
                    references_columns: vec![format!("column_{next}")],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            schema
                .tables
                .insert(TableName::new(*namespace, *name), table);
        }
        let out = render(&schema);
        assert_eq!(out.lines().filter(|line| line.ends_with(" {")).count(), 4);
        for (i, (_, _, id, label)) in names.iter().enumerate() {
            assert!(
                out.contains(&format!(
                    "{id}[\"{label}\"] {{\n        bigint column_{i} PK\n    }}"
                )),
                "{out}"
            );
            let next = names[(i + 1) % names.len()].2;
            assert!(
                out.contains(&format!("{id} }}|--|| {next} : \"fk_{i}\"")),
                "{out}"
            );
        }
        assert_eq!(out, render(&schema));
    }

    #[test]
    fn short_segment_pairs_have_distinct_lexer_safe_identities() {
        let alphabet = ['a', 'A', '_', 's', '5', 'f', '客', ' ', '-'];
        let mut segments = vec![String::new()];
        for a in alphabet {
            segments.push(a.to_string());
            for b in alphabet {
                segments.push(format!("{a}{b}"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for namespace in &segments {
            for name in &segments {
                let id = entity(&TableName::new(namespace, name));
                assert!(id.starts_with("t_"), "{id}");
                assert!(
                    id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{id}"
                );
                assert!(seen.insert(id), "collision for {namespace:?}.{name:?}");
            }
        }
        assert_eq!(seen.len(), segments.len().pow(2));
    }

    #[test]
    fn keyword_and_numeric_names_keep_a_fixed_identifier_prefix() {
        for namespace in ["u", "to", "one", "many", "class", "1"] {
            assert_eq!(
                entity(&TableName::new(namespace, "t")),
                format!("t_{namespace}_s_t")
            );
        }
        assert_ne!(
            entity(&TableName::new("public", "A")),
            entity(&TableName::new("public", "a"))
        );
    }

    #[test]
    fn alias_text_cannot_end_a_quote_or_become_a_mermaid_directive() {
        let name = TableName::new("style", "a:\"%\\\n#34;&<>\r\t");
        let expected = "style.a#58;#34;#37;#92;#10;#35;34;#38;#60;#62;#13;#9;";
        assert_eq!(alias(&name), expected);
        let mut schema = Schema::default();
        schema.tables.insert(name.clone(), Table::default());
        assert!(render(&schema).contains(&format!("{}[\"{expected}\"] {{", entity(&name))));
        assert_eq!(alias(&TableName::new("public", "客戶")), "public.客戶");
        assert_eq!(
            alias(&TableName::new("sales", "customer")),
            "sales.customer"
        );
        assert_eq!(
            alias(&TableName::new("public", "_*name*_")),
            "public.#95;#42;name#42;#95;"
        );
        assert_eq!(
            alias(&TableName::new("sales", "order_item")),
            "sales.order#95;item"
        );
    }

    /// A parameterised type would end the Mermaid attribute early and break the
    /// whole diagram, not just one row.
    #[test]
    fn a_column_name_is_one_mermaid_token_and_keeps_its_readable_name() {
        // Each of these stopped the whole diagram parsing in Mermaid 11.12.0
        // when written raw (#1874); each token below parses there, PK and FK
        // markers following it included.
        for (name, token) in [
            ("gross total", "gross_20_total"),
            ("1x", "_1x"),
            ("pk", "_pk"),
            ("Uk", "_Uk"),
            ("pk-x", "pk_2d_x"),
            ("x~y", "x_7e_y"),
            ("a\"b", "a_22_b"),
        ] {
            assert_eq!(attr_name(name), token, "{name}");
        }
        // Plain words are kept: a marker inside a word is no marker.
        for name in ["id", "pk_id", "pkx", "_a", "名前", "fk2"] {
            assert_eq!(attr_name(name), name);
        }

        let mut t = Table::default();
        let mut c = Column::new(ty("int"));
        c.description = Some("the \"sum\"".into());
        t.columns.insert("gross total".into(), c);
        t.columns.insert("id".into(), Column::new(ty("int")));
        t.computed.insert(
            "net #34; total".into(),
            ComputedColumn {
                expression: "[a] - [b]".into(),
                persisted: false,
                not_null: true,
            },
        );
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["gross total".into()],
            storage_parameters: Default::default(),
        });
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "t"), t);
        let out = render(&s);
        for line in [
            "        int gross_20_total PK \"gross total — the 'sum'\"\n",
            "        int id\n",
            "        computed net_20__23_34_3b__20_total \"net #35;34; total\"\n",
        ] {
            assert!(out.contains(line), "{line}{out}");
        }
    }

    #[test]
    fn a_foreign_key_name_cannot_end_its_quoted_label() {
        let mut s = two_table_schema(false);
        let t = s
            .tables
            .values_mut()
            .find(|t| !t.foreign_keys.is_empty())
            .unwrap();
        let fk = t.foreign_keys.pop_first().unwrap().1;
        t.foreign_keys.insert("fk \"a\" #1".into(), fk);
        let out = render(&s);
        assert!(out.contains(" : \"fk #34;a#34; #35;1\"\n"), "{out}");
        assert_eq!(quoted("fk_order"), "fk_order");
    }

    #[test]
    fn parameterised_types_are_flattened_into_one_token() {
        assert_eq!(attr_type("decimal(18, 2)"), "decimal_18_2");
        assert_eq!(attr_type("nvarchar(max)"), "nvarchar_max");
        assert_eq!(attr_type("bigint"), "bigint");
        let mut s = Schema::default();
        let mut t = Table::default();
        t.columns
            .insert("amount".into(), Column::new(ty("decimal(18,2)")));
        s.tables.insert(TableName::new("dbo", "t"), t);
        assert!(render(&s).contains("decimal_18_2 amount"));
    }

    /// An edge to a table outside the declarations would render as a phantom
    /// empty entity, which reads as "this table has no columns".
    #[test]
    fn an_edge_to_an_undeclared_table_is_omitted() {
        let mut s = two_table_schema(false);
        s.tables.remove(&TableName::new("dbo", "region"));
        let out = render(&s);
        assert!(!out.contains("--||"), "{out}");
    }

    /// A SQL Server computed column (#1174) has no type in the model; it is
    /// drawn with `computed` in its place, and keyed like any column.
    #[test]
    fn computed_columns_are_drawn_with_a_computed_marker() {
        let mut s = two_table_schema(false);
        let customer = s
            .tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap();
        customer.computed.insert(
            "total".into(),
            ComputedColumn {
                expression: "[qty] * [price]".into(),
                persisted: false,
                not_null: false,
            },
        );
        let out = render(&s);
        assert!(out.contains("        computed total\n"), "{out}");
    }

    /// An edge over a computed column is optional unless the column is
    /// declared `not_null`; its absence from `columns` must not read as
    /// mandatory.
    #[test]
    fn a_foreign_key_over_a_computed_column_is_optional_unless_not_null() {
        for (not_null, edge) in [(false, "}o--||"), (true, "}|--||")] {
            let mut s = two_table_schema(false);
            let customer = s
                .tables
                .get_mut(&TableName::new("dbo", "customer"))
                .unwrap();
            customer.columns.shift_remove("region_id");
            customer.computed.insert(
                "region_id".into(),
                ComputedColumn {
                    expression: "[id] % 10".into(),
                    persisted: true,
                    not_null,
                },
            );
            let out = render(&s);
            assert!(out.contains(edge), "not_null {not_null}: {out}");
            assert!(out.contains("computed region_id FK"), "{out}");
        }
    }

    #[test]
    fn rendering_is_deterministic() {
        let s = two_table_schema(false);
        let first = render(&s);
        for _ in 0..10 {
            assert_eq!(render(&s), first);
        }
    }
}

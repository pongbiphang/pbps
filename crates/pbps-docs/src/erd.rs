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

/// Mermaid identifiers cannot contain a dot, so `dbo.customer` is written
/// `dbo_customer`.
///
/// Two different names could in principle collide (`a.b_c` and `a_b.c`), which
/// is why the readable name is always restated as the entity's quoted alias
/// rather than left to the identifier.
fn entity(name: &TableName) -> String {
    format!("{}_{}", name.schema, name.name)
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_")
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
        let _ = writeln!(s, "    {}[\"{}\"] {{", entity(name), name);
        for (col_name, col) in &table.columns {
            // The key marker is what makes the diagram readable at a glance;
            // Mermaid only has PK and FK.
            let key = key_marker(table, col_name);
            let comment = col
                .description
                .as_deref()
                .map(|d| format!(" \"{}\"", d.replace('"', "'")))
                .unwrap_or_default();
            let _ = writeln!(
                s,
                "        {} {}{key}{comment}",
                attr_type(&col.ty.to_string()),
                col_name
            );
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
            let optional = fk
                .columns
                .iter()
                .any(|c| table.columns.get(c).is_some_and(|c| c.nullable));
            let left = if optional { "}o" } else { "}|" };
            let _ = writeln!(
                s,
                "    {} {left}--|| {} : \"{}\"",
                entity(name),
                entity(&fk.references_table),
                fk_name
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
    use pbps_model::{Column, ColumnType, ForeignKey, PrimaryKey};

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
            out.contains("dbo_customer }|--|| dbo_region : \"fk_customer_region\""),
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

    /// A parameterised type would end the Mermaid attribute early and break the
    /// whole diagram, not just one row.
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

    #[test]
    fn rendering_is_deterministic() {
        let s = two_table_schema(false);
        let first = render(&s);
        for _ in 0..10 {
            assert_eq!(render(&s), first);
        }
    }
}

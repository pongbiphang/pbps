//! Reading a live database back into the model — the heart of `pbps pull`.
//!
//! # Shape
//!
//! The catalog queries and the model assembly are deliberately separated: the
//! `Raw*` structs are plain data mirroring the catalog views, [`assemble`] is a
//! pure function from them to a [`Schema`], and only [`introspect`] touches a
//! connection. Everything that can be wrong here — a type read back differently
//! than declared, an index losing its INCLUDE columns — is in `assemble`, and a
//! pure `assemble` can be pinned by tests without a server in the room.
//!
//! # What cannot be expressed
//!
//! The model does not cover everything a database can hold (computed columns,
//! clustered-ness, collations). Those are **reported, never silently dropped**:
//! a `pull` that quietly loses a computed column would produce declarations that
//! plan the column's destruction on the next run. The caller decides whether the
//! warnings are acceptable.

use std::collections::BTreeMap;

use pbps_dialect::DialectError;
use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
    ReferentialAction, Schema, Table, TableName, TypeArg, UniqueConstraint,
};

use crate::types;

/// One row of `sys.tables`.
#[derive(Debug, Clone)]
pub struct RawTable {
    pub object_id: i32,
    pub schema: String,
    pub name: String,
}

/// One row of `sys.columns`, joined with its type, identity and default.
#[derive(Debug, Clone)]
pub struct RawColumn {
    pub object_id: i32,
    pub name: String,
    /// The type name as `sys.types` reports it.
    pub type_name: String,
    /// Bytes, not characters; `-1` for the `max` forms.
    pub max_length: i16,
    pub precision: u8,
    pub scale: u8,
    pub is_nullable: bool,
    pub is_computed: bool,
    /// Whether the type is a user-defined alias type.
    pub is_user_defined_type: bool,
    /// `(seed, increment)` when the column is IDENTITY.
    pub identity: Option<(i64, i64)>,
    /// The default definition as stored, wrapped in parentheses.
    pub default: Option<String>,
}

/// One column of a PRIMARY KEY or UNIQUE constraint, in key order.
#[derive(Debug, Clone)]
pub struct RawKeyColumn {
    pub object_id: i32,
    pub constraint_name: String,
    pub is_primary: bool,
    pub column: String,
}

/// One column pair of a foreign key, in constraint-column order.
#[derive(Debug, Clone)]
pub struct RawForeignKeyColumn {
    pub object_id: i32,
    pub constraint_name: String,
    pub ref_schema: String,
    pub ref_table: String,
    pub column: String,
    pub ref_column: String,
    /// `sys.foreign_keys.delete_referential_action`: 0..=3.
    pub on_delete: u8,
    pub on_update: u8,
}

/// One row of `sys.check_constraints`.
#[derive(Debug, Clone)]
pub struct RawCheck {
    pub object_id: i32,
    pub name: String,
    pub definition: String,
}

/// One column of an index that is not backing a PK or UNIQUE constraint.
#[derive(Debug, Clone)]
pub struct RawIndexColumn {
    pub object_id: i32,
    pub index_name: String,
    pub is_unique: bool,
    pub is_clustered: bool,
    pub filter: Option<String>,
    pub column: String,
    pub is_included: bool,
    pub is_descending: bool,
}

/// Everything read from one database.
#[derive(Debug, Clone, Default)]
pub struct RawCatalog {
    pub tables: Vec<RawTable>,
    pub columns: Vec<RawColumn>,
    pub key_columns: Vec<RawKeyColumn>,
    pub foreign_key_columns: Vec<RawForeignKeyColumn>,
    pub checks: Vec<RawCheck>,
    pub index_columns: Vec<RawIndexColumn>,
}

/// The result of a pull: the schema, plus everything that could not be said.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub schema: Schema,
    /// Facts about the database the model cannot express. Never empty silence:
    /// the caller must show these, because each one is a difference that would
    /// otherwise surface as phantom drift or a destructive plan later.
    pub warnings: Vec<String>,
}

/// Rebuilds the declared type from what the catalog stores.
///
/// The catalog does not keep the spelling the user wrote; it keeps the type id
/// plus lengths. This mapping plus [`types::normalize`] is what makes a pulled
/// schema comparable with a declared one.
fn column_type(c: &RawColumn) -> Result<ColumnType, DialectError> {
    let name = c.type_name.to_ascii_lowercase();
    let ty = match name.as_str() {
        // max_length is bytes; the n-types store UTF-16, two bytes a character.
        "nchar" | "nvarchar" => {
            if c.max_length == -1 {
                ColumnType::new(name, vec![TypeArg::Max])
            } else {
                ColumnType::new(name, vec![TypeArg::Int(i64::from(c.max_length) / 2)])
            }
        }
        "char" | "varchar" | "binary" | "varbinary" => {
            if c.max_length == -1 {
                ColumnType::new(name, vec![TypeArg::Max])
            } else {
                ColumnType::new(name, vec![TypeArg::Int(i64::from(c.max_length))])
            }
        }
        "decimal" | "numeric" => ColumnType::new(
            name,
            vec![
                TypeArg::Int(i64::from(c.precision)),
                TypeArg::Int(i64::from(c.scale)),
            ],
        ),
        "datetime2" | "datetimeoffset" | "time" => {
            ColumnType::new(name, vec![TypeArg::Int(i64::from(c.scale))])
        }
        _ => ColumnType::simple(name),
    };
    types::normalize(&ty)
}

/// Strips the parentheses the engine wraps around stored expressions.
///
/// A default declared as `0` is stored as `((0))`; a filter declared as
/// `a IS NOT NULL` comes back `([a] IS NOT NULL)`. Only *whole-string balanced*
/// pairs are removed, so `(a) AND (b)` is untouched — peeling that would change
/// its meaning.
pub fn strip_stored_parens(s: &str) -> &str {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') {
        let inner = &s[1..s.len() - 1];
        let mut depth = 0i32;
        // The outer pair is removable only if it closes at the very end.
        if inner.chars().all(|ch| {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth >= 0
        }) && depth == 0
        {
            s = inner.trim();
        } else {
            break;
        }
    }
    s
}

fn action(code: u8) -> ReferentialAction {
    match code {
        1 => ReferentialAction::Cascade,
        2 => ReferentialAction::SetNull,
        3 => ReferentialAction::SetDefault,
        _ => ReferentialAction::NoAction,
    }
}

/// Assembles the raw catalog rows into a [`Schema`].
///
/// Rows referring to an object id that is not in `tables` are ignored: the
/// queries fetch the whole database, and the table list is what defines the
/// managed set.
pub fn assemble(raw: &RawCatalog) -> Pulled {
    let mut warnings = Vec::new();
    let mut names: BTreeMap<i32, TableName> = BTreeMap::new();
    let mut tables: BTreeMap<i32, Table> = BTreeMap::new();

    for t in &raw.tables {
        names.insert(
            t.object_id,
            TableName::new(t.schema.clone(), t.name.clone()),
        );
        tables.insert(t.object_id, Table::default());
    }
    let name_of = |id: i32, names: &BTreeMap<i32, TableName>| {
        names
            .get(&id)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("object {id}"))
    };

    for c in &raw.columns {
        let Some(table) = tables.get_mut(&c.object_id) else {
            continue;
        };
        let table_name = name_of(c.object_id, &names);

        if c.is_computed {
            warnings.push(format!(
                "{table_name}.{}: computed columns are not supported yet; it was left out of the declarations",
                c.name
            ));
            continue;
        }
        if c.is_user_defined_type {
            warnings.push(format!(
                "{table_name}.{}: user-defined type `{}` is not supported yet; it was left out of the declarations",
                c.name, c.type_name
            ));
            continue;
        }
        let ty = match column_type(c) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!(
                    "{table_name}.{}: {e}; it was left out of the declarations",
                    c.name
                ));
                continue;
            }
        };

        let mut column = Column::new(ty);
        column.nullable = c.is_nullable;
        column.identity = c
            .identity
            .map(|(seed, increment)| Identity { seed, increment });
        column.default = c
            .default
            .as_deref()
            .map(|d| strip_stored_parens(d).to_owned());
        table.columns.insert(c.name.clone(), column);
    }

    for k in &raw.key_columns {
        let Some(table) = tables.get_mut(&k.object_id) else {
            continue;
        };
        if k.is_primary {
            let pk = table.primary_key.get_or_insert_with(|| PrimaryKey {
                name: Some(k.constraint_name.clone()),
                columns: Vec::new(),
            });
            pk.columns.push(k.column.clone());
        } else {
            table
                .unique
                .entry(k.constraint_name.clone())
                .or_insert_with(|| UniqueConstraint {
                    columns: Vec::new(),
                })
                .columns
                .push(k.column.clone());
        }
    }

    for f in &raw.foreign_key_columns {
        let Some(table) = tables.get_mut(&f.object_id) else {
            continue;
        };
        let fk = table
            .foreign_keys
            .entry(f.constraint_name.clone())
            .or_insert_with(|| ForeignKey {
                columns: Vec::new(),
                references_table: TableName::new(f.ref_schema.clone(), f.ref_table.clone()),
                references_columns: Vec::new(),
                on_delete: action(f.on_delete),
                on_update: action(f.on_update),
            });
        fk.columns.push(f.column.clone());
        fk.references_columns.push(f.ref_column.clone());
    }

    for c in &raw.checks {
        let Some(table) = tables.get_mut(&c.object_id) else {
            continue;
        };
        table.checks.insert(
            c.name.clone(),
            CheckConstraint {
                expression: strip_stored_parens(&c.definition).to_owned(),
            },
        );
    }

    for i in &raw.index_columns {
        let Some(table) = tables.get_mut(&i.object_id) else {
            continue;
        };
        if i.is_clustered {
            // The model has no clustered-ness; recording the index without it
            // would make bootstrap create a different physical layout.
            let table_name = name_of(i.object_id, &names);
            if !warnings
                .iter()
                .any(|w| w.contains(&format!("index `{}`", i.index_name)))
            {
                warnings.push(format!(
                    "{table_name}: index `{}` is clustered, which is not modelled yet; it was left out of the declarations",
                    i.index_name
                ));
            }
            continue;
        }
        let index = table
            .indexes
            .entry(i.index_name.clone())
            .or_insert_with(|| Index {
                columns: Vec::new(),
                include: Vec::new(),
                unique: i.is_unique,
                filter: i
                    .filter
                    .as_deref()
                    .map(|f| strip_stored_parens(f).to_owned()),
            });
        if i.is_included {
            index.include.push(i.column.clone());
        } else {
            index.columns.push(IndexColumn {
                name: i.column.clone(),
                descending: i.is_descending,
            });
        }
    }

    let mut schema = Schema::default();
    for (id, table) in tables {
        // A table whose every column was unsupported must not be declared as an
        // empty table — that would plan the drop of the columns it really has.
        if table.columns.is_empty() {
            let table_name = name_of(id, &names);
            warnings.push(format!(
                "{table_name}: no supported columns remain; the whole table was left out of the declarations"
            ));
            continue;
        }
        schema.tables.insert(names.remove(&id).unwrap(), table);
    }

    Pulled { schema, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_table(id: i32, schema: &str, name: &str) -> RawTable {
        RawTable {
            object_id: id,
            schema: schema.into(),
            name: name.into(),
        }
    }

    fn raw_column(id: i32, name: &str, type_name: &str) -> RawColumn {
        RawColumn {
            object_id: id,
            name: name.into(),
            type_name: type_name.into(),
            max_length: 8,
            precision: 0,
            scale: 0,
            is_nullable: true,
            is_computed: false,
            is_user_defined_type: false,
            identity: None,
            default: None,
        }
    }

    #[test]
    fn nvarchar_lengths_are_bytes_and_come_back_as_characters() {
        let mut c = raw_column(1, "x", "nvarchar");
        c.max_length = 200;
        assert_eq!(column_type(&c).unwrap().to_string(), "nvarchar(100)");
        c.max_length = -1;
        assert_eq!(column_type(&c).unwrap().to_string(), "nvarchar(max)");
        let mut v = raw_column(1, "x", "varchar");
        v.max_length = 200;
        assert_eq!(column_type(&v).unwrap().to_string(), "varchar(200)");
    }

    #[test]
    fn decimal_and_time_types_carry_their_stored_arguments() {
        let mut c = raw_column(1, "x", "numeric");
        (c.precision, c.scale) = (12, 4);
        // numeric normalizes to decimal, same as on the declared side.
        assert_eq!(column_type(&c).unwrap().to_string(), "decimal(12, 4)");
        let mut t = raw_column(1, "x", "datetime2");
        t.scale = 3;
        assert_eq!(column_type(&t).unwrap().to_string(), "datetime2(3)");
    }

    #[test]
    fn stored_parens_are_peeled_only_when_they_wrap_the_whole_string() {
        assert_eq!(strip_stored_parens("((0))"), "0");
        assert_eq!(strip_stored_parens("(getdate())"), "getdate()");
        assert_eq!(strip_stored_parens("([amount]>(0))"), "[amount]>(0)");
        // Peeling this one would change its meaning.
        assert_eq!(strip_stored_parens("(a) AND (b)"), "(a) AND (b)");
        assert_eq!(strip_stored_parens("plain"), "plain");
    }

    fn one_table_catalog() -> RawCatalog {
        let mut id_col = raw_column(10, "id", "bigint");
        id_col.is_nullable = false;
        id_col.identity = Some((1, 1));
        let mut email = raw_column(10, "email", "nvarchar");
        email.max_length = 510;
        let mut status = raw_column(10, "status", "tinyint");
        status.is_nullable = false;
        status.default = Some("((0))".into());
        RawCatalog {
            tables: vec![raw_table(10, "dbo", "customer")],
            columns: vec![id_col, email, status],
            key_columns: vec![RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "id".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_full_table_assembles_with_no_warnings() {
        let p = assemble(&one_table_catalog());
        assert_eq!(p.warnings, Vec::<String>::new());
        let t = p
            .schema
            .tables
            .get(&TableName::new("dbo", "customer"))
            .unwrap();
        assert_eq!(
            t.columns["id"].identity,
            Some(Identity {
                seed: 1,
                increment: 1
            })
        );
        assert_eq!(t.columns["email"].ty.to_string(), "nvarchar(255)");
        assert_eq!(t.columns["status"].default.as_deref(), Some("0"));
        assert_eq!(
            t.primary_key.as_ref().unwrap().columns,
            vec!["id".to_owned()]
        );
    }

    /// Losing a column silently is the one unforgivable failure of pull: the
    /// generated declarations would plan its destruction.
    #[test]
    fn unsupported_columns_are_warned_about_never_dropped_silently() {
        let mut raw = one_table_catalog();
        let mut computed = raw_column(10, "total", "money");
        computed.is_computed = true;
        raw.columns.push(computed);
        let mut udt = raw_column(10, "region_code", "my_udt");
        udt.is_user_defined_type = true;
        raw.columns.push(udt);

        let p = assemble(&raw);
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("computed"), "{:?}", p.warnings);
        assert!(p.warnings[1].contains("my_udt"), "{:?}", p.warnings);
        // The table itself survives with the supported columns.
        let t = p
            .schema
            .tables
            .get(&TableName::new("dbo", "customer"))
            .unwrap();
        assert_eq!(t.columns.len(), 3);
    }

    #[test]
    fn a_table_with_no_expressible_columns_is_left_out_entirely() {
        let mut c = raw_column(11, "only", "geometry_udt");
        c.is_user_defined_type = true;
        let raw = RawCatalog {
            tables: vec![raw_table(11, "dbo", "shapes")],
            columns: vec![c],
            ..Default::default()
        };
        let p = assemble(&raw);
        assert!(p.schema.tables.is_empty());
        assert!(
            p.warnings.iter().any(|w| w.contains("whole table")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn multi_column_keys_keep_their_order() {
        let mut raw = one_table_catalog();
        raw.key_columns = vec![
            RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "email".into(),
            },
            RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "id".into(),
            },
        ];
        let p = assemble(&raw);
        let t = &p.schema.tables[&TableName::new("dbo", "customer")];
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["email", "id"]);
    }

    #[test]
    fn foreign_keys_line_both_sides_up_in_order() {
        let mut raw = one_table_catalog();
        for (a, b) in [("id", "cid"), ("email", "cemail")] {
            raw.foreign_key_columns.push(RawForeignKeyColumn {
                object_id: 10,
                constraint_name: "fk_x".into(),
                ref_schema: "dbo".into(),
                ref_table: "other".into(),
                column: a.into(),
                ref_column: b.into(),
                on_delete: 1,
                on_update: 0,
            });
        }
        let p = assemble(&raw);
        let fk = &p.schema.tables[&TableName::new("dbo", "customer")].foreign_keys["fk_x"];
        assert_eq!(fk.columns, ["id", "email"]);
        assert_eq!(fk.references_columns, ["cid", "cemail"]);
        assert_eq!(fk.on_delete, ReferentialAction::Cascade);
        assert_eq!(fk.on_update, ReferentialAction::NoAction);
    }

    #[test]
    fn index_key_and_include_columns_are_kept_apart() {
        let mut raw = one_table_catalog();
        let base = RawIndexColumn {
            object_id: 10,
            index_name: "ix_email".into(),
            is_unique: false,
            is_clustered: false,
            filter: Some("([email] IS NOT NULL)".into()),
            column: "email".into(),
            is_included: false,
            is_descending: true,
        };
        raw.index_columns.push(base.clone());
        raw.index_columns.push(RawIndexColumn {
            column: "status".into(),
            is_included: true,
            ..base
        });
        let p = assemble(&raw);
        let ix = &p.schema.tables[&TableName::new("dbo", "customer")].indexes["ix_email"];
        assert_eq!(ix.columns.len(), 1);
        assert!(ix.columns[0].descending);
        assert_eq!(ix.include, ["status"]);
        assert_eq!(ix.filter.as_deref(), Some("[email] IS NOT NULL"));
    }

    #[test]
    fn clustered_indexes_are_warned_about_and_left_out() {
        let mut raw = one_table_catalog();
        raw.index_columns.push(RawIndexColumn {
            object_id: 10,
            index_name: "cx_customer".into(),
            is_unique: false,
            is_clustered: true,
            filter: None,
            column: "id".into(),
            is_included: false,
            is_descending: false,
        });
        let p = assemble(&raw);
        assert!(
            p.schema.tables[&TableName::new("dbo", "customer")]
                .indexes
                .is_empty()
        );
        assert!(p.warnings.iter().any(|w| w.contains("clustered")));
    }

    /// Rows for tables outside the managed set (dropped between queries, or
    /// pbps's own state tables) must not invent entries.
    #[test]
    fn rows_for_unknown_tables_are_ignored() {
        let mut raw = one_table_catalog();
        raw.columns.push(raw_column(99, "ghost", "int"));
        raw.checks.push(RawCheck {
            object_id: 99,
            name: "ck_ghost".into(),
            definition: "(1=1)".into(),
        });
        let p = assemble(&raw);
        assert_eq!(p.schema.tables.len(), 1);
    }
}

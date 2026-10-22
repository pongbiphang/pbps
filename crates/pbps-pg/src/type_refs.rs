//! Prerequisites of columns that reference existing types (DEC-1523.1–3).

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{
    Change, ChangeSet, Column, ColumnName, ColumnType, ModuleKind, Schema, TypePin, TypeRef,
    ValueSource,
};

use crate::pins::Within;
use crate::user_types::{self, TypeRead, UserType};

fn failed(why: impl Into<String>) -> DbError {
    DbError::BadRow(why.into())
}

pub fn references(schemas: &[&Schema], changes: &ChangeSet) -> BTreeSet<TypeRef> {
    let mut types: Vec<&ColumnType> = schemas
        .iter()
        .flat_map(|s| s.tables.values())
        .flat_map(|t| t.columns.values().map(|c| &c.ty))
        .collect();
    for planned in &changes.changes {
        match &planned.change {
            Change::CreateTable { table, .. }
            | Change::AttachPartition { shape: table, .. }
            | Change::DetachPartition { shape: table, .. } => {
                types.extend(table.columns.values().map(|c| &c.ty));
            }
            Change::AddColumn { column, .. } => types.push(&column.ty),
            Change::AlterColumnType { from, to, .. } => types.extend([from, to]),
            Change::AlterColumnNullability { ty, .. } | Change::AlterColumnNumbering { ty, .. } => {
                types.push(ty)
            }
            Change::InsertRow { types: carried, .. } => types.extend(carried.values()),
            Change::UpdateRow {
                types: before,
                after_types,
                key_type,
                ..
            }
            | Change::DeleteRow {
                types: before,
                after_types,
                key_type,
                ..
            } => {
                types.extend(before.values());
                types.extend(after_types.values());
                types.extend(key_type);
            }
            Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::DropPolicy { .. }
            | Change::CreatePolicy { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => {}
        }
    }
    types
        .into_iter()
        .filter_map(|ty| ty.type_ref().cloned())
        .collect()
}

pub(crate) async fn read(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
) -> Result<Vec<UserType>, DbError> {
    let reads = user_types::read(conn, within, references).await;
    reads
        .into_iter()
        .zip(references)
        .map(|(read, reference)| match read {
            TypeRead::Present(ty) => Ok(*ty),
            other @ (TypeRead::Absent
            | TypeRead::NotDefined
            | TypeRead::Unreadable(_)
            | TypeRead::NotAdmitted(_)) => Err(failed(
                other
                    .refusal(reference)
                    .expect("a non-present type refuses"),
            )),
        })
        .collect()
}

fn pin(ty: &UserType) -> TypePin {
    TypePin {
        schema: ty.reference().schema().to_owned(),
        name: ty.reference().name().to_owned(),
        oid: ty.oid(),
        kind: ty.kind().as_str().to_owned(),
        digest: ty.pin().as_str().to_owned(),
    }
}

pub async fn trusted_roles(
    conn: &mut Conn,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<(), DbError> {
    let missing =
        crate::input_path::missing_roles(conn, &trusted.iter().cloned().collect::<Vec<_>>())
            .await?;
    if !missing.is_empty() {
        return Err(failed(format!(
            "trusted_roles names roles that do not exist: {}",
            missing
                .iter()
                .map(|role| role.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<Vec<TypePin>, DbError> {
    trusted_roles(conn, trusted).await?;
    let references: Vec<_> = references(schemas, changes).into_iter().collect();
    let types = read(conn, Within::OwnTransaction, &references).await?;
    for schema in schemas {
        for (table, definition) in &schema.tables {
            let check_default = |column: &ColumnName, spec: &Column| {
                let ty = &spec.ty;
                let Some(reference) = ty.type_ref() else {
                    return Ok(());
                };
                if fill(spec) == Fill::Null
                    && (ty.is_array()
                        || types.iter().any(|ty| {
                            ty.reference() == reference && ty.kind() != user_types::Kind::Domain
                        }))
                {
                    return Err(failed(format!(
                        "column `{table}.{column}` has a NULL default the engine does not keep; declare no default",
                        table = table.shown(),
                        column = column.shown()
                    )));
                }
                Ok(())
            };
            for (column, spec) in &definition.columns {
                if matches!(&spec.value_source, ValueSource::Default(_)) {
                    check_default(column, spec)?;
                }
            }
            // Partitions declare only overrides; their column types live on the parent.
            if let Some(part) = &definition.partition_of
                && let Some(parent) = schema.tables.get(&part.parent)
            {
                for (column, own) in &part.columns {
                    if let Some(spec) = parent.columns.get(column)
                        && let Some(expr) = &own.default
                    {
                        let mut override_column = Column::new(spec.ty.clone());
                        override_column.value_source = ValueSource::Default(expr.clone());
                        check_default(column, &override_column)?;
                    }
                }
            }
        }
    }
    // Names alone conflate overloads and objects in different system catalogs.
    let mut managed_modules = BTreeSet::new();
    for schema in schemas {
        for (id, module) in &schema.modules {
            let catalog = match module.kind {
                ModuleKind::View => "pg_class",
                ModuleKind::Function | ModuleKind::Procedure => "pg_proc",
                ModuleKind::Trigger => "pg_trigger",
            };
            if types.iter().flat_map(UserType::reached).any(|r| {
                r.catalog == catalog
                    && r.schema.as_deref() == Some(id.schema())
                    && r.name.as_deref() == Some(id.name())
            }) && let Some(oid) = crate::modules::module_oid(conn, id, module.kind).await?
            {
                managed_modules.insert((catalog, oid));
            }
        }
    }
    for ty in &types {
        if let Some(reached) = ty.reaches_managed(|r| {
            r.catalog == "pg_class"
                && schemas.iter().any(|s| {
                    s.tables.keys().any(|t| {
                        r.schema.as_deref() == Some(&t.schema) && r.name.as_deref() == Some(&t.name)
                    })
                })
                || managed_modules.contains(&(r.catalog.as_str(), r.oid))
        }) {
            return Err(failed(format!(
                "type `{}` depends on managed {} `{}`; a referenced type cannot depend on an object pbps changes",
                ty.reference().shown(),
                reached.kind,
                reached.identity
            )));
        }
    }
    Ok(types.iter().map(pin).collect())
}

pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[TypePin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let references: Vec<_> = expected
        .iter()
        .map(|p| TypeRef::new(&p.schema, &p.name).map_err(|e| failed(e.to_string())))
        .collect::<Result<_, _>>()?;
    for reference in self::references(&[], changes) {
        if !references.contains(&reference) {
            return Err(failed(format!(
                "plan has no type pin for `{reference}`; plan again against the database",
                reference = reference.shown(),
            )));
        }
    }
    let actual: Vec<_> = read(conn, within, &references)
        .await?
        .iter()
        .map(pin)
        .collect();
    for (old, new) in expected.iter().zip(&actual) {
        if old != new {
            return Err(failed(format!(
                "the referenced type `{}.{}` changed since planning; plan again",
                old.schema, old.name
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    Null,
    EmptyArray,
    Constructs,
}

// #2155: admission and erased-NULL sealing must agree for every value source.
// Unrecognised shapes keep the full input-path check; no expression runs here.
fn fill(column: &Column) -> Fill {
    let expr = match &column.value_source {
        ValueSource::Default(expr) => expr,
        ValueSource::Generated(g) if g.stored => &g.expression,
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => return Fill::Constructs,
    };
    let (core, casts) = crate::rows::unwrapped_with_casts(expr);
    if !casts.iter().all(|cast| {
        cast.parse::<ColumnType>()
            .ok()
            .and_then(|ty| crate::types::normalize(&ty).ok())
            .is_some_and(|ty| ty == column.ty)
    }) {
        return Fill::Constructs;
    }
    if core.eq_ignore_ascii_case("null") {
        return Fill::Null;
    }
    let empty = core == "'{}'"
        || (core
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("array["))
            && core.ends_with(']')
            && core[6..core.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_whitespace()));
    // A domain over an array still evaluates its own CHECK.
    if empty && column.ty.is_array() && !casts.is_empty() {
        Fill::EmptyArray
    } else {
        Fill::Constructs
    }
}

/// Potential input paths, independent of the change kind or fill (DEC-2181.1).
/// Both construction and the declared type's own DEFAULT must be eligible.
pub async fn input_routines(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> Result<BTreeSet<i64>, DbError> {
    use crate::input_path::{Added, InputPath, Reason, Why};
    let types = read(conn, within, references).await?;
    let added: Vec<_> = types
        .iter()
        .flat_map(|ty| [Added::new(ty, true), Added::new(ty, false)])
        .collect();
    let roles: Vec<_> = trusted.iter().cloned().collect();
    let answers = crate::input_path::read(conn, within, &roles, &added).await;
    if answers.len() != added.len() {
        return Err(failed("the input-path read returned an incomplete answer"));
    }
    let mut roots = BTreeSet::new();
    for (index, mut answer) in answers.into_iter().enumerate() {
        if let InputPath::Refused(reasons) = &mut answer {
            reasons.retain(|reason| {
                if let Reason::Routine(routine) = reason
                    && routine.why == Why::NotBuiltin
                    && pin_routines
                {
                    roots.insert(routine.oid);
                    return false;
                }
                true
            });
            if reasons.is_empty() {
                continue;
            }
        }
        if let Some(why) = answer.refusal(&references[index / 2]) {
            return Err(failed(why));
        }
    }
    Ok(roots)
}

/// Offline reconstruction asserts prerequisites before it creates any object.
pub fn bootstrap_guard(
    schema: &Schema,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<Option<String>, pbps_dialect::DialectError> {
    let refs = references(&[schema], &ChangeSet::default());
    if refs.is_empty() && trusted.is_empty() {
        return Ok(None);
    }
    let mut sql = String::from("BEGIN\n");
    for reference in refs {
        let schema = crate::emit::literal(reference.schema());
        let name = crate::emit::literal(reference.name());
        let message = crate::emit::literal(&format!(
            "required type `{reference}` is missing, unusable or not an enum, domain or standalone composite",
            // Raw SQL message literal; preserve its bytes until #2084 step 5.
            reference = reference.raw(),
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid WHERE n.nspname={schema} AND t.typname={name} AND t.typisdefined AND (t.typtype IN ('e','d') OR (t.typtype='c' AND c.relkind='c')) AND pg_catalog.has_schema_privilege(n.oid,'USAGE') AND pg_catalog.has_type_privilege(t.oid,'USAGE')) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    for role in trusted {
        let name = crate::emit::literal(role.raw());
        let message = crate::emit::literal(&format!(
            "trusted role `{role}` does not exist",
            role = role.shown()
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname={name}) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    sql.push_str("END");
    let tag = crate::emit::dollar_tag(&sql);
    Ok(Some(format!("DO {tag}\n{sql}\n{tag};")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_stored_generated_fills_share_the_same_cast_checked_shapes() {
        for ty in ["app.checked", "app.checked[]"] {
            for expr in [
                "NULL".to_owned(),
                format!("NULL::{ty}"),
                format!("CAST(NULL AS {ty})"),
                format!("(NULL::{ty})::{ty}"),
            ] {
                for source in [
                    ValueSource::Default(expr.clone()),
                    ValueSource::Generated(pbps_model::Generated {
                        expression: expr.clone(),
                        stored: true,
                    }),
                ] {
                    let mut column = Column::new(ty.parse().unwrap());
                    column.value_source = source;
                    assert_eq!(fill(&column), Fill::Null, "{ty}: {:?}", column.value_source);
                }
            }
        }
        for (ty, expr, expected) in [
            ("app.checked[]", "ARRAY[]::app.checked[]", Fill::EmptyArray),
            (
                "app.checked[]",
                "ARRAY[ \t\n ]::app.checked[]",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "CAST(ARRAY[] AS app.checked[])",
                Fill::EmptyArray,
            ),
            ("app.checked[]", "'{}'::app.checked[]", Fill::EmptyArray),
            (
                "app.checked[]",
                "CAST('{}' AS app.checked[])",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "(ARRAY[]::app.checked[])::app.checked[]",
                Fill::EmptyArray,
            ),
            (
                "app.checked[]",
                "ARRAY[NULL]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "ARRAY['x']::app.checked[]",
                Fill::Constructs,
            ),
            ("app.checked[]", "'{x}'::app.checked[]", Fill::Constructs),
            ("app.checked[]", "'{{}}'::app.checked[]", Fill::Constructs),
            (
                "app.checked[]",
                "ARRAY[ARRAY[]]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "app.empty_array()::app.checked[]",
                Fill::Constructs,
            ),
            ("app.checked[]", "'{}'", Fill::Constructs),
            ("app.checked[]", "ARRAY[]", Fill::Constructs),
            ("app.checked[]", "ARRAY[]::app.other[]", Fill::Constructs),
            (
                "app.checked[]",
                "ARRAY[]::app.other[]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked[]",
                "ARRAY[\u{a0}]::app.checked[]",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "NULL::app.domain_over_checked",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "NULL::app.domain_over_checked::app.checked",
                Fill::Constructs,
            ),
            (
                "app.checked",
                "CAST(NULL::app.domain_over_checked AS app.checked)",
                Fill::Constructs,
            ),
            (
                "app.outer_domain",
                "'{}'::app.outer_domain",
                Fill::Constructs,
            ),
            (
                "app.outer_domain",
                "ARRAY[]::app.checked[]",
                Fill::Constructs,
            ),
        ] {
            for source in [
                ValueSource::Default(expr.into()),
                ValueSource::Generated(pbps_model::Generated {
                    expression: expr.into(),
                    stored: true,
                }),
            ] {
                let mut column = Column::new(ty.parse().unwrap());
                column.value_source = source;
                assert_eq!(fill(&column), expected, "{ty}: {:?}", column.value_source);
            }
        }
    }

    #[test]
    fn sources_without_a_stored_fill_expression_construct_values() {
        for source in [
            ValueSource::None,
            ValueSource::Identity(pbps_model::Identity::new(1, 1)),
            ValueSource::Sequence(pbps_model::OwnedSequence::serial()),
            ValueSource::Generated(pbps_model::Generated {
                expression: "'{}'::app.checked[]".into(),
                stored: false,
            }),
        ] {
            let mut column = Column::new("app.checked[]".parse().unwrap());
            column.value_source = source;
            assert_eq!(fill(&column), Fill::Constructs, "{:?}", column.value_source);
        }
    }

    #[test]
    fn bootstrap_guards_only_references_and_choose_a_tag_the_names_cannot_close() {
        let mut schema = Schema::default();
        assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "v".into(),
            pbps_model::Column::new(ColumnType::reference(
                TypeRef::new("app", "x$pbps$y").unwrap(),
            )),
        );
        schema
            .tables
            .insert(pbps_model::TableName::new("app", "t"), table);
        let sql = bootstrap_guard(&schema, &BTreeSet::new()).unwrap().unwrap();
        assert!(sql.starts_with("DO $pbps1$"), "{sql}");
        assert_eq!(sql.matches("$pbps1$").count(), 2);
        assert!(
            sql.contains("t.typisdefined")
                && sql.contains("has_type_privilege")
                && sql.contains("c.relkind='c'")
        );
        assert_eq!(references(&[&schema], &ChangeSet::default()).len(), 1);
    }

    #[test]
    fn referenced_type_keys_and_cast_fallback_keep_raw_names_distinct() {
        let raw = TypeRef::new("app", "type\n\\\u{1b}\"名前").unwrap();
        let visible = TypeRef::new("app", r#"type\n\\\u{1b}"名前"#).unwrap();
        let ty = ColumnType::reference(raw.clone());
        let other = ColumnType::reference(visible.clone());
        let table = pbps_model::Table {
            columns: [
                ("raw".into(), Column::new(ty.clone())),
                ("shown".into(), Column::new(other.clone().into_array())),
            ]
            .into(),
            ..Default::default()
        };
        let schema = Schema {
            tables: [(pbps_model::TableName::new("app", "t"), table)].into(),
            ..Default::default()
        };
        assert_eq!(
            references(&[&schema], &ChangeSet::default()),
            BTreeSet::from([raw.clone(), visible])
        );
        let canonical = "app.\"type\n\\\u{1b}\"\"名前\"";
        let mut column = Column::new(ty);
        column.value_source = ValueSource::Default(format!("NULL::{canonical}"));
        // The existing cast recognizer conservatively leaves quoted control
        // characters to the full input-path check; changing that parser is
        // outside the mechanical name migration (#2084 step 1c).
        assert_eq!(fill(&column), Fill::Constructs);
        column.value_source = ValueSource::Default(format!("NULL::{}", other.raw()));
        assert_eq!(fill(&column), Fill::Constructs);
        let pins = [pbps_model::TypePin {
            schema: raw.schema().into(),
            name: raw.name().into(),
            oid: 1,
            kind: "enum".into(),
            digest: "ab".repeat(32),
        }];
        let from_pin = TypeRef::new(&pins[0].schema, &pins[0].name).unwrap();
        assert_eq!(from_pin, raw);
        assert!(references(&[&schema], &ChangeSet::default()).contains(&from_pin));
    }
}

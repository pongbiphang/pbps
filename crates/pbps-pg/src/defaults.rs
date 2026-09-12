//! Plan-owned literal resolution. Declaration text remains the ledger truth.

use std::collections::BTreeMap;

use pbps_dialect::{DialectError, Statement};
use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, DefaultResolution, PlannedChange};

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Invalid(#[from] DialectError),
    #[error(transparent)]
    Database(#[from] pbps_db::DbError),
}

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: "postgres",
        message: message.into(),
    }
}

fn declared(p: &PlannedChange) -> BTreeMap<ColumnRef, (&str, Option<&ColumnType>)> {
    match &p.change {
        Change::CreateTable { name, table, .. } => table
            .columns
            .iter()
            .filter_map(|(n, c)| {
                c.default
                    .as_deref()
                    .map(|s| (ColumnRef::new(name.clone(), n), (s, Some(&c.ty))))
            })
            .collect(),
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => column
            .default
            .as_deref()
            .map(|s| (ColumnRef::new(table.clone(), name), (s, Some(&column.ty))))
            .into_iter()
            .collect(),
        Change::AlterColumnDefault {
            column,
            to: Some(s),
            ..
        } => [(column.clone(), (s.as_str(), None))].into(),
        Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::AlterColumnDefault { to: None, .. } => BTreeMap::new(),
    }
}

fn validate(p: &PlannedChange) -> Result<(), DialectError> {
    let expected = declared(p);
    if expected.len() != p.default_resolutions.len() {
        return Err(invalid(
            "default plan context is missing or contains extra entries; regenerate the plan",
        ));
    }
    for (at, (source, ty)) in expected {
        let Some(d) = p.default_resolutions.get(&at) else {
            return Err(invalid(format!("default plan context is missing {at}")));
        };
        if source != d.source || ty.is_some_and(|ty| ty != &d.column_type) {
            return Err(invalid(format!("default plan context does not match {at}")));
        }
        crate::types::normalize(&d.column_type)?;
    }
    Ok(())
}

fn needs_resolution(ty: &ColumnType, source: &str) -> Result<bool, DialectError> {
    Ok(
        crate::emit::SETTING_SENSITIVE.contains(&crate::types::normalize(ty)?.base.as_str())
            && crate::rows::is_constant(source),
    )
}

pub(crate) fn emit(
    pg: &crate::Postgres,
    p: &PlannedChange,
) -> Result<Vec<Statement>, DialectError> {
    validate(p)?;
    let mut change = p.change.clone();
    for (at, d) in &p.default_resolutions {
        match (&d.resolution, needs_resolution(&d.column_type, &d.source)?) {
            (DefaultResolution::Unresolved, true) => {
                return Err(invalid(format!(
                    "default for {at} needs canonical engine resolution; use plan --db or bootstrap --db"
                )));
            }
            (DefaultResolution::Canonical { .. }, false) => {
                return Err(invalid(format!("unexpected canonical default for {at}")));
            }
            (DefaultResolution::Canonical { rendered }, true) => {
                if !crate::rows::is_constant(rendered) {
                    return Err(invalid(format!(
                        "canonical default for {at} is not a literal"
                    )));
                }
                let slot = match &mut change {
                    Change::CreateTable { table, .. } => {
                        &mut table
                            .columns
                            .get_mut(&at.name)
                            .expect("validated column")
                            .default
                    }
                    Change::AddColumn { column, .. } => &mut column.default,
                    Change::AlterColumnDefault { to, .. } => to,
                    Change::DropTable { .. }
                    | Change::RenameTable { .. }
                    | Change::DropColumn { .. }
                    | Change::RenameColumn { .. }
                    | Change::AlterColumnType { .. }
                    | Change::AlterColumnNullability { .. }
                    | Change::SetColumnDeprecated { .. }
                    | Change::SetPrimaryKey { .. }
                    | Change::AddUnique { .. }
                    | Change::DropUnique { .. }
                    | Change::AddForeignKey { .. }
                    | Change::DropForeignKey { .. }
                    | Change::AddCheck { .. }
                    | Change::DropCheck { .. }
                    | Change::AddIndex { .. }
                    | Change::DropIndex { .. }
                    | Change::InsertRow { .. }
                    | Change::UpdateRow { .. }
                    | Change::DeleteRow { .. }
                    | Change::SetDataMode { .. }
                    | Change::CreateModule { .. }
                    | Change::AlterModule { .. }
                    | Change::DropModule { .. }
                    | Change::CreateRole { .. }
                    | Change::DropRole { .. }
                    | Change::RenameRole { .. }
                    | Change::Grant { .. }
                    | Change::Revoke { .. } => unreachable!("validated default-bearing change"),
                };
                *slot = Some(rendered.clone());
            }
            (DefaultResolution::Unresolved, false) => {}
        }
    }
    crate::emit::emit_resolved(pg, &change, p.strategy, true)
}

/// Qualify the built-in type rather than letting a temporary type capture it.
/// Arbitrary type input functions are not authorized by literal resolution.
fn builtin(ty: &str) -> Result<String, DialectError> {
    let lower = ty.to_ascii_lowercase();
    let ty = lower.as_str();
    let ty = ty.strip_prefix("pg_catalog.").unwrap_or(ty);
    if let Some(fields) = ty.strip_prefix("interval ")
        && crate::rows::is_an_interval_qualifier(fields)
    {
        return Ok(format!("pg_catalog.interval {fields}"));
    }
    let ty: ColumnType = ty
        .parse()
        .map_err(|e| invalid(format!("cannot resolve literal type: {e}")))?;
    let ty = crate::types::normalize(&ty)?;
    let name = match ty.base.as_str() {
        "date" => "date",
        "time without time zone" => "time",
        "time with time zone" => "timetz",
        "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "interval" => "interval",
        "real" => "float4",
        "double precision" => "float8",
        "text" => "text",
        "character varying" => "varchar",
        "character" => "bpchar",
        "smallint" => "int2",
        "integer" => "int4",
        "bigint" => "int8",
        "numeric" => "numeric",
        "boolean" => "bool",
        _ => {
            return Err(invalid(format!(
                "literal resolution does not execute input functions for {ty}"
            )));
        }
    };
    let args = if ty.args.is_empty() {
        String::new()
    } else {
        format!(
            "({})",
            ty.args
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    Ok(format!("pg_catalog.{name}{args}"))
}

/// Reconstruct only literal/cast syntax, never evaluate the broad constant
/// classifier's arbitrary user-defined casts (issue #319 is separate).
fn input(source: &str) -> Result<String, DialectError> {
    let s = crate::emit::without_trailing_trivia(crate::emit::after_the_gap(source).0).trim();
    if let Some((inner, ty)) =
        crate::rows::without_a_cast_typed(s).or_else(|| crate::rows::without_a_cast_call_typed(s))
    {
        return Ok(format!("({})::{}", input(inner)?, builtin(&ty)?));
    }
    if crate::emit::is_a_bare_literal(s)
        || crate::rows::is_a_number(s)
        || ["null", "true", "false"]
            .iter()
            .any(|v| s.eq_ignore_ascii_case(v))
    {
        return Ok(format!("{s}\n"));
    }
    if let Some(inner) = s.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        return Ok(format!("({})", input(inner)?));
    }
    if let Some((ty, literal)) = crate::rows::typed_literal_parts(s) {
        return Ok(format!("({literal}\n)::{}", builtin(&ty)?));
    }
    if let Some(inner) = s.strip_prefix(['+', '-']) {
        return Ok(format!("{}({})", &s[..1], input(inner)?));
    }
    Err(invalid(
        "literal resolution requires built-in literal/cast syntax",
    ))
}

/// Resolve before SQL emission and save the answer beside the original text.
/// This function owns its read-only scope; it never commits a caller's work.
pub async fn resolve(conn: &mut pbps_db::Conn, cs: &mut ChangeSet) -> Result<(), ResolveError> {
    for p in &cs.changes {
        validate(p)?;
    }
    if !cs.changes.iter().any(|p| {
        p.default_resolutions
            .values()
            .any(|d| needs_resolution(&d.column_type, &d.source).unwrap_or(false))
    }) {
        return Ok(());
    }
    crate::catalog::refuse_a_caller_owned_transaction(conn).await?;
    conn.execute(crate::catalog::BEGIN).await?;
    let result = async {
        conn.execute(crate::catalog::CANONICAL_PATH).await?;
        conn.execute("SET LOCAL timezone_abbreviations = 'Default'").await?;
        let mut resolved = cs.clone();
        for p in &mut resolved.changes {
            for d in p.default_resolutions.values_mut() {
                if !needs_resolution(&d.column_type, &d.source)? { continue; }
                let ty = builtin(&d.column_type.to_string())?;
                let expression = input(&d.source)?;
                let sql = format!("SELECT pg_catalog.quote_nullable((({expression})::{ty})::pg_catalog.text) AS value");
                let rows = conn.query(&sql).await?;
                let value = rows.first().ok_or_else(|| invalid("literal resolution returned no row"))?
                    .try_get::<&str>("value")?.ok_or_else(|| invalid("literal resolution returned NULL"))?;
                d.resolution = DefaultResolution::Canonical { rendered: format!("{value}::{}", crate::types::normalize(&d.column_type)?) };
            }
        }
        Ok::<_, ResolveError>(resolved)
    }.await;
    let rollback = conn.execute("ROLLBACK").await;
    let resolved = result?;
    rollback?;
    *cs = resolved;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_dialect::Dialect;

    fn add(ty: &str, source: &str) -> PlannedChange {
        PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: "public.t".parse().unwrap(),
            name: "d".into(),
            column: Box::new(pbps_model::Column {
                ty: ty.parse().unwrap(),
                default: Some(source.into()),
                ..pbps_model::Column::new("integer".parse().unwrap())
            }),
        })
    }

    #[test]
    fn typed_literals_require_resolution_but_expressions_and_other_types_do_not() {
        let pg = crate::Postgres::new();
        for source in [
            "'01/02/2026'",
            "'01/02/2026'::date",
            "DATE '01/02/2026'",
            "CAST('01/02/2026' AS date)",
            "'2026-01-02'::date",
        ] {
            assert!(pg.emit_planned(&add("date", source)).is_err(), "{source}");
        }
        for (ty, source) in [
            ("date", "CURRENT_DATE"),
            ("integer", "42"),
            ("text", "'hello'"),
        ] {
            assert!(pg.emit_planned(&add(ty, source)).is_ok(), "{ty} {source}");
        }
    }

    #[test]
    fn saved_resolution_emits_canonical_text_but_records_the_declaration() {
        let mut p = add("date", "'01/02/2026'::date");
        p.default_resolutions
            .values_mut()
            .next()
            .unwrap()
            .resolution = DefaultResolution::Canonical {
            rendered: "'2026-01-02'::date".into(),
        };
        let cs = ChangeSet { changes: vec![p] };
        let json = serde_json::to_string(&cs).unwrap();
        let replay: ChangeSet = serde_json::from_str(&json).unwrap();
        assert_eq!(cs, replay);
        let sql = crate::Postgres::new()
            .emit_planned(&replay.changes[0])
            .unwrap();
        assert!(format!("{sql:?}").contains("'2026-01-02'::date"));
        let mut ledger = pbps_model::Declared::default();
        ledger.advance(&replay);
        assert_eq!(
            ledger.expressions.defaults[&"public.t".parse().unwrap()]["d"],
            "'01/02/2026'::date"
        );
    }

    #[test]
    fn missing_extra_wrong_table_and_stale_context_are_refused() {
        let pg = crate::Postgres::new();
        let p = add("date", "'01/02/2026'::date");
        let mut missing = p.clone();
        missing.default_resolutions.clear();
        assert!(pg.emit_planned(&missing).is_err());
        let mut wrong = p.clone();
        let (_, d) = wrong.default_resolutions.pop_first().unwrap();
        wrong
            .default_resolutions
            .insert("other.t.d".parse().unwrap(), d);
        assert!(pg.emit_planned(&wrong).is_err());
        let mut extra = p.clone();
        extra.default_resolutions.insert(
            "public.t.other".parse().unwrap(),
            p.default_resolutions.values().next().unwrap().clone(),
        );
        assert!(pg.emit_planned(&extra).is_err());
        for field in ["source", "type"] {
            let mut stale = p.clone();
            let d = stale.default_resolutions.values_mut().next().unwrap();
            if field == "source" {
                d.source = "'other'".into();
            } else {
                d.column_type = "text".parse().unwrap();
            }
            assert!(pg.emit_planned(&stale).is_err());
        }
    }

    #[test]
    fn resolver_reconstructs_only_builtin_literal_syntax() {
        for source in [
            "'01/02/2026'::date",
            "DATE '01/02/2026'",
            "CAST('01/02/2026' AS date)",
            "(E'01/02/2026')",
            "INTERVAL '1' DAY",
            "-1",
        ] {
            assert!(input(source).is_ok(), "{source}");
        }
        for source in [
            "nextval('s')",
            "'x'::public.custom_type",
            "CAST('x' AS public.custom_type)",
            "'x' || dangerous()",
            "1; SELECT dangerous()",
        ] {
            assert!(input(source).is_err(), "{source}");
        }
    }
}

//! Recorded conversions (`pbps convert`, #2331, DEC-2331.1).
//!
//! A column type change the engine will not make by itself is refused, because
//! the cast would be a data transformation nobody declared (ADR-0012 section 5).
//! The declared alternative is an expression a person wrote, recorded the way a
//! rename is: answered once, kept in the identity file by the column's uid, and
//! dropped by the tool when it stops applying. Nothing is added to the
//! declarations, so there is nothing for the author to delete.
//!
//! This layer only decides what is recorded and when a record is stale. Whether
//! a plan uses one is the differ's question, asked of the change it is making.

use pbps_dialect::Dialect;
use pbps_model::{Change, ColumnRef, ColumnType, Conversion, IdsFile, PlannedChange, Schema, Uid};

use crate::{Context, Side};

/// Why a conversion cannot be recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot record a conversion for {column}: {why}", column = .column.shown())]
pub struct ConversionError {
    pub column: ColumnRef,
    pub why: String,
}

fn refused(column: &ColumnRef, why: impl Into<String>) -> ConversionError {
    ConversionError {
        column: column.clone(),
        why: why.into(),
    }
}

/// The type in the dialect's canonical spelling, the one the differ compares
/// and a record is written in, so `varchar(20)` and `character varying(20)`
/// are one record. A type the dialect rejects is not recorded under a guess.
fn canonical(
    column: &ColumnRef,
    ty: &ColumnType,
    dialect: &(impl Dialect + ?Sized),
) -> Result<String, ConversionError> {
    dialect
        .normalize_type(ty)
        .map(|ty| ty.raw().to_string())
        .map_err(|e| refused(column, e.to_string()))
}

fn check_expression(
    column: &ColumnRef,
    using: &str,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), ConversionError> {
    if using.trim().is_empty() {
        return Err(refused(column, "the expression is blank"));
    }
    if !dialect.lexicon().parentheses_balanced(using) {
        return Err(refused(
            column,
            "the USING expression has unbalanced parentheses; write one complete expression and re-record with `pbps convert <column> --using -`",
        ));
    }
    Ok(())
}

/// Validate only conversions this operation consumes, including rebuild copies.
/// Preview planning need not emit SQL, so it asks the same question explicitly.
pub fn check_change(
    change: &Change,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), ConversionError> {
    let mut expressions = Vec::new();
    if let Change::AlterColumnType {
        column,
        using: Some(using),
        ..
    } = change
    {
        expressions.push((column, using.as_str()));
    }
    if let Change::CopyTableRows(copy) = change {
        expressions.extend(copy.columns.iter().flat_map(|column| {
            column
                .stages
                .iter()
                .filter_map(|stage| stage.using.as_deref().map(|using| (&column.target, using)))
        }));
    }
    for (column, using) in expressions {
        check_expression(column, using, dialect)?;
    }
    Ok(())
}

fn declared_type<'a>(schema: &'a Schema, column: &ColumnRef) -> Option<&'a ColumnType> {
    schema
        .tables
        .get(&column.table)
        .and_then(|t| t.columns.get(&column.name))
        .map(|c| &c.ty)
}

/// Records `using` as the conversion of `column` from its type at `base` (or
/// `from`, which names another starting type) to the type `declared` gives it.
///
/// Offline: it reads the two sides' declarations and identities and no
/// database. Recording the same conversion again keeps the record, so the
/// command converges; another expression or start type replaces it, since one
/// column has one record.
#[allow(clippy::too_many_arguments)]
pub fn record(
    base: Side<'_>,
    declared: &Schema,
    ids: &IdsFile,
    column: &ColumnRef,
    using: &str,
    from: Option<&str>,
    ctx: &Context,
    dialect: &(impl Dialect + ?Sized),
) -> Result<IdsFile, ConversionError> {
    if !dialect.records_conversions() {
        return Err(refused(
            column,
            "this engine has no `USING` clause and converts a column's values itself \
             (ADR-0012 section 5)",
        ));
    }
    check_expression(column, using, dialect)?;
    let Some(uid) = ids.column_uid(column) else {
        return Err(refused(
            column,
            "no identity records this column; run `pbps plan` first, or check the name",
        ));
    };
    let Some(to) = declared_type(declared, column) else {
        return Err(refused(column, "the declarations do not hold this column"));
    };
    let to = canonical(column, to, dialect)?;
    let explicit_from = from;
    let from = match from {
        Some(text) => {
            let ty: ColumnType = text
                .parse()
                .map_err(|e| refused(column, format!("`--from` is not a type: {e}")))?;
            canonical(column, &ty, dialect)?
        }
        None => {
            let Some(before) = base
                .ids
                .columns
                .get(uid)
                .and_then(|was| declared_type(base.schema, was))
            else {
                return Err(refused(
                    column,
                    "the base declarations do not hold this column; name its starting type \
                     with `--from`",
                ));
            };
            canonical(column, before, dialect)?
        }
    };
    // The command is run again once the change has been committed: the base
    // is then the declared type, and the record already says what was meant.
    if from == to
        && explicit_from.is_none()
        && ids
            .conversions
            .get(uid)
            .is_some_and(|c| c.to == to && c.using == using)
    {
        return Ok(ids.clone());
    }
    if from == to {
        return Err(refused(
            column,
            format!(
                "the column is `{}` at the base and in the declarations, so there is nothing \
                 to convert; name another starting type with `--from`",
                pbps_model::escape::Escaped(&to)
            ),
        ));
    }
    let mut updated = ids.clone();
    let kept = updated
        .conversions
        .get(uid)
        .is_some_and(|c| c.from == from && c.to == to && c.using == using);
    if !kept {
        updated.conversions.insert(
            uid.clone(),
            Conversion {
                from,
                to,
                using: using.to_owned(),
                allowed_at: ctx.today.clone(),
                operator: ctx.operator.clone(),
            },
        );
    }
    Ok(updated)
}

/// Drops the records that no longer apply: a column whose declared type is not
/// the record's `to` any more.
///
/// A record is a fact about one change, so a later change makes it history. A
/// round trip such as `integer -> text -> integer` must not revive an old
/// expression for a start type nobody wrote it for, and an environment still
/// on the old type is then refused by name until it has a record for the new
/// pair. A column the identities no longer hold is dropped by resolution
/// itself. A declared type the dialect cannot read keeps its record, because
/// that is an error elsewhere and pruning on it would erase intent over a typo.
pub fn prune(ids: &mut IdsFile, declared: &Schema, dialect: &(impl Dialect + ?Sized)) {
    // An engine that records none keeps what it finds, so that the differ
    // refuses it by name rather than the tool quietly erasing intent.
    if !dialect.records_conversions() {
        return;
    }
    let columns = &ids.columns;
    ids.conversions.retain(|uid, record| {
        let Some(column) = columns.get(uid) else {
            return false;
        };
        let Some(ty) = declared_type(declared, column) else {
            return false;
        };
        match dialect.normalize_type(ty) {
            Ok(ty) => ty.raw().to_string() == record.to,
            Err(_) => true,
        }
    });
}

/// A retype the engine refuses for want of a recorded conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needed {
    pub uid: Uid,
    pub column: ColumnRef,
    /// Both ends canonical, as a record spells them.
    pub from: String,
    pub to: String,
}

/// The changes that need a conversion and have none: what the plan asks a
/// person for, with a terminal, or reports with the command to run, without.
///
/// Asked of the dialect and not guessed from the types, so the question is the
/// emitter's own: it refuses exactly these.
pub fn needed(changes: &[PlannedChange], dialect: &(impl Dialect + ?Sized)) -> Vec<Needed> {
    changes
        .iter()
        .filter_map(|p| {
            let Change::AlterColumnType {
                uid,
                column,
                from,
                to,
                using: None,
                ..
            } = &p.change
            else {
                return None;
            };
            p.change
                .value_change()
                .map_or_else(
                    || dialect.type_change_needs_using(from, to),
                    |values| dialect.value_change(&values, "").needs_using.is_some(),
                )
                .then(|| Needed {
                    uid: uid.clone(),
                    column: column.clone(),
                    from: from.raw().to_string(),
                    to: to.raw().to_string(),
                })
        })
        .collect()
}

/// Records what a person answered for a change [`needed`] named: the
/// expression for exactly that `from` to `to`.
pub fn answer(
    ids: &mut IdsFile,
    need: &Needed,
    using: &str,
    ctx: &Context,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), ConversionError> {
    check_expression(&need.column, using, dialect)?;
    ids.conversions.insert(
        need.uid.clone(),
        Conversion {
            from: need.from.clone(),
            to: need.to.clone(),
            using: using.to_owned(),
            allowed_at: ctx.today.clone(),
            operator: ctx.operator.clone(),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiffError, diff};
    use pbps_dialect::{Dialect, DialectError, MinimalDialect};
    use pbps_model::{Column, Hints, Table, TableName};

    /// `MinimalDialect` that records conversions, as PostgreSQL does, and
    /// needs one wherever the engine would refuse the change outright. `int`
    /// and `integer` are one type to it, and `varchar(10)` and
    /// `character varying(10)` another, to show records are written in the
    /// dialect's spelling.
    struct Converts;

    impl Dialect for Converts {
        fn name(&self) -> &'static str {
            "converts"
        }
        fn records_conversions(&self) -> bool {
            true
        }
        fn type_change_needs_using(&self, from: &ColumnType, to: &ColumnType) -> bool {
            self.type_change_risk(from, to) == pbps_dialect::TypeChangeRisk::Incompatible
        }
        fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
            let mut ty = ty.clone();
            if ty.base == "int" {
                ty.base = "integer".into();
            }
            Ok(ty)
        }
        fn type_change_risk(
            &self,
            from: &ColumnType,
            to: &ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(
            &self,
            zone: &pbps_model::SessionZone,
        ) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-10-24".into(),
        }
    }

    fn amount() -> ColumnRef {
        "app.t.amount".parse().unwrap()
    }

    /// One table, `app.t`, holding `amount` of the given type.
    fn schema(ty: &str) -> Schema {
        let mut table = Table::default();
        table
            .columns
            .insert("amount".into(), Column::new(ty.parse().unwrap()));
        let mut schema = Schema::default();
        schema.tables.insert("app.t".parse().unwrap(), table);
        schema
    }

    fn ids_of(schema: &Schema) -> IdsFile {
        crate::resolve(schema, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids
    }

    fn recorded(base: &Schema, declared: &Schema, using: &str, from: Option<&str>) -> IdsFile {
        let ids = ids_of(base);
        record(
            Side {
                schema: base,
                ids: &ids,
            },
            declared,
            &ids,
            &amount(),
            using,
            from,
            &ctx(),
            &Converts,
        )
        .unwrap()
    }

    /// The identities of the environment a record was made for: the same uids,
    /// before anyone recorded anything.
    fn unrecorded(ids: &IdsFile) -> IdsFile {
        IdsFile {
            conversions: Default::default(),
            ..ids.clone()
        }
    }

    fn plan(
        base: (&Schema, &IdsFile),
        declared: (&Schema, &IdsFile),
        dialect: &(impl Dialect + ?Sized),
    ) -> Result<Vec<Change>, Vec<DiffError>> {
        diff(
            Side {
                schema: base.0,
                ids: base.1,
            },
            Side {
                schema: declared.0,
                ids: declared.1,
            },
            dialect,
            &Hints::default(),
        )
        .map(|plan| {
            plan.changes()
                .changes
                .iter()
                .map(|p| p.change.clone())
                .collect()
        })
    }

    fn using_of(changes: &[Change]) -> Vec<Option<&str>> {
        changes
            .iter()
            .filter_map(|c| {
                let Change::AlterColumnType { using, .. } = c else {
                    return None;
                };
                Some(using.as_deref())
            })
            .collect()
    }

    #[test]
    fn a_conversion_is_recorded_from_the_base_type_to_the_declared_one() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = recorded(&base, &declared, "amount::bigint", None);
        let uid = ids.column_uid(&amount()).unwrap();
        let record = &ids.conversions[uid];
        assert_eq!(
            (
                record.from.as_str(),
                record.to.as_str(),
                record.using.as_str()
            ),
            ("varchar(10)", "bigint", "amount::bigint")
        );
        assert_eq!(
            (&*record.allowed_at, &*record.operator),
            ("2026-10-24", "leon")
        );
        ids.validate().unwrap();
    }

    #[test]
    fn a_type_is_recorded_in_the_dialects_spelling() {
        let (base, declared) = (schema("int"), schema("varchar(10)"));
        let ids = recorded(&base, &declared, "amount::varchar", None);
        assert_eq!(ids.conversions.values().next().unwrap().from, "integer");
        let named = recorded(&base, &declared, "amount::varchar", Some("INT"));
        assert_eq!(named.conversions.values().next().unwrap().from, "integer");
    }

    #[test]
    fn from_names_another_starting_type_and_recording_twice_changes_nothing() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let once = recorded(&base, &declared, "amount::bigint", Some("varchar(20)"));
        assert_eq!(
            once.conversions.values().next().unwrap().from,
            "varchar(20)"
        );
        let again = record(
            Side {
                schema: &base,
                ids: &once,
            },
            &declared,
            &once,
            &amount(),
            "amount::bigint",
            Some("varchar(20)"),
            &Context {
                today: "2030-01-01".into(),
                ..ctx()
            },
            &Converts,
        )
        .unwrap();
        assert_eq!(again, once, "the same record keeps its date and operator");
        let replaced = recorded(&base, &declared, "trim(amount)::bigint", None);
        assert_eq!(replaced.conversions.len(), 1, "one record per column");
    }

    #[test]
    fn what_cannot_be_recorded_is_refused_and_names_why() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = ids_of(&base);
        let go = |declared: &Schema,
                  ids: &IdsFile,
                  column: &ColumnRef,
                  using: &str,
                  from: Option<&str>,
                  dialect: &dyn Dialect| {
            record(
                Side { schema: &base, ids },
                declared,
                ids,
                column,
                using,
                from,
                &ctx(),
                dialect,
            )
            .unwrap_err()
            .why
        };
        let missing: ColumnRef = "app.t.missing".parse().unwrap();
        assert!(go(&declared, &ids, &amount(), " \t", None, &Converts).contains("blank"));
        assert!(go(&declared, &ids, &missing, "x", None, &Converts).contains("no identity"));
        assert!(
            go(&declared, &ids, &amount(), "x", None, &MinimalDialect).contains("no `USING`"),
            "an engine without the clause records nothing"
        );
        assert!(
            go(&base, &ids, &amount(), "x", None, &Converts).contains("nothing to convert"),
            "a column that keeps its type has no conversion"
        );
        assert!(
            go(&declared, &ids, &amount(), "x", Some("bigint"), &Converts)
                .contains("nothing to convert")
        );
        assert!(
            go(&declared, &ids, &amount(), "x", Some("("), &Converts).contains("not a type"),
            "an unparseable --from is not recorded under a guess"
        );
        let mut unknown = declared.clone();
        unknown.tables.values_mut().next().unwrap().columns.clear();
        assert!(go(&unknown, &ids, &amount(), "x", None, &Converts).contains("declarations"));
        // A column the base never held needs its start named.
        let mut grown = ids.clone();
        grown.columns.insert(
            pbps_model::Uid::generate(pbps_model::UidKind::Column),
            "app.t.fresh".parse().unwrap(),
        );
        let mut with_fresh = declared.clone();
        with_fresh
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .insert("fresh".into(), Column::new("text".parse().unwrap()));
        let fresh: ColumnRef = "app.t.fresh".parse().unwrap();
        assert!(go(&with_fresh, &grown, &fresh, "x", None, &Converts).contains("--from"));
    }

    #[test]
    fn the_differ_gives_the_change_its_expression_only_for_the_recorded_pair() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let declared_ids = recorded(&base, &declared, "amount::bigint", None);
        let base_ids = unrecorded(&declared_ids);
        let changes = plan((&base, &base_ids), (&declared, &declared_ids), &Converts).unwrap();
        assert_eq!(using_of(&changes), [Some("amount::bigint")]);
        // An environment already on the declared type has no change, so it
        // never reads the record.
        assert!(
            plan(
                (&declared, &declared_ids),
                (&declared, &declared_ids),
                &Converts
            )
            .unwrap()
            .is_empty()
        );
        // And a change with no record carries no expression.
        let bare = plan((&base, &base_ids), (&declared, &base_ids), &Converts).unwrap();
        assert_eq!(using_of(&bare), [None]);
    }

    #[test]
    fn a_start_the_record_was_not_written_for_is_refused_by_name() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let declared_ids = recorded(&base, &declared, "amount::bigint", None);
        let other = schema("varchar(20)");
        let other_ids = ids_of(&other);
        // The same uids on the base side: it is the same column elsewhere.
        let mut as_base = declared_ids.clone();
        as_base.conversions.clear();
        let errors = plan((&other, &as_base), (&declared, &declared_ids), &Converts).unwrap_err();
        let [
            DiffError::ConversionFromMismatch {
                recorded,
                actual,
                to,
                ..
            },
        ] = &errors[..]
        else {
            panic!("{errors:?}");
        };
        assert_eq!(
            (recorded.as_str(), actual.as_str(), to.as_str()),
            ("varchar(10)", "varchar(20)", "bigint")
        );
        let message = errors[0].to_string();
        assert!(
            message.contains("pbps convert app.t.amount --using - --from 'varchar(20)'"),
            "{message}"
        );
        let _ = other_ids;
    }

    #[test]
    fn a_start_the_engine_converts_itself_leaves_the_record_unused() {
        // varchar(10) -> varchar(20) needs no expression, so a record written
        // for another start is neither applied nor an error.
        let base = schema("varchar(10)");
        let declared = schema("varchar(20)");
        let mut ids = ids_of(&declared);
        let uid = ids.column_uid(&amount()).unwrap().clone();
        ids.conversions.insert(
            uid,
            Conversion {
                from: "text".into(),
                to: "varchar(20)".into(),
                using: "amount::varchar(20)".into(),
                allowed_at: "2026-10-24".into(),
                operator: "leon".into(),
            },
        );
        let base_ids = {
            let mut b = ids.clone();
            b.conversions.clear();
            b
        };
        let changes = plan((&base, &base_ids), (&declared, &ids), &Converts).unwrap();
        assert_eq!(using_of(&changes), [None]);
    }

    #[test]
    fn a_column_renamed_in_the_same_plan_is_refused() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let mut renamed = declared.clone();
        let table = renamed.tables.values_mut().next().unwrap();
        let column = table.columns.shift_remove("amount").unwrap();
        table.columns.insert("total".into(), column);
        let mut ids = recorded(&base, &declared, "amount::bigint", None);
        let uid = ids.column_uid(&amount()).unwrap().clone();
        let base_ids = {
            let mut b = ids.clone();
            b.conversions.clear();
            b
        };
        ids.columns.insert(uid, "app.t.total".parse().unwrap());
        let errors = plan((&base, &base_ids), (&renamed, &ids), &Converts).unwrap_err();
        assert!(
            matches!(&errors[..], [DiffError::ConversionWithRename { .. }]),
            "{errors:?}"
        );
    }

    #[test]
    fn a_conversion_is_refused_on_an_engine_without_using() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let declared_ids = recorded(&base, &declared, "amount::bigint", None);
        let base_ids = unrecorded(&declared_ids);
        let errors = plan(
            (&base, &base_ids),
            (&declared, &declared_ids),
            &MinimalDialect,
        )
        .unwrap_err();
        assert!(
            matches!(
                &errors[..],
                [DiffError::ConversionsUnsupported { count: 1 }]
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn a_record_goes_when_the_declared_type_is_no_longer_its_target() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = recorded(&base, &declared, "amount::bigint", None);
        let mut kept = ids.clone();
        prune(&mut kept, &declared, &Converts);
        assert_eq!(kept, ids, "still the declared type: kept");
        // The declared type moves on, and a round trip never revives it.
        for moved in ["text", "integer", "varchar(10)"] {
            let mut pruned = ids.clone();
            prune(&mut pruned, &schema(moved), &Converts);
            assert!(pruned.conversions.is_empty(), "{moved}");
            pruned.validate().unwrap();
        }
        let mut again = ids.clone();
        prune(&mut again, &schema("text"), &Converts);
        prune(&mut again, &declared, &Converts);
        assert!(again.conversions.is_empty());
        // The same target under another spelling is still the target.
        let spelled = recorded(&schema("varchar(5)"), &schema("int"), "amount::int", None);
        let mut same = spelled.clone();
        prune(&mut same, &schema("INT"), &Converts);
        assert_eq!(same, spelled);
        // A declared type nobody can read is an error elsewhere, not a reason
        // to erase intent.
        let mut unreadable = ids.clone();
        let mut broken = declared.clone();
        broken.tables.values_mut().next().unwrap().columns.clear();
        prune(&mut unreadable, &broken, &Converts);
        assert!(unreadable.conversions.is_empty(), "a dropped column goes");
    }

    #[test]
    fn dropping_a_column_takes_its_record_with_it() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = recorded(&base, &declared, "amount::bigint", None);
        let mut without_column = declared.clone();
        without_column
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .clear();
        let drop = pbps_model::Intent::DropColumn {
            column: amount(),
            reason: "retired".parse().unwrap(),
        };
        let resolved = crate::resolve(&without_column, &ids, &[drop], &ctx()).unwrap();
        assert!(resolved.ids.conversions.is_empty());
        resolved.ids.validate().unwrap();
    }

    #[test]
    fn only_a_change_the_engine_refuses_and_nothing_records_is_needed() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let base_ids = ids_of(&base);
        let bare = plan((&base, &base_ids), (&declared, &base_ids), &Converts).unwrap();
        let planned: Vec<PlannedChange> = bare.into_iter().map(PlannedChange::new).collect();
        let needed = needed(&planned, &Converts);
        assert_eq!(
            needed
                .iter()
                .map(|n| (n.column.shown().to_string(), n.from.as_str(), n.to.as_str()))
                .collect::<Vec<_>>(),
            [("app.t.amount".to_owned(), "varchar(10)", "bigint")]
        );
        // The engine takes a widening itself.
        let wider = plan(
            (&base, &base_ids),
            (&schema("varchar(20)"), &base_ids),
            &Converts,
        )
        .unwrap();
        let planned: Vec<PlannedChange> = wider.into_iter().map(PlannedChange::new).collect();
        assert!(super::needed(&planned, &Converts).is_empty());
        // And a change that carries its expression asks for nothing.
        let declared_ids = recorded(&base, &declared, "amount::bigint", None);
        let base_ids = unrecorded(&declared_ids);
        let done = plan((&base, &base_ids), (&declared, &declared_ids), &Converts).unwrap();
        let planned: Vec<PlannedChange> = done.into_iter().map(PlannedChange::new).collect();
        assert!(super::needed(&planned, &Converts).is_empty());
        // An engine that records none never asks.
        let planned: Vec<PlannedChange> =
            plan((&base, &base_ids), (&declared, &base_ids), &MinimalDialect)
                .unwrap()
                .into_iter()
                .map(PlannedChange::new)
                .collect();
        assert!(super::needed(&planned, &MinimalDialect).is_empty());
    }

    #[test]
    fn recording_and_prompt_answers_refuse_unbalanced_using_before_changing_ids() {
        let (base, declared) = (schema("text"), schema("integer"));
        let original = ids_of(&base);
        let need = Needed {
            uid: original.column_uid(&amount()).unwrap().clone(),
            column: amount(),
            from: "text".into(),
            to: "integer".into(),
        };
        for using in ["amount) IS NULL OR (amount", "(amount"] {
            let error = record(
                Side {
                    schema: &base,
                    ids: &original,
                },
                &declared,
                &original,
                &amount(),
                using,
                None,
                &ctx(),
                &Converts,
            )
            .unwrap_err();
            assert!(error.to_string().contains("has unbalanced parentheses"));
            let mut answered = original.clone();
            assert!(answer(&mut answered, &need, using, &ctx(), &Converts).is_err());
            assert_eq!(answered, original, "a refused answer records nothing");
        }
        let using = "(nullif(amount, '(' /* ) */))::integer";
        let mut answered = original;
        answer(&mut answered, &need, using, &ctx(), &Converts).unwrap();
        assert_eq!(answered.conversions[&need.uid].using, using);
    }

    #[test]
    fn answering_records_exactly_the_pair_that_was_asked_about() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let base_ids = ids_of(&base);
        let bare = plan((&base, &base_ids), (&declared, &base_ids), &Converts).unwrap();
        let planned: Vec<PlannedChange> = bare.into_iter().map(PlannedChange::new).collect();
        let asked = needed(&planned, &Converts);
        let mut ids = base_ids.clone();
        answer(&mut ids, &asked[0], "amount::bigint", &ctx(), &Converts).unwrap();
        ids.validate().unwrap();
        let changes = plan((&base, &base_ids), (&declared, &ids), &Converts).unwrap();
        assert_eq!(using_of(&changes), [Some("amount::bigint")]);
    }

    #[test]
    fn recording_again_once_the_change_is_committed_keeps_the_record_and_nothing_else() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = recorded(&base, &declared, "amount::bigint", None);
        let at_head = |using: &str| {
            record(
                Side {
                    schema: &declared,
                    ids: &ids,
                },
                &declared,
                &ids,
                &amount(),
                using,
                None,
                &ctx(),
                &Converts,
            )
        };
        assert_eq!(at_head("amount::bigint").unwrap(), ids);
        // A different expression has no base type to start from any more.
        assert!(
            at_head("trim(amount)::bigint")
                .unwrap_err()
                .why
                .contains("nothing to convert")
        );
    }

    #[test]
    fn an_engine_that_records_none_keeps_what_it_finds_for_the_differ_to_refuse() {
        let (base, declared) = (schema("varchar(10)"), schema("bigint"));
        let ids = recorded(&base, &declared, "amount::bigint", None);
        let mut kept = ids.clone();
        prune(&mut kept, &schema("text"), &MinimalDialect);
        assert_eq!(kept, ids);
    }
}

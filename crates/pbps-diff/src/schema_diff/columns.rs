//! Column changes of a table that stays.

use super::*;

/// Columns of a surviving table that exist on both sides: compare attributes.
#[allow(clippy::too_many_arguments)]
pub(super) fn diff_columns(
    index: &Indexes<'_>,
    types: &TypeResolver<'_>,
    base_table_name: &TableName,
    declared_table_name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    dialect: &(impl Dialect + ?Sized),
    conversions: &BTreeMap<Uid, pbps_model::Conversion>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let base_cols = columns_of(&index.base, base_table_name);
    let declared_cols = columns_of(&index.declared, declared_table_name);

    for (uid, declared_ref) in &declared_cols {
        let Some(base_ref) = base_cols.get(uid) else {
            // Present only on the declared side: a new column.
            if let Some(c) = declared_table.columns.get(&declared_ref.name) {
                // The change does not carry the table's persistence, and the
                // emitter creates the sequence logged (#1631).
                if let ValueSource::Sequence(_) = &c.value_source
                    && declared_table.unlogged
                {
                    errs.push(DiffError::SequenceChangeUnsupported {
                        column: declared_ref.clone(),
                    });
                }
                changes.push(Change::Column(ColumnChange::AddColumn {
                    uid: uid.clone(),
                    table: declared_table_name.clone(),
                    name: declared_ref.name.clone(),
                    column: Box::new(c.clone()),
                }));
            }
            continue;
        };

        if base_ref.name != declared_ref.name {
            changes.push(Change::Column(ColumnChange::RenameColumn {
                uid: uid.clone(),
                table: declared_table_name.clone(),
                from: base_ref.name.clone(),
                to: declared_ref.name.clone(),
                table_was: (base_table_name != declared_table_name)
                    .then(|| base_table_name.clone()),
            }));
        }

        let (Some(base_col), Some(col)) = (
            base_table.columns.get(&base_ref.name),
            declared_table.columns.get(&declared_ref.name),
        ) else {
            continue;
        };

        let norm = |t: &ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (mut from_ty, to_ty) = (norm(&base_col.ty), norm(&col.ty));
        if let (Some(old), Some(new)) = (from_ty.type_ref(), to_ty.type_ref())
            && types.same_declared(old, new)
            && from_ty.is_array() == to_ty.is_array()
        {
            from_ty = to_ty.clone();
        }
        let mut recomputed = false;
        // A numbering source changed in place (#1889, DEC-1889.1), where the
        // engine has an `ALTER` for it and the table takes it. A parent's
        // serial default recurses into its partitions (DEC-2219.1); an
        // identity still needs its partition handling (#1681). An owned
        // sequence is created logged,
        // which an `UNLOGGED` table's would not be (#1631).
        //
        // A plain default on either side is not the change's: one given up
        // or taken is an `AlterColumnDefault` beside it, from or to nothing,
        // so it is ordered as every default is, around a retype and after
        // the functions it calls (#1930 review).
        let numbering = |from: &ValueSource,
                         to: &ValueSource,
                         errs: &mut Vec<DiffError>,
                         changes: &mut Vec<Change>| {
            let identity =
                matches!(from, ValueSource::Identity(_)) || matches!(to, ValueSource::Identity(_));
            if !dialect.changes_numbering() {
                errs.push(if identity {
                    DiffError::IdentityChangeUnsupported {
                        column: declared_ref.clone(),
                    }
                } else {
                    DiffError::SequenceChangeUnsupported {
                        column: declared_ref.clone(),
                    }
                });
            } else if [base_table, declared_table]
                .iter()
                .any(|t| t.partition_of.is_some() || (identity && t.partition_by.is_some()))
            {
                errs.push(DiffError::NumberingChangeOnPartitionedTable {
                    column: declared_ref.clone(),
                });
            } else if matches!(to, ValueSource::Sequence(_))
                && !matches!(from, ValueSource::Sequence(_))
                && (base_table.unlogged || declared_table.unlogged)
            {
                errs.push(DiffError::SequenceChangeUnsupported {
                    column: declared_ref.clone(),
                });
            } else if let (
                ValueSource::Identity(pbps_model::Identity { increment: was, .. }),
                ValueSource::Identity(pbps_model::Identity { increment: now, .. }),
            )
            | (
                ValueSource::Sequence(pbps_model::OwnedSequence { increment: was, .. }),
                ValueSource::Sequence(pbps_model::OwnedSequence { increment: now, .. }),
            ) = (from, to)
                && (*was < 0) != (*now < 0)
            {
                // The sequence keeps the bounds of the direction it was
                // made for, which a start in the other refuses (#1930 review).
                errs.push(DiffError::NumberingDirectionChange {
                    column: declared_ref.clone(),
                });
            } else {
                changes.push(Change::Column(ColumnChange::AlterColumnNumbering {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    ty: to_ty.clone(),
                    from: from.clone(),
                    to: to.clone(),
                }));
            }
        };
        // Compare the source as one property: ordinary defaults may change,
        // generated text may change in place, a numbering source changes as
        // one change, and a change to or from generated refuses.
        let (old_default, new_default) = match (&base_col.value_source, &col.value_source) {
            (ValueSource::None, ValueSource::None) => (None, None),
            (ValueSource::Default(old), ValueSource::Default(new)) => (Some(old), Some(new)),
            (ValueSource::None, ValueSource::Default(new)) => (None, Some(new)),
            (ValueSource::Default(old), ValueSource::None) => (Some(old), None),
            (ValueSource::Identity(old), ValueSource::Identity(new)) => {
                // The engine moves an identity's sequence to a new type on
                // its own (measured on 16 and 18).
                if old != new {
                    numbering(&base_col.value_source, &col.value_source, errs, changes);
                }
                (None, None)
            }
            (ValueSource::Generated(from), ValueSource::Generated(to)) => {
                if from.stored != to.stored {
                    errs.push(DiffError::GenerationChangeUnsupported {
                        column: declared_ref.clone(),
                    });
                } else if from.expression != to.expression {
                    recomputed = true;
                    changes.push(Change::Column(ColumnChange::AlterColumnExpression {
                        uid: uid.clone(),
                        column: declared_ref.clone(),
                        from: from.expression.clone(),
                        to: to.expression.clone(),
                    }));
                }
                (None, None)
            }
            (ValueSource::Generated(_), ValueSource::None | ValueSource::Default(_))
            | (ValueSource::None | ValueSource::Default(_), ValueSource::Generated(_)) => {
                errs.push(DiffError::GenerationChangeUnsupported {
                    column: declared_ref.clone(),
                });
                (None, None)
            }
            // A name is compared only where both sides give one. The
            // declaration's may be left out (`OwnedSequence::declares`), and
            // a base without one does not know it: a git baseline records
            // the name an earlier declaration generated, under whatever table
            // and column it was declared on then, and a rename keeps the
            // sequence's. So a renamed table may name the sequence it kept,
            // and a later pull's name for it is no change. The apply's own
            // read compares the name the engine holds. A retype moves the
            // sequence to the new type, which the engine does not do for an
            // owned sequence (measured on 16 and 18), and which the reader
            // holds a `serial` to (#1631).
            (ValueSource::Sequence(from), ValueSource::Sequence(to)) => {
                let from = pbps_model::OwnedSequence {
                    name: from.name.clone().or_else(|| to.name.clone()),
                    ..from.clone()
                };
                if !to.declares(&from) || from_ty != to_ty {
                    numbering(&base_col.value_source, &col.value_source, errs, changes);
                }
                (None, None)
            }
            (
                ValueSource::Identity(_) | ValueSource::Sequence(_),
                ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_),
            )
            | (ValueSource::None, ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                numbering(&base_col.value_source, &col.value_source, errs, changes);
                (None, None)
            }
            (ValueSource::Identity(_) | ValueSource::Sequence(_), ValueSource::Default(new)) => {
                numbering(&base_col.value_source, &ValueSource::None, errs, changes);
                (None, Some(new))
            }
            (ValueSource::Default(old), ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                numbering(&ValueSource::None, &col.value_source, errs, changes);
                (Some(old), None)
            }
            (ValueSource::Identity(_) | ValueSource::Sequence(_), ValueSource::Generated(_))
            | (ValueSource::Generated(_), ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                errs.push(DiffError::GenerationChangeUnsupported {
                    column: declared_ref.clone(),
                });
                (None, None)
            }
        };

        // A collation change is the same `ALTER COLUMN` as a type change, and
        // takes down the same dependents (#1175), so it is carried by the
        // same change with the type restated.
        let recollated = base_col.collation != col.collation;
        let retyped = from_ty != to_ty || recollated;
        // A type change subsumes a nullability change rather than sitting beside
        // one: `ALTER COLUMN` restates the whole definition, so two changes would
        // mean two statements where the second undoes half of the first.
        //
        // Except a tightening of a recomputed column. The type goes before the
        // new expression, so the values are computed in the final type, and
        // `NOT NULL` after it, so it is checked against the new values rather
        // than the old expression's (DEC-1168.1).
        let tightened_after = recomputed && base_col.nullable && !col.nullable;
        // The expression a human recorded for exactly this change (#2331,
        // DEC-2331.1), and only for it: this environment's start must be the
        // recorded one. An environment already on the declared type has no
        // change and never reads the record, so each applies it once. One
        // that starts elsewhere, and needs an expression to get there, is
        // refused by name rather than converted under text written for
        // another type.
        let mut using = None;
        let spelled = |ty: &ColumnType| ty.raw().to_string();
        if let Some(record) = conversions
            .get(uid)
            .filter(|r| r.to == spelled(&to_ty) && from_ty != to_ty)
        {
            if record.from == spelled(&from_ty) {
                if base_ref.name == declared_ref.name {
                    using = Some(record.using.clone());
                } else {
                    errs.push(DiffError::ConversionWithRename {
                        column: declared_ref.clone(),
                    });
                }
            } else if pbps_model::ValueChange::cast(&from_ty, &to_ty, None).map_or_else(
                || dialect.type_change_needs_using(&from_ty, &to_ty),
                |values| dialect.value_change(&values, "").needs_using.is_some(),
            ) {
                errs.push(DiffError::ConversionFromMismatch {
                    column: declared_ref.clone(),
                    recorded: record.from.clone(),
                    actual: spelled(&from_ty),
                    to: record.to.clone(),
                });
            }
        }
        let converting = using.is_some();
        if retyped {
            changes.push(Change::Column(ColumnChange::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty.clone(),
                to: to_ty.clone(),
                from_nullable: base_col.nullable,
                to_nullable: col.nullable || tightened_after,
                through_external_partitions: false,
                from_collation: base_col.collation.clone(),
                to_collation: col.collation.clone(),
                using,
            }));
            if tightened_after {
                changes.push(Change::Column(ColumnChange::AlterColumnNullability {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    ty: to_ty,
                    to_nullable: false,
                    through_external_partitions: false,
                    collation: col.collation.clone(),
                }));
            }
        } else if base_col.nullable != col.nullable {
            changes.push(Change::Column(ColumnChange::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
                ty: to_ty,
                to_nullable: col.nullable,
                through_external_partitions: false,
                collation: col.collation.clone(),
            }));
        }
        // A default the expression's result cannot take is refused by the
        // engine at the retype -- measured on 16 and 18, `default for column
        // "c" cannot be cast automatically` -- even where its text is
        // unchanged and the old one was cast across without a word. So a
        // conversion takes the default down and puts it back, as a changed
        // one does (#2331).
        if defaults_differ(old_default, new_default) || (converting && old_default.is_some()) {
            // A default that is *replaced* on a column this same plan retypes
            // is two changes, because the type change has to run between them.
            // Measured on SQL Server, an `ALTER COLUMN` that changes the type
            // is refused while any default constraint stands — 5074, "the
            // object 'df_dn2' is dependent on column 'n'", with 4922 behind it
            // — and the nullability form of the same statement is accepted, so
            // the dependency is the type's and not `ALTER COLUMN`'s.
            //
            // Only when the type moves, and only when there is an old default
            // *and* a new one. A default replaced on a column that keeps its
            // type needs no drop: `SET DEFAULT` replaces on PostgreSQL, and
            // the SQL Server emitter already drops and adds inside its one
            // statement. Splitting it there would put two lines at opposite
            // ends of the plan where one says it better (SPEC §14.1).
            let split = retyped && old_default.is_some() && new_default.is_some();
            changes.push(Change::Column(ColumnChange::AlterColumnDefault {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: old_default.cloned(),
                to: if split { None } else { new_default.cloned() },
            }));
            if split {
                changes.push(Change::Column(ColumnChange::AlterColumnDefault {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    from: None,
                    to: new_default.cloned(),
                }));
            }
        }
        if base_col.deprecated != col.deprecated {
            changes.push(Change::Column(ColumnChange::SetColumnDeprecated {
                uid: uid.clone(),
                column: declared_ref.clone(),
                reason: col.deprecated.clone(),
            }));
        }
    }

    // Present only on the base side: a dropped column.
    for (uid, base_ref) in &base_cols {
        if !declared_cols.contains_key(uid) {
            changes.push(Change::Column(ColumnChange::DropColumn {
                uid: uid.clone(),
                // The **declared** table, with the column's base name. Only
                // the table is renamed here — a column this plan drops is
                // absent from the declarations, so it has no rename intent
                // and the database still knows it by the base name.
                //
                // `base_ref` carries the pre-rename table, and every sibling
                // in this loop is built from `declared_table_name`. Taken
                // whole it emitted `sp_rename 'dbo.customers', 'clients'`
                // and then `ALTER TABLE [dbo].[customers] DROP COLUMN ...`,
                // because `order_key` runs the rename first: a valid,
                // reviewed plan refused by the engine.
                column: declared_table_name.column(base_ref.name.clone()),
            }));
        }
    }
}

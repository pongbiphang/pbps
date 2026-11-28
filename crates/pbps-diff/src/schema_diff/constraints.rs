//! Constraints and indexes: the diff of keys, checks and index definitions, and what makes one adopt another.

use super::*;

/// Whether a new parent index can take a partition's own index as its
/// clone. Measured on 16.15 and 18.6: the method, the keys in order with
/// their classes, uniqueness, the predicate and the `INCLUDE` columns must
/// match; a key's direction, its class options (`opclass_parameters`) and
/// the storage parameters need not (#1688, DEC-2449.2).
///
/// An expression key or a predicate is a match whatever its text: the engine
/// compares what each parses and binds to, so `n+1` and `n + 1` are one index
/// to it (#1737 review). The one caller left is the refusal of a partition's
/// own index that the plan creates with the partition; what a plan can drop
/// instead is asked of [`may_adopt`] (DEC-1671.1). An expression key's class is
/// not text, though: absent is the default on both sides, since a
/// declaration may not spell it and the catalog reads it back as none
/// (DEC-2449.2), so two expression keys under different classes are told
/// apart here, as two column keys are (DEC-2961.1).
pub fn adopts(parent: &pbps_model::Index, own: &pbps_model::Index) -> bool {
    parent.method == own.method
        && parent.unique == own.unique
        && parent.filter.is_some() == own.filter.is_some()
        && parent.include == own.include
        && parent.columns.len() == own.columns.len()
        && parent.columns.iter().zip(&own.columns).all(|(p, o)| {
            match (p.key.column(), o.key.column()) {
                (Some(p_column), Some(o_column)) => p_column == o_column && p.opclass == o.opclass,
                (None, None) => p.opclass == o.opclass,
                _ => true,
            }
        })
}

/// Whether a parent's index might take a partition's own as its clone, by a
/// comparison looser than the engine's, so that no engine rule is predicted
/// (#1671, DEC-1671.1). A plan drops every own one this matches before the
/// parent's index is built or the table attached, and puts back what the
/// declaration keeps, so the engine has nothing of the plan's to adopt.
///
/// It asks only what no adoption survives a difference in: uniqueness, the
/// number of keys, and at each position the column ([`may_share_key`]). Method,
/// direction, class, collation, options, predicate, `INCLUDE` and storage
/// parameters are left out; the engine compares most of them, which only makes
/// this wider.
pub fn may_adopt(
    parent: &pbps_model::Index,
    own: &pbps_model::Index,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    parent.unique == own.unique
        && parent.columns.len() == own.columns.len()
        && parent
            .columns
            .iter()
            .zip(&own.columns)
            .all(|(p, o)| may_share_key(&p.key, &o.key, dialect))
}

/// Whether two keys at one position could be one to the engine: the same
/// column; two expressions, whatever their text; or a column and an expression
/// that may read it.
///
/// The engine stores some expression keys as column keys: measured on 16.15 and
/// 18.6, `((m))`, `(t.m)`, a cast of the column, `(s collate "C")`, `(int4(m))`
/// and `((t).m)` among them (DEC-1671.1). Which text it would reduce so is not
/// predicted. Every one names the column in its code, so the dialect's own
/// column scan ([`Dialect::generation_may_read_column`]) finds it: it reads
/// identifiers by the engine's rules, leaves literals and comments out, and
/// over-approximates, counting a call or a field of the same name, which here
/// only drops more.
pub(super) fn may_share_key(
    parent: &pbps_model::IndexKey,
    own: &pbps_model::IndexKey,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    use pbps_model::IndexKey::{Column, Expression};
    match (parent, own) {
        (Column(parent), Column(own)) => parent == own,
        (Expression(_), Expression(_)) => true,
        (Expression(text), Column(column)) | (Column(column), Expression(text)) => {
            dialect.generation_may_read_column(text, column)
        }
    }
}

/// `Some(None)` keeps a part without an edit; `None` replaces it. Keeping
/// those answers distinct lets storage parameters change without a rebuild.
fn by_name<T>(
    base: &BTreeMap<pbps_model::ObjectLocalName, T>,
    declared: &BTreeMap<pbps_model::ObjectLocalName, T>,
    kept: impl Fn(&pbps_model::ObjectLocalName, &T, &T) -> Option<Option<Change>>,
    drop: impl Fn(&pbps_model::ObjectLocalName) -> Change,
    add: impl Fn(&pbps_model::ObjectLocalName, &T) -> Change,
    changes: &mut Vec<Change>,
) {
    for (name, part) in declared {
        if let Some(edit) = base.get(name).and_then(|was| kept(name, was, part)) {
            changes.extend(edit);
        } else {
            if base.contains_key(name) {
                changes.push(drop(name));
            }
            changes.push(add(name, part));
        }
    }
    changes.extend(base.keys().filter(|n| !declared.contains_key(*n)).map(drop));
}

/// What a difference in an index's storage parameters asks of the plan, by
/// the verdicts of the method's policy (DEC-2448.1).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParameterEdit {
    Unchanged,
    /// One changed parameter cannot be altered in place: the part is replaced.
    Rebuild,
    /// One changed parameter is refused outright.
    Refuse {
        parameter: String,
    },
    Alter {
        set: BTreeMap<String, String>,
        reset: BTreeSet<String>,
    },
}

pub(super) fn parameter_edit(
    policy: &pbps_model::storage::MethodPolicy,
    was: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> ParameterEdit {
    use pbps_model::storage::ParameterChange;
    let set: BTreeMap<String, String> = now
        .iter()
        .filter(|(k, v)| was.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let reset: BTreeSet<String> = was
        .keys()
        .filter(|k| !now.contains_key(*k))
        .cloned()
        .collect();
    if set.is_empty() && reset.is_empty() {
        return ParameterEdit::Unchanged;
    }
    let named = set.keys().chain(reset.iter()).map(String::as_str);
    match policy.change_of(named) {
        (ParameterChange::Alter, _) => ParameterEdit::Alter { set, reset },
        (ParameterChange::Rebuild, _) => ParameterEdit::Rebuild,
        (ParameterChange::Refuse, parameter) => ParameterEdit::Refuse {
            parameter: parameter.unwrap_or_default().to_owned(),
        },
    }
}

/// Constraints and indexes are always matched by name and never modified in
/// place — the database itself does drop + add, and pretending otherwise would
/// only give the emitter one more path that can fail.
pub(super) fn diff_constraints(
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    diff_constraints_by(name, base, declared, changes, errs, &IndexMethod::policy);
}

/// [`diff_constraints`] under a given policy table, so that a verdict no
/// method holds yet can be exercised end to end.
pub(super) fn diff_constraints_by(
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
    policy_of: &dyn Fn(IndexMethod) -> &'static pbps_model::storage::MethodPolicy,
) {
    // Storage parameters change in place, every supported one of both
    // methods (measured on 16 and 18), so a part is rebuilt only where it
    // differs otherwise; a difference in them alone is one `ALTER INDEX`
    // (DEC-1442.1). A rebuilt part carries its declared ones in its `CREATE`.
    // That is each parameter's verdict in the method's policy (DEC-2448.1):
    // every one this model holds is `Alter`, and a method that gains a
    // parameter the engine cannot change in place says `Rebuild` or `Refuse`
    // in its own row and nothing here changes.
    fn bare<T: Clone>(part: &T, strip: impl Fn(&mut T)) -> T {
        let mut part = part.clone();
        strip(&mut part);
        part
    }
    let pk_bare = |pk: &PrimaryKey| bare(pk, |p| p.storage_parameters.clear());
    let unique_bare = |u: &UniqueConstraint| bare(u, |u| u.storage_parameters.clear());
    let index_bare = |ix: &Index| bare(ix, |ix| ix.storage_parameters.clear());
    let partitioned = declared.partition_by.is_some();
    let refused = std::cell::RefCell::new(Vec::new());
    // `None` replaces the part, `Some(None)` leaves it, `Some(Some(_))` alters it.
    let parameters = |target: IndexPart,
                      method: IndexMethod,
                      was: &BTreeMap<String, String>,
                      now: &BTreeMap<String, String>|
     -> Option<Option<Change>> {
        match parameter_edit(policy_of(method), was, now) {
            ParameterEdit::Unchanged => Some(None),
            ParameterEdit::Rebuild => None,
            // A partitioned parent's index takes no `ALTER INDEX SET`/`RESET`
            // of any method (42809, measured on 16.15 and 18.6, #2709), so an
            // option the engine would change in place on an ordinary table is
            // a rebuild here: the parent is dropped and created again with
            // the declared options, which its leaves are created from.
            ParameterEdit::Alter { .. } if partitioned => None,
            ParameterEdit::Refuse { parameter } => {
                refused.borrow_mut().push(DiffError::IndexParameterRefused {
                    table: name.clone(),
                    target: match &target {
                        IndexPart::PrimaryKey => "the primary key's index".to_owned(),
                        IndexPart::Unique(n) | IndexPart::Index(n) => {
                            format!("index {}", n.shown())
                        }
                        IndexPart::Exclusion(n) => format!("exclusion constraint {}", n.shown()),
                    },
                    method: method.as_str(),
                    parameter,
                });
                Some(None)
            }
            ParameterEdit::Alter { set, reset } => Some(Some(Change::Index(
                IndexChange::SetIndexStorageParameters {
                    table: name.clone(),
                    target,
                    method,
                    set,
                    reset,
                },
            ))),
        }
    };

    // A declaration that leaves the key unnamed (`primary_key: [id]`) leaves
    // the name to the engine, and the engine invents one (`PK__t__357D...`)
    // that the recorded state then carries. Comparing names there would
    // restate the key on every connected plan until somebody copied the
    // invented name into the file. So an unnamed declaration matches any
    // stored name and only the columns are compared; a *named* declaration is
    // compared in full, because renaming a constraint is a real change.
    //
    // A key that stays but changes layout — clustered to nonclustered or
    // back, because the table's `clustered` selector moved — is a different
    // index under the same constraint, and the engine has no statement that
    // moves a key between the two in place: `DROP_EXISTING` refuses to turn a
    // clustered index nonclustered (1925, measured), and the other direction
    // is refused while a foreign key references the key (1930). So it is
    // replaced like any other changed key (#1178).
    let mut pk_differs = match (&base.primary_key, &declared.primary_key) {
        (Some(b), Some(d)) if d.name.is_none() => b.columns != d.columns,
        (Some(b), Some(d)) => pk_bare(b) != pk_bare(d),
        (b, d) => b != d,
    } || (base.primary_key.is_some()
        && declared.primary_key.is_some()
        && base.primary_key_is_clustered() != declared.primary_key_is_clustered());
    if !pk_differs && let (Some(b), Some(d)) = (&base.primary_key, &declared.primary_key) {
        match parameters(
            IndexPart::PrimaryKey,
            IndexMethod::Btree,
            &b.storage_parameters,
            &d.storage_parameters,
        ) {
            Some(edit) => changes.extend(edit),
            None => pk_differs = true,
        }
    }
    if pk_differs {
        // A key that is *replaced* is emitted as two changes: the old one's
        // drop, and the new one's add. One change carrying both directions can
        // only be in one ordering class, and the two halves need opposite
        // ones — the drop before every column change a standing key blocks
        // (DECISIONS 269), the add after every column its new shape may name.
        // Measured, the plan that came out of one change was refused:
        // `PRIMARY KEY (id)` becoming `PRIMARY KEY (other)` while `id` is
        // relaxed ran `DROP NOT NULL` against a column `pk_t` still held —
        // `42P16` on PostgreSQL, 5074 with 4922 behind it on SQL Server.
        //
        // No model change and no new SQL: both emitters already emit the two
        // halves independently, so the statements are the ones they were and
        // only their positions move. `from: None` on the add half is accurate
        // where it runs, because the drop half has already taken the key away.
        //
        // The risk classes follow, and follow correctly: a replacement now
        // carries `Destructive` for the key it gives up as well as
        // `Constraint` for the key it takes, which is what happens to the
        // database (DECISIONS 270).
        let split = base.primary_key.is_some() && declared.primary_key.is_some();
        changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: name.clone(),
            from: base.primary_key.clone(),
            to: if split {
                None
            } else {
                declared.primary_key.clone()
            },
            // Only a key this change adds has a layout: a key that is only
            // dropped claims nothing, and PostgreSQL's emitter refuses a
            // change that says nonclustered.
            nonclustered: !split
                && declared.primary_key.is_some()
                && !declared.primary_key_is_clustered(),
        }));
        if split {
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: declared.primary_key.clone(),
                nonclustered: !declared.primary_key_is_clustered(),
            }));
        }
    }

    by_name(
        &base.foreign_keys,
        &declared.foreign_keys,
        |_, b, d| (b == d).then_some(None),
        |n| {
            Change::Constraint(ConstraintChange::DropForeignKey {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddForeignKey {
                table: name.clone(),
                name: n.clone(),
                constraint: Box::new(c.clone()),
            })
        },
        changes,
    );
    by_name(
        &base.checks,
        &declared.checks,
        |_, b, d| (b == d).then_some(None),
        |n| {
            Change::Constraint(ConstraintChange::DropCheck {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddCheck {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
            })
        },
        changes,
    );
    by_name(
        &base.unique,
        &declared.unique,
        |n, b, d| {
            (unique_bare(b) == unique_bare(d)
                && base.unique_is_clustered(n.raw()) == declared.unique_is_clustered(n.raw()))
            .then(|| {
                parameters(
                    IndexPart::Unique(n.clone()),
                    IndexMethod::Btree,
                    &b.storage_parameters,
                    &d.storage_parameters,
                )
            })
            .flatten()
        },
        |n| {
            Change::Constraint(ConstraintChange::DropUnique {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddUnique {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
                clustered: declared.unique_is_clustered(n.raw()),
            })
        },
        changes,
    );
    by_name(
        &base.indexes,
        &declared.indexes,
        |n, b, d| {
            (index_bare(b) == index_bare(d)
                && base.index_is_clustered(n.raw()) == declared.index_is_clustered(n.raw()))
            .then(|| {
                parameters(
                    IndexPart::Index(n.clone()),
                    d.method,
                    &b.storage_parameters,
                    &d.storage_parameters,
                )
            })
            .flatten()
        },
        |n| {
            Change::Index(IndexChange::DropIndex {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, ix| {
            Change::Index(IndexChange::AddIndex {
                table: name.clone(),
                name: n.clone(),
                index: Box::new(ix.clone()),
                clustered: declared.index_is_clustered(n.raw()),
                rehearsal: None,
            })
        },
        changes,
    );
    errs.extend(refused.into_inner());
}

use pbps_model::{
    CheckConstraint, Clustered, Column, ComputedColumn, ForeignKey, Index, PartitionBy,
    PartitionColumn, PartitionOf, PrimaryKey, ReplicaIdentity, Schema, SystemTime, Table,
    TableData, TableName, UniqueConstraint,
};
use std::borrow::Cow;
use std::collections::BTreeMap;

use super::{
    Expr, Req, Source,
    expect::{ColumnExpected, Expected, Implied, Own, OwnExpected, place},
};

/// The comparator's complete catalog field boundary. These borrowed fields,
/// rather than the original Table, are what the comparison families consume.
/// A new model field therefore needs an explicit policy here (DEC-1774.1).
#[derive(Clone, Copy)]
pub(crate) struct Fields<'a> {
    pub(crate) columns: &'a indexmap::IndexMap<String, Column>,
    pub(crate) computed: &'a BTreeMap<String, ComputedColumn>,
    pub(crate) primary_key: &'a Option<PrimaryKey>,
    pub(crate) unique: &'a BTreeMap<String, UniqueConstraint>,
    pub(crate) foreign_keys: &'a BTreeMap<String, ForeignKey>,
    pub(crate) checks: &'a BTreeMap<String, CheckConstraint>,
    pub(crate) indexes: &'a BTreeMap<String, Index>,
    pub(crate) clustered: &'a Option<Clustered>,
    pub(crate) replica_identity: &'a Option<ReplicaIdentity>,
    pub(crate) storage_parameters: &'a BTreeMap<String, String>,
    pub(crate) unlogged: bool,
    pub(crate) row_security: &'a pbps_model::RowSecurity,
    pub(crate) row_policies: &'a BTreeMap<String, pbps_model::RowPolicy>,
    pub(crate) system_time: &'a Option<SystemTime>,
    pub(crate) partition_by: &'a Option<PartitionBy>,
    pub(crate) partition_of: &'a Option<PartitionOf>,
    pub(crate) data: Option<&'a TableData>,
}

impl<'a> Fields<'a> {
    pub(crate) fn of(table: &'a Table) -> Self {
        let Table {
            // This metadata is not a catalog observation (SPEC 7.6). Rows
            // remain with the existing scope-aware row comparator below.
            description: _,
            columns,
            computed,
            primary_key,
            unique,
            foreign_keys,
            checks,
            indexes,
            clustered,
            replica_identity,
            storage_parameters,
            unlogged,
            row_security,
            row_policies,
            system_time,
            partition_by,
            partition_of,
            data,
        } = table;
        Self {
            columns,
            computed,
            primary_key,
            unique,
            foreign_keys,
            checks,
            indexes,
            clustered,
            replica_identity,
            storage_parameters,
            unlogged: *unlogged,
            row_security,
            row_policies,
            system_time,
            partition_by,
            partition_of,
            data: data.as_ref(),
        }
    }

    pub(crate) fn place(&self) -> super::expect::Place {
        (
            self.partition_of
                .as_ref()
                .map(|of| (of.parent.clone(), of.bound.clone())),
            self.partition_by.clone(),
        )
    }
}

pub(crate) fn column(expected: &ColumnExpected<'_>, now: &pbps_model::Column) -> Vec<&'static str> {
    let pbps_model::Column {
        ty,
        nullable,
        value_source,
        collation,
        description,
        deprecated,
    } = now;
    let source = Source::of(value_source);
    let mut moved = Vec::new();
    for (kept, name) in [
        (expected.ty.accepts(&ty), "type"),
        (expected.nullable.accepts(nullable), "nullability"),
        (expected.default.accepts(source.default), "default"),
        (
            expected
                .deprecated
                .accepts(&deprecated.as_ref().map(pbps_model::Text::raw)),
            "deprecation",
        ),
        (expected.collation.accepts(&collation.as_ref()), "collation"),
    ] {
        if !kept {
            moved.push(name);
        }
    }
    // Only the text is a planned expression change's to move. Becoming or
    // ceasing to be generated, or changing its stored kind, is still drift.
    if expected.generated_kind != source.generated.map(|g| g.stored) {
        moved.push("generation");
    } else if !expected
        .generated_expression
        .accepts(source.generated.map(|g| g.expression.as_str()))
    {
        moved.push("generation expression");
    }
    for (kept, name) in [
        (expected.identity.accepts(&source.identity), "identity"),
        (expected.sequence.accepts(&source.sequence), "sequence"),
        (
            expected.description == description.as_ref().map(pbps_model::Text::raw),
            "description",
        ),
    ] {
        if !kept {
            moved.push(name);
        }
    }
    moved
}

pub(crate) fn implied(expected: &Implied<'_>, now: &Schema, moved: &mut Vec<String>) {
    for (name, presence) in &expected.0 {
        let present = now.tables.contains_key(*name);
        if !presence.accepts(&present) {
            moved.push(if present {
                format!(
                    "{name} is there, and no change of this plan creates it",
                    name = name.shown()
                )
            } else {
                format!(
                    "{name} is gone, and no change of this plan drops it",
                    name = name.shown()
                )
            });
        }
    }
}

pub(crate) fn own(
    expected: &OwnExpected<'_>,
    now: &Fields<'_>,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    let empty = Default::default();
    let columns = now
        .partition_of
        .as_ref()
        .map(|of| &of.columns)
        .unwrap_or(&empty);
    let absent = Own::default();
    let changed = expected.0.keys().chain(columns.keys()).any(|name| {
        let required = expected.0.get(name).unwrap_or(&absent);
        let (default, not_null) = columns
            .get(name)
            .map_or((None, false), |PartitionColumn { default, not_null }| {
                (default.as_deref(), *not_null)
            });
        !required.default.accepts(default) || !required.not_null.accepts(&not_null)
    });
    if changed {
        moved.push(format!(
            "{name}'s own column defaults or NOT NULLs are not the ones this plan leaves",
            name = name.shown()
        ));
    }
}

pub(crate) fn created_place(
    declared: &Table,
    now: &Fields<'_>,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    // Connected planning and bootstrap check bound spellings before DDL;
    // an offline preview cannot be applied. Hold the pinned declaration
    // even at the first staged read, before it could become a baseline.
    if !Req::Planned(place(declared)).accepts(&now.place()) {
        moved.push(format!(
            "{name}'s partition parent, bound or key is not the one this plan creates",
            name = name.shown()
        ));
    }
}

pub(crate) fn table(
    expected: &Expected<'_>,
    now: &Fields<'_>,
    placed: &Fields<'_>,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    if !expected.identity.accepts(&now.replica_identity.as_ref()) {
        moved.push(match expected.identity {
            Req::Keep(_) => {
                format!(
                    "{name} replica identity changed, and no change of this plan sets it",
                    name = name.shown()
                )
            }
            Req::Planned(_) | Req::PlannedOrKeep(_, _) | Req::Unpromised => {
                format!(
                    "{name} replica identity is not the one this plan sets",
                    name = name.shown()
                )
            }
        });
    }
    if !expected.row_security.accepts(&now.row_security)
        || !expected.row_policies.accepts(&now.row_policies)
    {
        moved.push(format!(
            "{} row-security flags or policies changed, and no change of this plan sets them",
            name.shown()
        ));
    }
    if !expected
        .storage
        .accepts(&Cow::Borrowed(now.storage_parameters))
    {
        moved.push(format!(
            "{name} storage parameters are not the ones this plan leaves",
            name = name.shown()
        ));
    }
    if !expected.persistence.accepts(&now.unlogged) {
        moved.push(format!(
            "{name} is {} and this plan does not leave it so",
            if now.unlogged { "unlogged" } else { "logged" },
            name = name.shown()
        ));
    }
    if !expected.place.accepts(&placed.place()) {
        moved.push(format!(
            "{name}'s partition parent, bound or key is not the one the plan was approved over",
            name = name.shown()
        ));
    }
}

// Keep the existing diagnostic order around partition-own checks until their
// expectation slice moves them here too.
pub(crate) fn system_time(
    expected: &Expected<'_>,
    now: &Fields<'_>,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    if !expected.system_time.accepts(&now.system_time.as_ref()) {
        moved.push(format!(
            "{name} period or system versioning changed, and this plan does not change it",
            name = name.shown()
        ));
    }
}

/// Whether a column read back is the one a declaration asked for, in the
/// fields the catalog reads back unchanged: the normalized type, the
/// nullability, the identity, and whether there is a default at all — its
/// **text** is the engine's (`0` comes back `((0))`), so only its presence is
/// comparable (DECISIONS 185, 186).
pub(crate) fn declared_column(
    dialect: &dyn pbps_dialect::Dialect,
    declared: &pbps_model::Column,
    now: &pbps_model::Column,
) -> bool {
    use pbps_model::Column;
    // Catalogs do not read the declaration-only annotations. Enumerate them
    // nevertheless: a new observable model field must choose a policy here.
    let Column {
        ty,
        nullable,
        value_source,
        collation,
        description: _,
        deprecated: _,
    } = declared;
    let Column {
        ty: observed_type,
        nullable: observed_nullable,
        value_source: observed_source,
        collation: observed_collation,
        description: _,
        deprecated: _,
    } = now;
    let expected = Source::of(value_source);
    let observed = Source::of(observed_source);
    super::super::same_type(dialect, ty, observed_type)
        && nullable == observed_nullable
        && collation == observed_collation
        && Expr::Planned(expected.default.is_some()).accepts(observed.default)
        && expected.identity == observed.identity
        && expected.generated.map(|g| g.stored) == observed.generated.map(|g| g.stored)
        && match (expected.sequence, observed.sequence) {
            (Some(declared), Some(now)) => declared.declares(now),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
}

pub(crate) fn columns_after(
    dialect: &dyn pbps_dialect::Dialect,
    expected: &super::expect::ColumnChanges<'_>,
    after: &Schema,
    moved: &mut Vec<String>,
) {
    for (column, expected) in &expected.presence {
        let Some(table) = after.tables.get(&column.table) else {
            // The table itself is gone or renamed; its own check answers.
            continue;
        };
        match (expected, table.columns.contains_key(&column.name)) {
            (pbps_model::Presence::Present, false) => moved.push(format!(
                "{} column `{}` is not there, and this plan writes it",
                column.table.shown(),
                column.name
            )),
            (pbps_model::Presence::Absent, true) => moved.push(format!(
                "{} column `{}` is still there, and this plan removes it",
                column.table.shown(),
                column.name
            )),
            _ => {}
        }
    }
    for ((column, _), promise) in &expected.promises {
        let Some(now) = after
            .tables
            .get(&column.table)
            .and_then(|t| t.columns.get(&column.name))
        else {
            // Reported above, or the table's own check answers.
            continue;
        };
        use pbps_model::ColumnPromise;
        let (kept, what) = match promise {
            ColumnPromise::Whole(declared) => {
                (declared_column(dialect, declared, now), "definition")
            }
            ColumnPromise::Type(to) => (super::super::same_type(dialect, to, &now.ty), "type"),
            ColumnPromise::Nullable(to) => (now.nullable == *to, "nullability"),
            ColumnPromise::Default(has) => (
                Expr::Planned(*has).accepts(Source::of(&now.value_source).default),
                "default",
            ),
            ColumnPromise::Generated(has) => (
                *has == Source::of(&now.value_source).generated.is_some(),
                "generation expression",
            ),
            ColumnPromise::Numbering { from, to } => (numbering(from, to, now), "numbering source"),
            ColumnPromise::Collation(c) => (now.collation.as_ref() == *c, "collation"),
        };
        if !kept {
            moved.push(format!(
                "{} column `{}` does not have the {what} this plan gives it",
                column.table.shown(),
                column.name
            ));
        }
    }
}

/// Whether `now` has the numbering source a numbering change gives it (#1889):
/// start and increment included, not its kind alone, so a retune another
/// session undoes is found.
fn numbering(
    from: &pbps_model::ValueSource,
    to: &pbps_model::ValueSource,
    now: &pbps_model::Column,
) -> bool {
    use pbps_model::ValueSource;
    match (to, &now.value_source) {
        (ValueSource::Identity(a), ValueSource::Identity(b)) => a == b,
        // A name the declaration leaves out is the one the sequence kept,
        // where it was known.
        (ValueSource::Sequence(a), ValueSource::Sequence(b)) => {
            let kept = match (&a.name, from) {
                (None, ValueSource::Sequence(was)) => was.name.as_ref(),
                (None | Some(_), _) => None,
            };
            a.declares(b) && kept.is_none_or(|name| b.name.as_ref() == Some(name))
        }
        (ValueSource::Identity(_) | ValueSource::Sequence(_), _) => false,
        (ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_), now) => {
            pbps_model::NumberingKind::of(now).is_none()
        }
    }
}

pub(crate) fn columns(
    expected: &super::expect::ColumnsExpected<'_>,
    now: &Fields<'_>,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    for required in &expected.0 {
        let column = &required.name;
        let observed = now.columns.get(column);
        if !required.present.accepts(&observed.is_some()) {
            moved.push(format!("{name} column `{column}` is not on both sides, and no change of this plan adds or removes it", name = name.shown()));
        } else if let (Some(fields), Some(observed)) = (&required.fields, observed) {
            for what in self::column(fields, observed) {
                moved.push(format!("{name} column `{column}` has a different {what} from the one the plan was approved over, and no change of this plan moves it", name = name.shown()));
            }
        }
    }
}

//! One declared value interface per side, independent of the emitted change.
use super::Side;
use pbps_model::{
    Change, ChangeSet, Column, ColumnRef, DomainDelta, ManagedType, ObjectLocalName, TypeRef, Uid,
    ValueSource,
};
use std::collections::{BTreeMap, BTreeSet};

type Key = (Uid, Uid);

#[derive(Default)]
struct EffectiveSide(BTreeMap<Key, EffectiveColumn>);

struct ValueContext {
    nullable: bool,
    types: BTreeSet<TypeRef>,
    checks: BTreeMap<(TypeRef, ObjectLocalName), String>,
}
impl Default for ValueContext {
    fn default() -> Self {
        Self {
            nullable: true,
            types: BTreeSet::new(),
            checks: BTreeMap::new(),
        }
    }
}

pub(super) struct EffectiveColumn {
    pub at: ColumnRef,
    pub column: Column,
    contexts: Vec<ValueContext>,
}
impl EffectiveColumn {
    fn of(at: ColumnRef, mut column: Column, side: Side<'_>) -> Self {
        let mut contexts = vec![ValueContext {
            nullable: column.nullable,
            ..Default::default()
        }];
        let ty = column.ty.clone();
        let mut next = &ty;
        // Declarations are validated before planning. The bound also keeps this
        // private projection finite for incomplete unit fixtures.
        for _ in 0..=side.schema.types.len() {
            if next.is_array() {
                contexts.push(ValueContext::default());
            }
            let Some(reference) = next.type_ref() else {
                break;
            };
            let whole = contexts.len() == 1;
            let context = contexts.last_mut().expect("whole-column context exists");
            context.types.insert(reference.clone());
            let Some(ManagedType::Domain {
                base,
                not_null,
                default,
                checks,
                collation,
            }) = side.schema.types.get(reference)
            else {
                break;
            };
            context.nullable &= !not_null;
            context.checks.extend(
                checks
                    .iter()
                    .map(|(name, text)| ((reference.clone(), name.clone()), text.clone())),
            );
            if whole && matches!(column.value_source, ValueSource::None) {
                column.value_source = default
                    .clone()
                    .map_or(ValueSource::None, ValueSource::Default);
            }
            // Unlike a DEFAULT or NOT NULL, collation follows the collatable
            // type through an array; explicit and outer overrides still win.
            column.collation = column.collation.or_else(|| {
                collation
                    .as_ref()
                    .map(|c| pbps_model::Collation::new(c.raw().to_string()))
            });
            next = base;
        }
        column.nullable = contexts[0].nullable;
        Self {
            at,
            column,
            contexts,
        }
    }
    fn uses(&self, ty: &TypeRef) -> bool {
        self.contexts.iter().any(|c| c.types.contains(ty))
    }
}
impl EffectiveSide {
    fn of(side: Side<'_>) -> Self {
        let mut columns = BTreeMap::new();
        for owner in side.schema.tables.keys() {
            let Some(table) = side.ids.table_uid(owner) else {
                continue;
            };
            for (at, source, column) in side.columns(owner) {
                if let Some(uid) = side.ids.column_uid(&source) {
                    columns.insert(
                        (table.clone(), uid.clone()),
                        EffectiveColumn::of(at, column, side),
                    );
                }
            }
        }
        Self(columns)
    }
}

pub(super) struct Interfaces {
    sides: [EffectiveSide; 2],
    added: BTreeSet<Uid>,
    created: BTreeSet<Uid>,
    added_columns: BTreeSet<ColumnRef>,
}
impl Interfaces {
    pub fn of(sides: &[Side<'_>; 2], cs: &ChangeSet) -> Self {
        let mut added = BTreeSet::new();
        let mut created = BTreeSet::new();
        let mut added_columns = BTreeSet::new();
        for p in &cs.changes {
            if let Change::AddColumn {
                uid, table, name, ..
            } = &p.change
            {
                added_columns.insert(table.column(name.clone()));
                if !sides[0].ids.columns.contains_key(uid) {
                    added.insert(uid.clone());
                }
            }
            if let Change::CreateTable { uid, .. } = &p.change
                && sides[0].table(Some(uid)).is_none()
            {
                created.insert(uid.clone());
            }
        }
        Self {
            sides: sides.map(EffectiveSide::of),
            added,
            created,
            added_columns,
        }
    }
    pub fn adds_column(&self, column: &ColumnRef) -> bool {
        self.added_columns.contains(column)
    }
    pub fn created(&self, table: &Uid) -> bool {
        self.created.contains(table)
    }
    pub fn columns(&self) -> impl Iterator<Item = ColumnPair<'_>> {
        let [before, after] = &self.sides;
        before
            .0
            .keys()
            .chain(after.0.keys().filter(|key| !before.0.contains_key(*key)))
            .map(|key| ColumnPair {
                key,
                before: before.0.get(key),
                after: after.0.get(key),
                added: self.added.contains(&key.1),
            })
    }
}

#[derive(Clone, Copy)]
pub(super) struct ColumnPair<'a> {
    pub key: &'a Key,
    pub before: Option<&'a EffectiveColumn>,
    pub after: Option<&'a EffectiveColumn>,
    added: bool,
}
impl<'a> ColumnPair<'a> {
    pub fn addresses(self) -> impl Iterator<Item = &'a ColumnRef> {
        [self.before, self.after]
            .into_iter()
            .flatten()
            .map(|c| &c.at)
    }
    pub fn uses(self, ty: &TypeRef) -> bool {
        [self.before, self.after]
            .into_iter()
            .flatten()
            .any(|c| c.uses(ty))
    }
    pub fn supplied(self) -> bool {
        self.added
            && self
                .after
                .is_some_and(|c| !matches!(c.column.value_source, ValueSource::None))
    }
    pub fn nullability(self, writes: bool) -> bool {
        let (Some(before), Some(after)) = (self.before, self.after) else {
            return false;
        };
        // An element restriction never masks the array's nullable bit, nor a
        // restriction in another element context (DEC-2484.1).
        (0..before.contexts.len().max(after.contexts.len())).any(|i| {
            let was = before.contexts.get(i).is_none_or(|c| c.nullable);
            let now = after.contexts.get(i).is_none_or(|c| c.nullable);
            was != now && now != writes
        })
    }
    pub fn default_changed(self) -> bool {
        fn default(c: &EffectiveColumn) -> Option<&String> {
            match &c.column.value_source {
                ValueSource::Default(text) => Some(text),
                ValueSource::None
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_)
                | ValueSource::Generated(_) => None,
            }
        }
        self.before
            .zip(self.after)
            .is_some_and(|(b, a)| default(b) != default(a))
    }
    pub fn collation_changed(self) -> bool {
        self.before
            .zip(self.after)
            .is_some_and(|(b, a)| b.column.collation != a.column.collation)
    }
    pub fn adds_checks(self, ty: &TypeRef, delta: &DomainDelta<'_>) -> bool {
        // Keep the existing conservative two-sided usage rule. DomainDelta,
        // not a second comparison of opaque CHECK text, owns the added checks.
        self.uses(ty) && !delta.add_checks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::Schema;

    #[test]
    fn element_not_null_leaves_the_array_nullable_but_scalar_overrides_still_hold() {
        let domain = |base: &str, not_null| ManagedType::Domain {
            base: base.parse().unwrap(),
            not_null,
            default: None,
            checks: Default::default(),
            collation: None,
        };
        let schema = Schema {
            types: [
                ("app.base".parse().unwrap(), domain("integer", false)),
                ("app.d".parse().unwrap(), domain("app.base", true)),
                ("app.outer_array".parse().unwrap(), domain("app.d[]", true)),
            ]
            .into(),
            ..Default::default()
        };
        // Measured on PG 16/18: NOT NULL on the element rejects NULL elements,
        // while a column's or outer domain's own NOT NULL rejects NULL arrays.
        for (ty, nullable, expected) in [
            ("app.d[]", true, true),
            ("app.d", true, false),
            ("app.outer_array", true, false),
            ("app.d[]", false, false),
        ] {
            let mut column = Column::new(ty.parse().unwrap());
            column.nullable = nullable;
            let effective = EffectiveColumn::of(
                "app.t.c".parse().unwrap(),
                column,
                Side {
                    schema: &schema,
                    ids: &pbps_model::IdsFile::default(),
                },
            )
            .column;
            assert_eq!(
                effective.nullable, expected,
                "{ty}, column nullable={nullable}"
            );
        }
    }
}

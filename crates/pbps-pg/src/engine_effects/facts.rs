//! Endpoint identity and normalized values. These descriptions carry no
//! choice of membership cut or executable plan (DEC-2296.2).
//!
//! Which physical column carries a parent's column at either end is read from
//! the plan's [`Lineage`], never rediscovered from names here.
use crate::Postgres;
use pbps_dialect::Dialect;
use pbps_model::operation::{
    BoundInput, ColumnKey, End, Incarnation, Lineage, MembershipCatalog, ObjectKey, Recorded,
    Snapshot, Standing, symbolic::*,
};
use pbps_model::{Column, ColumnName, ColumnRef, TableName, ValueSource};
use std::collections::{BTreeMap, BTreeSet};

pub(super) type Outputs = BTreeMap<Attribute, Fact<Value>>;
pub(super) const ATTRIBUTES: [Attribute; 7] = [
    Attribute::Presence,
    Attribute::Name,
    Attribute::Type,
    Attribute::Collation,
    Attribute::Default,
    Attribute::Numbering,
    Attribute::NotNull,
];
pub(super) fn value(datum: Datum, ownership: Ownership) -> Fact<Value> {
    Fact::Known(Value {
        datum,
        ownership,
        bindings: BTreeSet::new(),
    })
}
pub(super) fn definition(text: impl Into<String>) -> Fact<Value> {
    value(Datum::Definition(text.into()), Ownership::Own)
}
pub(super) fn absent() -> Fact<Value> {
    value(Datum::Absent, Ownership::Own)
}
pub(super) fn default_value(
    pg: &Postgres,
    source: &ValueSource,
    table: &TableName,
    name: &str,
) -> Fact<Value> {
    match source {
        ValueSource::Default(expression) => {
            // Lexical dependency candidates are not catalog binding evidence.
            // Until the connected seam supplies those identities this fact is
            // unavailable, rather than an expression with no dependencies.
            if !crate::rows::is_constant(expression) {
                return Fact::Unread;
            }
            definition(pg.normalize_definition(expression))
        }
        ValueSource::Sequence(sequence) => {
            match crate::emit::owned_sequence_default(table, name, sequence) {
                Ok(expression) => definition(pg.normalize_definition(&expression)),
                Err(error) => Fact::Unsupported {
                    sqlstate: None,
                    reason: error.to_string(),
                },
            }
        }
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => {
            definition("no column default")
        }
    }
}
/// ATTACH compares the complete type form, including generation/identity.
/// Ordinary DEFAULT and sequence ownership are independent properties.
pub(super) fn type_value(
    pg: &Postgres,
    ty: &pbps_model::ColumnType,
    source: &ValueSource,
) -> Fact<Value> {
    let Ok(ty) = pg.normalize_type(ty) else {
        return Fact::Unread;
    };
    let generation = match source {
        ValueSource::Generated(g) => Some((pg.normalize_definition(&g.expression), g.stored)),
        ValueSource::None
        | ValueSource::Default(_)
        | ValueSource::Sequence(_)
        | ValueSource::Identity(_) => None,
    };
    let identity = match source {
        ValueSource::Identity(i) => Some(i),
        ValueSource::None
        | ValueSource::Default(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => None,
    };
    definition(
        serde_json::to_string(&(ty.raw().to_string(), generation, identity))
            .expect("type form serializes"),
    )
}
/// This is only a conservative unread-fact check, never a binding resolver.
/// Quoted/unqualified calls and OID-alias literals need actual catalog proof.
pub(super) fn may_bind_expression(expression: &str) -> bool {
    if !crate::LEXICON.string_literals(expression).is_empty() {
        return true;
    }
    let mut previous = None;
    for c in crate::LEXICON.code_only(expression).chars() {
        if c == '('
            && previous.is_some_and(|p| (crate::LEXICON.identifier_continues)(p) || p == '"')
        {
            return true;
        }
        if !c.is_ascii_whitespace() {
            previous = Some(c);
        }
    }
    false
}
pub(super) fn column(pg: &Postgres, table: &TableName, name: &ColumnName, c: &Column) -> Outputs {
    use Attribute::*;
    let ty = type_value(pg, &c.ty, &c.value_source);
    BTreeMap::from([
        (Presence, definition("present")),
        (Name, definition(name.raw())),
        (Type, ty),
        (
            Collation,
            definition(serde_json::to_string(&c.collation).expect("column collation serializes")),
        ),
        (
            Default,
            default_value(pg, &c.value_source, table, name.raw()),
        ),
        (NotNull, value(Datum::NotNull(!c.nullable), Ownership::Own)),
        (
            Numbering,
            match &c.value_source {
                ValueSource::Sequence(s) => {
                    definition(serde_json::to_string(s).expect("owned sequence serializes"))
                }
                ValueSource::Identity(i) => {
                    definition(serde_json::to_string(i).expect("identity serializes"))
                }
                ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_) => absent(),
            },
        ),
    ])
}
fn put(state: &mut State, table: &Recorded, key: &ColumnKey, attributes: Outputs, exists: bool) {
    for (attribute, value) in attributes {
        // Numbering describes the engine-owned sequence, rather than the
        // column which owns it. No sequence means no surviving source.
        let exists = exists
            && !(attribute == Attribute::Numbering
                && matches!(&value,Fact::Known(v) if v.datum==Datum::Absent));
        let property = Property::Column(key.clone(), attribute);
        state.cells.insert(
            (table.clone(), property),
            Cell {
                value,
                version: Version::Base,
                origin: exists.then(|| PhysicalOrigin {
                    table: table.clone(),
                    object: ObjectKey::Column(key.clone()),
                    birth: Version::Base,
                }),
            },
        );
    }
}
fn missing() -> Outputs {
    ATTRIBUTES
        .into_iter()
        .map(|attr| (attr, absent()))
        .collect()
}

/// The two ends of one analysis and what the connected seam read of the
/// baseline's stored columns.
struct Read<'a> {
    pg: &'a Postgres,
    catalog: &'a MembershipCatalog,
    lineage: &'a Lineage,
    ends: [Snapshot<'a>; 2],
}

/// All facts refer to recorded identities. Enumeration supplies known absence
/// for the property universe; actual membership always comes from one side.
/// A plan that moves no partition has no facts.
pub(crate) fn of(
    pg: &Postgres,
    catalog: &MembershipCatalog,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Facts {
    let lineage = Lineage::of(before, after);
    if lineage.is_empty() {
        return Facts::default();
    }
    let read = Read {
        pg,
        catalog,
        lineage: &lineage,
        ends: [before, after],
    };
    Facts {
        parents: lineage.parents().cloned().collect(),
        base: read.state(End::Base),
        goal: read.state(End::Goal),
    }
}

impl Read<'_> {
    fn side(&self, end: End) -> Snapshot<'_> {
        self.ends[end as usize]
    }

    fn state(&self, end: End) -> State {
        let side = self.side(end);
        let mut tables = BTreeSet::new();
        for (parent, family) in self.lineage.families() {
            if family.present(end) {
                tables.insert(parent);
            }
            tables.extend(
                family
                    .children()
                    .filter(|c| family.standing(c, end) != Standing::Absent),
            );
        }
        let mut state = State::default();
        for owner in tables {
            let Some(name) = side.ids.tables.get(owner.uid()) else {
                continue;
            };
            let Some(table) = side.schema.tables.get(name) else {
                continue;
            };
            state.tables.insert(owner.clone());
            state.cells.insert(
                (owner.clone(), Property::Address),
                Cell {
                    value: definition(name.raw().to_string()),
                    version: Version::Base,
                    origin: Some(PhysicalOrigin {
                        table: owner.clone(),
                        object: ObjectKey::Table(owner.clone()),
                        birth: Version::Base,
                    }),
                },
            );
            for (column_name, c) in &table.columns {
                if let Some(key) = side.column_identity(&name.column(column_name)) {
                    let mut values = column(self.pg, name, column_name, c);
                    let source = self.source(owner, &key, end);
                    if let ValueSource::Default(expression) = &c.value_source
                        && let Some(value) =
                            source.as_ref().and_then(|s| self.observed(s, expression))
                    {
                        values.insert(Attribute::Default, value);
                    }
                    if let ValueSource::Generated(g) = &c.value_source {
                        self.bind_generated(&mut values, source.as_ref(), &g.expression);
                    }
                    put(&mut state, owner, &key, values, true);
                }
            }
            if table.partition_of.is_some() {
                self.member(&mut state, owner, end);
            }
        }
        for (parent, family) in self.lineage.families() {
            self.absences(&mut state, parent, family, end);
        }
        state
    }

    /// A member's effective columns: its parent's, inherited, with the
    /// member's own DEFAULT and NOT NULL over them.
    fn member(&self, state: &mut State, owner: &Recorded, end: End) {
        let side = self.side(end);
        let name = &side.ids.tables[owner.uid()];
        let of = side.schema.tables[name].partition_of.as_ref().unwrap();
        let Some(parent) = side.table_identity(&of.parent) else {
            return;
        };
        let Some(parent_table) = side.schema.tables.get(&of.parent) else {
            return;
        };
        let Some(family) = self.lineage.family(&parent) else {
            return;
        };
        let version_18 = self.pg.server_version_num.is_some_and(|v| v >= 180000);
        let mut aliases = BTreeMap::new();
        for (uid, parent_key) in family.columns() {
            let Some(column_name) = family.name(uid, end) else {
                continue;
            };
            let c = &parent_table.columns[column_name];
            let physical = family.slot(uid, owner, end).expect("named at this end");
            let (key, own) = (physical.key, of.columns.get(column_name));
            let source = self.source(owner, &key, end);
            let mut effective = column(self.pg, &of.parent, column_name, c);
            if let ValueSource::Generated(g) = &c.value_source {
                self.bind_generated(&mut effective, source.as_ref(), &g.expression);
            }
            for value in effective.values_mut() {
                if let Fact::Known(v) = value {
                    v.ownership = Ownership::Inherited;
                }
            }
            if let Some(own) = own {
                if let Some(expression) = &own.default {
                    effective.insert(
                        Attribute::Default,
                        default_value(
                            self.pg,
                            &ValueSource::Default(expression.clone()),
                            name,
                            column_name.raw(),
                        ),
                    );
                }
                if own.not_null {
                    let ownership = if version_18 {
                        Ownership::LocalNotNull
                    } else {
                        Ownership::Own
                    };
                    effective.insert(Attribute::NotNull, value(Datum::NotNull(true), ownership));
                }
            }
            if let Some(source) = &source {
                let own_default = own.and_then(|o| o.default.as_deref());
                let expression = own_default.or(match &c.value_source {
                    ValueSource::Default(e) => Some(e.as_str()),
                    ValueSource::None
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => None,
                });
                if let Some(expression) = expression
                    && let Some(mut value) = self.observed(source, expression)
                {
                    if let (None, Fact::Known(v)) = (own_default, &mut value) {
                        v.ownership = Ownership::Inherited;
                    }
                    effective.insert(Attribute::Default, value);
                }
                if end == End::Base
                    && !c.nullable
                    && version_18
                    && self
                        .catalog
                        .columns
                        .get(source)
                        .is_some_and(|c| c.local_not_null)
                {
                    effective.insert(
                        Attribute::NotNull,
                        value(Datum::NotNull(true), Ownership::LocalNotNull),
                    );
                }
            }
            // A partition inherits a sequence-backed DEFAULT, never
            // ownership of its parent's sequence (DEC-2296.2).
            effective.insert(Attribute::Numbering, absent());
            for attr in ATTRIBUTES {
                aliases.insert(
                    Property::Column(parent_key.clone(), attr.clone()),
                    Property::Column(key.clone(), attr),
                );
            }
            put(state, owner, &key, effective, true);
        }
        state.members.insert(
            owner.clone(),
            Membership {
                parent,
                incarnation: Version::Base,
                aliases,
            },
        );
    }

    /// New and retired parent columns have recorded IDs at the other end.
    /// That is a known absent physical source, never a guessed identity, in
    /// the parent and in every child's slot for it.
    fn absences(
        &self,
        state: &mut State,
        parent: &Recorded,
        family: &pbps_model::operation::Family,
        end: End,
    ) {
        let side = self.side(end);
        let Some(name) = side.ids.tables.get(parent.uid()) else {
            return;
        };
        let Some(table) = side.schema.tables.get(name) else {
            return;
        };
        for (uid, key) in family.columns() {
            if let Some(other_name) = family.name(uid, end.other())
                && family.name(uid, end).is_none()
            {
                // A column of that name without a recorded UID is unread.
                let unrecorded = table.columns.contains_key(other_name)
                    && side.column_identity(&name.column(other_name)).is_none();
                let outputs = if unrecorded {
                    ATTRIBUTES.into_iter().map(|a| (a, Fact::Unread)).collect()
                } else {
                    missing()
                };
                put(state, parent, &key, outputs, false);
            }
            for child in family.children() {
                // No slot: another column of the parent holds this one's old
                // name here, and the slot's facts are that column's.
                let Some(slot) = family.slot(uid, child, end) else {
                    continue;
                };
                match family.standing(child, end) {
                    Standing::Standalone
                        if family.standing(child, end.other()) == Standing::Member =>
                    {
                        let own = &side.ids.tables[child.uid()];
                        if !side.schema.tables[own].columns.contains_key(&slot.name)
                            && !state.cells.contains_key(&(
                                child.clone(),
                                Property::Column(slot.key.clone(), Attribute::Presence),
                            ))
                        {
                            put(state, child, &slot.key, missing(), false);
                        }
                    }
                    Standing::Member if family.name(uid, end).is_none() => {
                        // A recursively retired member property has no
                        // physical source, but still belongs to the
                        // inherited namespace. Owned sequences never enter
                        // it.
                        for attr in ATTRIBUTES {
                            let ownership = if attr == Attribute::Numbering {
                                Ownership::Own
                            } else {
                                Ownership::Inherited
                            };
                            let local = Property::Column(slot.key.clone(), attr.clone());
                            state
                                .members
                                .get_mut(child)
                                .expect("a member has a membership")
                                .aliases
                                .insert(Property::Column(key.clone(), attr), local.clone());
                            state.cells.insert(
                                (child.clone(), local),
                                Cell {
                                    value: value(Datum::Absent, ownership),
                                    version: Version::Base,
                                    origin: None,
                                },
                            );
                        }
                    }
                    Standing::Member | Standing::Standalone | Standing::Absent => {}
                }
            }
        }
    }

    /// The baseline column which stores what `key` holds at `end`. The table's
    /// own address selects the stored relation, and the key's identity through
    /// the lineage selects the column in it, whatever either is called now.
    fn source(&self, table: &Recorded, key: &ColumnKey, end: End) -> Option<ColumnRef> {
        let before = self.side(End::Base);
        let address = before.ids.tables.get(table.uid())?;
        let stored = |name: &ColumnName| {
            let at = address.column(name);
            self.catalog.columns.contains_key(&at).then_some(at)
        };
        if let ColumnKey::Recorded(uid) = key
            && let Some(at) = before
                .ids
                .columns
                .get(uid.uid())
                .filter(|at| &at.table == address)
        {
            return stored(&at.name);
        }
        self.lineage.families().find_map(|(_, family)| {
            let uid = family.parent_column(key, table, end)?;
            stored(&family.physical(uid, table, End::Base)?.name)
        })
    }

    /// The stored default when it is the declared one, with what it binds.
    fn observed(&self, source: &ColumnRef, expression: &str) -> Option<Fact<Value>> {
        let held = self.catalog.columns.get(source)?;
        // What the stored default binds says nothing about another expression
        // observed in its place, so the match comes before the bindings.
        let matches = |actual: &str| {
            self.pg.normalize_definition(expression) == self.pg.normalize_definition(actual)
        };
        if held.default.as_deref().is_some_and(|a| !matches(a)) {
            return None;
        }
        if held.unrepresented {
            return Some(Fact::Unsupported {
                sqlstate: None,
                reason: "the default binds a resource not represented by the membership facts"
                    .into(),
            });
        }
        held.default.as_ref()?;
        let mut bindings = held
            .routines
            .iter()
            .cloned()
            .map(|id| BoundInput {
                input: ObjectKey::Module(id),
                incarnation: Incarnation::Base,
            })
            .collect::<BTreeSet<_>>();
        for sequence in &held.sequences {
            let Some(uid) = self.side(End::Base).ids.column_uid(sequence) else {
                return Some(Fact::Unread);
            };
            bindings.insert(BoundInput {
                input: pbps_model::operation::column_key(uid),
                incarnation: Incarnation::Base,
            });
        }
        Some(Fact::Known(Value {
            datum: Datum::Definition(self.pg.normalize_definition(expression)),
            bindings,
            ownership: Ownership::Own,
        }))
    }

    /// A generated column's expression binds what its stored text binds. An
    /// expression that may bind and was not read is unread, not empty.
    fn bind_generated(&self, values: &mut Outputs, source: Option<&ColumnRef>, expression: &str) {
        match source.and_then(|s| self.observed(s, expression)) {
            Some(Fact::Known(proof)) => {
                if let Some(Fact::Known(ty)) = values.get_mut(&Attribute::Type) {
                    ty.bindings = proof.bindings;
                }
            }
            Some(fact @ (Fact::Unread | Fact::Unsupported { .. })) => {
                values.insert(Attribute::Type, fact);
            }
            None if may_bind_expression(expression) => {
                values.insert(Attribute::Type, Fact::Unread);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests;

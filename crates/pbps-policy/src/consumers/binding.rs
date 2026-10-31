use super::{Consumers, Side};
use pbps_model::{Change, ColumnRef, ModuleId, TableName, Uid};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Target {
    Table(Uid),
    Column {
        table: Uid,
        uid: Uid,
    },
    NamedColumn {
        table: Uid,
        name: pbps_model::ColumnName,
    },
    Module(ModuleId),
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub(super) consumer: String,
    pub(super) text: String,
    pub(super) subject: String,
    pub(super) writes: bool,
    pub(super) target: Target,
}

#[derive(Debug, Default, Clone)]
pub struct Bindings(pub(super) Vec<Entry>);

impl Bindings {
    pub fn contracts(&self) -> impl Iterator<Item = &Entry> {
        self.0.iter()
    }

    pub fn on_sides(&self, sides: &[Side<'_>; 2]) -> Self {
        Self(self.0.iter().map(|entry| entry.on_sides(sides)).collect())
    }

    /// Spelling and review metadata do not retire an identity-bound contract.
    pub fn contains(&self, entry: &Entry) -> bool {
        self.0.iter().any(|own| {
            own.consumer == entry.consumer
                && own.writes == entry.writes
                && own.target == entry.target
        })
    }
}

impl Entry {
    /// The declaration spelling, retained for retirement diagnostics.
    pub fn declaration(&self) -> String {
        format!(
            "consumers.{}.{}: {}",
            self.consumer,
            self.access(),
            self.text
        )
    }

    /// Evaluate one restored entry through the ordinary plan rule.
    pub fn bindings(&self) -> Bindings {
        Bindings(vec![self.clone()])
    }

    /// Preserve the bound target, including when a vacated spelling is reused.
    /// Only a named column needs the ordinary partition identity bridge; find
    /// its table by UID and reuse Object::key, never parse the entry's text.
    pub fn on_sides(&self, sides: &[Side<'_>; 2]) -> Self {
        let mut entry = self.clone();
        if let Target::NamedColumn { table, name } = &self.target {
            entry.target = sides
                .iter()
                .find_map(|side| {
                    let table = side.ids.tables.get(table)?;
                    side.schema.tables.get(table)?;
                    let object = Object::Column(table.column(name.clone()));
                    // Computed columns also have named targets. Start from a
                    // side with this bound key, not an ordinary replacement.
                    if object.key(*side, &[*side])? != self.target {
                        return None;
                    }
                    object.key(*side, sides)
                })
                .unwrap_or_else(|| self.target.clone());
        }
        entry
    }

    pub(super) fn access(&self) -> &'static str {
        if self.writes { "writes" } else { "uses" }
    }
    pub(super) fn table(&self) -> Option<&Uid> {
        match &self.target {
            Target::Table(uid)
            | Target::Column { table: uid, .. }
            | Target::NamedColumn { table: uid, .. } => Some(uid),
            Target::Module(_) => None,
        }
    }
    pub(super) fn column(&self, uid: &Uid) -> bool {
        match &self.target {
            Target::Table(_) => true,
            Target::Column { uid: own, .. } => own == uid,
            Target::NamedColumn { .. } | Target::Module(_) => false,
        }
    }
    pub(super) fn named_column(&self, name: &pbps_model::ColumnName, sides: &[Side<'_>]) -> bool {
        match &self.target {
            Target::Table(_) => true,
            Target::NamedColumn { name: own, .. } => own == name,
            Target::Column { uid, .. } => sides
                .iter()
                .any(|s| s.ids.columns.get(uid).is_some_and(|c| &c.name == name)),
            Target::Module(_) => false,
        }
    }
    pub(super) fn change_text(&self, change: &Change) -> String {
        // Only typed names and the serialized tag feed prose, never SQL.
        let value = serde_json::to_value(change).expect("a typed change serializes");
        let op = value["op"]
            .as_str()
            .expect("a change has an op")
            .replace('_', " ");
        // Nullability changes no cell reading, but it does change a definition.
        let columns = change.columns_redefined();
        let object = columns
            .first()
            .map(|(c, _)| c.shown().to_string())
            .unwrap_or_else(|| pbps_model::escape::Escaped(&change.subject()).to_string());
        let detail = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(|name| format!(" `{}`", pbps_model::escape::Escaped(name)))
            .unwrap_or_default();
        let mut description = format!("{op}{detail} on `{object}`");
        if let Some(nullable) = value
            .get("to_nullable")
            .and_then(serde_json::Value::as_bool)
        {
            description.push_str(if nullable {
                " to nullable"
            } else {
                " to NOT NULL"
            });
        }
        if let Some(to) = value.get("to").and_then(serde_json::Value::as_str) {
            description.push_str(&format!(" to `{}`", pbps_model::escape::Escaped(to)));
        }
        description
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Object {
    Table(TableName),
    Column(ColumnRef),
    Module(ModuleId),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Declared(Object),
    Bound(Target),
}

impl Object {
    fn label(&self) -> String {
        match self {
            Self::Table(t) => format!("table {}", t.shown()),
            Self::Column(c) => format!("column {}", c.shown()),
            Self::Module(m) => format!("module {}", m.shown()),
        }
    }
    fn key(&self, side: Side<'_>, sides: &[Side<'_>]) -> Option<Target> {
        match self {
            Self::Table(t) => side.ids.table_uid(t).cloned().map(Target::Table),
            Self::Column(c) => column_target(c, side, sides),
            Self::Module(m) => Some(Target::Module(m.clone())),
        }
    }
    fn subject(&self) -> String {
        match self {
            Self::Table(t) => t.raw().to_string(),
            Self::Column(c) => c.raw().to_string(),
            Self::Module(m) => m.raw().to_string(),
        }
    }
}

// Binding and effective-interface pairing must share the partition UID bridge.
// An ordinary column's own UID always wins over another column's spelling.
pub(super) fn column_target(c: &ColumnRef, side: Side<'_>, sides: &[Side<'_>]) -> Option<Target> {
    let table = side.ids.table_uid(&c.table)?.clone();
    let uid = side.ids.column_uid(c).or_else(|| {
        // Attaching retires the leaf's column UID; detaching assigns
        // one. Its table UID and effective column join the two sides.
        side.schema.tables[&c.table].partition_of.as_ref()?;
        side.column(c)?;
        sides.iter().find_map(|s| {
            s.ids
                .column_uid(&s.ids.tables.get(&table)?.column(c.name.clone()))
        })
    });
    Some(match uid {
        Some(uid) => Target::Column {
            table,
            uid: uid.clone(),
        },
        None if side.column(c).is_some_and(|(source, _)| source != *c)
            || side.schema.tables[&c.table].computed.contains_key(&c.name) =>
        {
            Target::NamedColumn {
                table,
                name: c.name.clone(),
            }
        }
        None => return None,
    })
}

fn candidates(
    text: &str,
    sides: &[Side<'_>],
    spell: &dyn Fn(&ModuleId) -> ModuleId,
) -> Result<Vec<(Object, usize)>, String> {
    let mut out = Vec::new();
    let relation = text.parse::<TableName>().ok();
    let column = text.parse::<ColumnRef>().ok();
    let routine = text
        .contains('(')
        .then(|| text.parse::<ModuleId>().ok())
        .flatten()
        .map(|m| spell(&m));
    if relation.is_none() && column.is_none() && routine.is_none() {
        return Err(
            "invalid object reference: use a qualified relation, column or routine signature"
                .into(),
        );
    }
    for (i, side) in sides.iter().enumerate() {
        // Resolve candidates, not a punctuation guess: a declared identifier
        // can itself contain parentheses. Ambiguity must name both objects.
        if let Some(m) = &routine
            && side.schema.modules.contains_key(m)
        {
            out.push((Object::Module(m.clone()), i));
        }
        if let Some(c) = &column
            && let Some(t) = side.schema.tables.get(&c.table)
            && (side.column(c).is_some() || t.computed.contains_key(&c.name))
        {
            out.push((Object::Column(c.clone()), i));
        }
        if let Some(t) = &relation {
            if side.schema.tables.contains_key(t) {
                out.push((Object::Table(t.clone()), i));
            }
            let m = ModuleId::Named(t.clone());
            if side.schema.modules.contains_key(&m) {
                out.push((Object::Module(m), i));
            }
        }
    }
    if out.is_empty() {
        return Err("names no declared object".into());
    }
    Ok(out)
}

pub(super) fn entries(
    consumers: &Consumers,
    sides: &[Side<'_>],
    spell: &dyn Fn(&ModuleId) -> ModuleId,
    bind: bool,
) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for (consumer, declaration) in consumers {
        if consumer.trim().is_empty() {
            errors.push("a consumer name must not be blank".into());
        }
        for (writes, names) in [(false, &declaration.uses), (true, &declaration.writes)] {
            let mut seen = BTreeSet::new();
            for text in names {
                let result = candidates(text, sides, spell).and_then(|objects| {
                    let keys: BTreeSet<_> = objects
                        .iter()
                        .map(|(o, i)| {
                            if bind {
                                o.key(sides[*i], sides).map(Key::Bound).ok_or_else(|| {
                                    format!("{} has no resolved identity", o.label())
                                })
                            } else {
                                Ok(Key::Declared(o.clone()))
                            }
                        })
                        .collect::<Result<_, _>>()?;
                    if keys.len() != 1 {
                        return Err(format!(
                            "is ambiguous: {}",
                            objects
                                .iter()
                                .map(|(o, _)| o.label())
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if writes
                        && objects.iter().any(|(o, i)| match o {
                            Object::Table(_) => false,
                            Object::Column(c) => {
                                let schema = sides[*i].schema;
                                let t = &schema.tables[&c.table];
                                let column = sides[*i].column(c);
                                t.computed.contains_key(&c.name)
                                    || column.is_some_and(|(_, c)| {
                                        matches!(
                                            c.value_source,
                                            pbps_model::ValueSource::Generated(_)
                                        )
                                    })
                            }
                            Object::Module(_) => true,
                        })
                    {
                        return Err("does not resolve only to a table or writable column".into());
                    }
                    let key = keys.into_iter().next().expect("candidates are nonempty");
                    if !seen.insert(key.clone()) {
                        return Err("duplicates an entry for the same object".into());
                    }
                    Ok(match key {
                        Key::Bound(target) => Some(Entry {
                            consumer: consumer.clone(),
                            text: text.clone(),
                            subject: objects[0].0.subject(),
                            writes,
                            target,
                        }),
                        Key::Declared(_) => None,
                    })
                });
                match result {
                    Ok(Some(entry)) => entries.push(entry),
                    Ok(None) => {}
                    Err(reason) => errors.push(format!(
                        "consumers.{}.{}: {}: {reason}",
                        pbps_model::escape::Escaped(consumer),
                        if writes { "writes" } else { "uses" },
                        pbps_model::escape::Escaped(text)
                    )),
                }
            }
        }
    }
    (entries, errors)
}

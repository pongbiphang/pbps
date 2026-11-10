//! The walk over a plan's steps that tracks which names are held and taken at
//! each point.

use super::*;

/// One entry of a schema's `sys.objects` namespace while a plan is walked:
/// read from the catalog, or put there by an earlier change of the plan.
#[derive(Debug, Clone)]
pub(super) struct Held {
    /// The same entry across every move the walk makes of it, so a refusal
    /// can name the later change that would have freed it (#1366).
    id: usize,
    name: TableName,
    kind: String,
    parent: Option<TableName>,
    column: Option<pbps_model::ColumnName>,
    /// Put where it is by the plan, rather than found there in the database.
    planned: bool,
}

/// A name a change claimed while another entry held it.
#[derive(Debug, Clone)]
pub(super) struct Taken {
    /// The claiming change, by its position in the walk.
    step: usize,
    /// The holder's [`Held::id`].
    holder: usize,
    line: String,
}

/// What the catalog read and the collation question answered: fixed for one
/// connected plan, and shared by every order of it that is tried (#1366).
pub(crate) struct NameFacts {
    occupants: Vec<pbps_mssql::catalog::NameOccupant>,
    /// The catalog's spelling of each name the plan asked about. The plan
    /// spells a created name its own way, and the database's collation may
    /// read it as an object the catalog spells differently.
    spelling: BTreeMap<TableName, TableName>,
    /// Each name the database reads as another, pointing at the first
    /// spelling of it, so two names meet exactly when they point at the same
    /// one.
    first: BTreeMap<TableName, TableName>,
}

impl NameFacts {
    /// `alike` is the pairs of names the plan uses that the database reads as
    /// one name under its collation, as `catalog::object_names_alike` answers
    /// (#1215); none is how a case-sensitive database answers.
    pub(crate) fn new(
        occupants: &[pbps_mssql::catalog::NameOccupant],
        alike: &[(TableName, TableName)],
    ) -> Self {
        let mut first: BTreeMap<TableName, TableName> = BTreeMap::new();
        for (earlier, later) in alike {
            let root = first.get(earlier).unwrap_or(earlier).clone();
            first.insert(later.clone(), root);
        }
        NameFacts {
            occupants: occupants.to_vec(),
            spelling: occupants
                .iter()
                .map(|o| (o.wanted.clone(), o.name.clone()))
                .collect(),
            first,
        }
    }

    fn one<'n>(&'n self, name: &'n TableName) -> &'n TableName {
        self.first.get(name).unwrap_or(name)
    }

    /// Whether where the drop of module `id` runs among `changes`' table
    /// renames can change what the walk finds (#1680 review). Only then may
    /// the rename search move it. Two things must hold:
    /// - The catalog read found an object under its name. Otherwise its drop
    ///   frees nothing (`Walk::step`).
    /// - A table rename can claim that name, under the collation. The renames
    ///   are the only changes among the drops that claim a name. Every claim
    ///   after them runs after every module drop whatever the order, and every
    ///   claim before them runs before it.
    ///
    /// A rename can claim its target, the stop a transfer passes through, the
    /// generated and fallback names of each default it moves, and, across
    /// schemas, the name of each child the read found that it carries
    /// (`Walk::rename_table`). Missing one leaves the drop where it is, as
    /// before #1680.
    // The complement is every change that is not a table rename.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn module_drop_matters(
        &self,
        changes: &[pbps_model::PlannedChange],
        id: &ModuleId,
    ) -> bool {
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        let name = module_object(id);
        if !self.occupants.iter().any(|o| o.name == name) {
            return false;
        }
        let module = self.one(&name);
        let in_schema = |t: &TableName, n: &str| TableName::new(t.schema.clone(), n);
        changes.iter().any(|p| {
            let pbps_model::Change::RenameTable {
                from, to, defaults, ..
            } = &p.change
            else {
                return false;
            };
            let mut claims = vec![to.clone()];
            for c in defaults {
                claims.push(in_schema(to, generated(to, c.raw()).raw()));
                claims.push(in_schema(to, fallback_name(to, c.raw()).raw()));
            }
            if from.schema != to.schema {
                claims.push(in_schema(to, &from.name));
                // The children the read found on this table, as
                // `carried_destinations` reads them.
                claims.extend(
                    self.occupants
                        .iter()
                        .filter(|o| o.parent.as_ref() == Some(from))
                        .map(|o| in_schema(to, &o.name.name)),
                );
            }
            claims.iter().any(|c| self.one(c) == module)
        })
    }
}

/// An entry a change puts into the namespace.
pub(super) struct Entry {
    name: TableName,
    kind: String,
    parent: Option<TableName>,
    column: Option<pbps_model::ColumnName>,
}

/// SQL Server's `sys.objects` namespace as a plan's changes move it, from
/// what the catalog read found (#1077). Each change moves it as the emitter's
/// statements do:
/// - drops remove an object, and a dropped table takes its children along;
/// - a rename moves a table; a transfer moves its children to the new schema;
/// - a table or column rename moves a generated default, found under either
///   name `pbps` may have left it under, to its new generated name when
///   nothing holds that name at that point (#975), and otherwise to the
///   digested fallback when nothing holds that (DEC-981.1);
/// - a dropped column takes its default, and a changed default is replaced;
/// - added constraints, defaults, tables and modules take their names, each
///   refused if the walk holds that name when it is added (#1201).
///
/// So a name freed before the `CREATE` runs is free, and a name something
/// moves into first is taken, whatever order the changes that do it come in.
/// Cloned to try one order of a plan without committing to it
/// (`object_order`).
#[derive(Clone)]
pub(crate) struct Walk<'f> {
    facts: &'f NameFacts,
    held: Vec<Held>,
    ids: usize,
    taken: Vec<Taken>,
    /// For each entry a change moved away or removed, the first change that
    /// did.
    freed: BTreeMap<usize, usize>,
}

impl<'f> Walk<'f> {
    pub(crate) fn new(facts: &'f NameFacts) -> Self {
        let mut walk = Walk {
            facts,
            held: Vec::new(),
            ids: 0,
            taken: Vec::new(),
            freed: BTreeMap::new(),
        };
        for o in &facts.occupants {
            if !walk.held.iter().any(|h| h.name == o.name) {
                walk.push(
                    Entry {
                        name: o.name.clone(),
                        kind: o.kind.clone(),
                        parent: o.parent.clone(),
                        column: o.parent_column.clone(),
                    },
                    false,
                );
            }
        }
        walk
    }

    /// Every change, in the order given.
    pub(crate) fn run(&mut self, changes: &[pbps_model::PlannedChange]) {
        for (step, p) in changes.iter().enumerate() {
            self.step(step, &p.change);
        }
    }

    /// Whether no change so far claimed a name another entry held.
    pub(crate) fn is_clear(&self) -> bool {
        self.taken.is_empty()
    }

    /// The namespace as it stands, independent of how each entry got there
    /// and of which order put it there: two trial orders reaching the same
    /// state have the same future.
    pub(crate) fn state(
        &self,
    ) -> Vec<(
        TableName,
        String,
        Option<TableName>,
        Option<pbps_model::ColumnName>,
    )> {
        let mut out: Vec<_> = self
            .held
            .iter()
            .map(|h| {
                (
                    h.name.clone(),
                    h.kind.clone(),
                    h.parent.clone(),
                    h.column.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn push(&mut self, entry: Entry, planned: bool) -> usize {
        let id = self.ids;
        self.ids += 1;
        self.held.push(Held {
            id,
            name: entry.name,
            kind: entry.kind,
            parent: entry.parent,
            column: entry.column,
            planned,
        });
        id
    }

    /// The entry holding `name`: the same name, the catalog's spelling of it,
    /// or one the database reads as the same, other than the entries in
    /// `except`.
    fn holder(&self, name: &TableName, except: &[usize]) -> Option<usize> {
        let spelled = self.facts.spelling.get(name);
        self.held.iter().position(|h| {
            !except.contains(&h.id)
                && (&h.name == name
                    || Some(&h.name) == spelled
                    || self.facts.one(&h.name) == self.facts.one(name))
        })
    }

    fn refuse(&mut self, step: usize, name: &TableName, at: usize) {
        let h = &self.held[at];
        let what = match &h.parent {
            Some(parent) => format!(
                "{} `{}` on `{parent}`",
                h.kind,
                h.name.shown(),
                parent = parent.shown()
            ),
            None => format!("{} `{}`", h.kind, h.name.shown()),
        };
        let line = if h.planned {
            format!(
                "`{name}`: this plan puts {what} there first",
                name = name.shown()
            )
        } else {
            format!(
                "`{name}`: the database already has {what}",
                name = name.shown()
            )
        };
        self.taken.push(Taken {
            step,
            holder: h.id,
            line,
        });
    }

    /// Every name a change adds is checked against what the walk holds at
    /// that point, not only a created table's or module's: a constraint or
    /// default the plan adds under a name it or the database already holds
    /// fails with Msg 2714 just the same (#1201).
    fn claim(&mut self, step: usize, entry: Entry) {
        if let Some(at) = self.holder(&entry.name, &[]) {
            self.refuse(step, &entry.name, at);
        }
        self.push(entry, true);
    }

    /// Removes every entry `gone` names.
    fn release(&mut self, step: usize, gone: impl Fn(&Held) -> bool) {
        for h in &self.held {
            if gone(h) {
                self.freed.entry(h.id).or_insert(step);
            }
        }
        self.held.retain(|h| !gone(h));
    }

    /// `sp_rename` of a generated default, taken only when it is at `old` on
    /// `column` of `table`: to `new` when nothing holds it, else to
    /// `fallback` when nothing holds that (the emitter's own guards, #991,
    /// DEC-981.1). A hand-named default of another column under that name is
    /// not moved.
    fn move_default(
        &mut self,
        step: usize,
        table: &TableName,
        column: &pbps_model::ColumnName,
        old: TableName,
        new: TableName,
        fallback: TableName,
    ) {
        if old == new {
            return;
        }
        let new = if self.holder(&new, &[]).is_none() {
            new
        } else if fallback != new && fallback != old && self.holder(&fallback, &[]).is_none() {
            fallback
        } else {
            return;
        };
        if let Some(h) = self.held.iter_mut().find(|h| {
            h.name == old
                && h.kind == "default constraint"
                && h.parent.as_ref() == Some(table)
                && h.column.as_ref() == Some(column)
        }) {
            h.name = new;
            h.planned = true;
            self.freed.entry(h.id).or_insert(step);
        }
    }

    fn rename_table(
        &mut self,
        step: usize,
        from: &TableName,
        to: &TableName,
        defaults: &[pbps_model::ColumnName],
    ) {
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        use pbps_mssql::emit::generated_default_names as left_under;
        let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
        // The table's own entry, where the read found it.
        let table: Vec<usize> = self
            .held
            .iter()
            .filter(|h| &h.name == from)
            .map(|h| h.id)
            .collect();
        // `ALTER SCHEMA ... TRANSFER` first, where the schema changes: the
        // table to its name in the new schema, which is in neither the
        // baseline nor the plan, and its constraints and defaults along with
        // it. Each is refused there (Msg 15530) while another object holds
        // its name, so each is a claim (#1362, #1366). A carried default
        // keeps the old table's generated name until the rename below moves
        // it, and holds that name meanwhile.
        let stop = in_schema(to, &from.name);
        let mut carried = Vec::new();
        for h in &mut self.held {
            if table.contains(&h.id) && h.name != stop {
                h.name = stop.clone();
                self.freed.entry(h.id).or_insert(step);
            }
            if h.parent.as_ref() == Some(from) {
                h.parent = Some(to.clone());
                if h.name.schema != to.schema {
                    h.name.schema = to.schema.clone();
                    h.planned = true;
                    self.freed.entry(h.id).or_insert(step);
                    carried.push(h.id);
                }
            }
        }
        if from.schema != to.schema {
            // Where the basename stays, the stop is the target, asked below.
            if from.name != to.name
                && let Some(at) = self.holder(&stop, &table)
            {
                self.refuse(step, &stop, at);
            }
            for id in carried {
                let Some(name) = self
                    .held
                    .iter()
                    .find(|h| h.id == id)
                    .map(|h| h.name.clone())
                else {
                    continue;
                };
                if let Some(at) = self.holder(&name, &[id]) {
                    self.refuse(step, &name, at);
                }
            }
        }
        // The rename's own target, held when it runs by an object other than
        // the table: the same name, or one the database reads as the same
        // (DEC-981.2). Asked after the table's constraints have moved with
        // it: a transfer runs before the `sp_rename`, so a carried check
        // `New` lands in the target schema first and holds `new` there
        // (review of #1346).
        let collides = self.holder(to, &table);
        if let Some(at) = collides {
            self.refuse(step, to, at);
        }
        // The table itself holds its new name from here on, whether or not
        // the read found it: a later change adding a name the database reads
        // as the same one fails (review of #1240).
        let mut found = false;
        for h in &mut self.held {
            if table.contains(&h.id) {
                if &h.name != to {
                    h.name = to.clone();
                    self.freed.entry(h.id).or_insert(step);
                }
                h.planned |= collides.is_some();
                found = true;
            }
        }
        if !found && !self.held.iter().any(|h| &h.name == to) {
            self.push(
                Entry {
                    name: to.clone(),
                    kind: "user table".into(),
                    parent: None,
                    column: None,
                },
                true,
            );
        }
        for column in defaults {
            for old in left_under(from, column.raw()) {
                self.move_default(
                    step,
                    to,
                    column,
                    in_schema(to, old.raw()),
                    in_schema(to, generated(to, column.raw()).raw()),
                    in_schema(to, fallback_name(to, column.raw()).raw()),
                );
            }
        }
    }

    /// One change, at position `step` of the order being walked.
    // The complement is every change that touches no object name.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn step(&mut self, step: usize, change: &pbps_model::Change) {
        use pbps_model::Change;
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        use pbps_mssql::emit::generated_default_names as left_under;
        let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
        let default_of = |table: &TableName, column: &str| Entry {
            name: in_schema(table, generated(table, column).raw()),
            kind: "default constraint".into(),
            parent: Some(table.clone()),
            column: Some(column.into()),
        };
        let constraint = |table: &TableName, name: &str, kind: &str| Entry {
            name: in_schema(table, name),
            kind: kind.into(),
            parent: Some(table.clone()),
            column: None,
        };
        let object = |name: &TableName, kind: &str| Entry {
            name: name.clone(),
            kind: kind.into(),
            parent: None,
            column: None,
        };
        match change {
            Change::DropModule { id, .. } => {
                let name = module_object(id);
                self.release(step, |h| h.name == name);
            }
            Change::DropTable { name, .. } => {
                self.release(step, |h| &h.name == name || h.parent.as_ref() == Some(name));
            }
            Change::RenameTable {
                from, to, defaults, ..
            } => self.rename_table(step, from, to, defaults),
            Change::RenameColumn {
                table,
                from,
                to,
                table_was,
                ..
            } => {
                let new = in_schema(table, generated(table, to.raw()).raw());
                let fallback = in_schema(table, fallback_name(table, to.raw()).raw());
                for was in std::iter::once(table).chain(table_was) {
                    // Still under `from`: the walk renames the column's
                    // entries below, after the default has moved.
                    for old in left_under(was, from.raw()) {
                        self.move_default(
                            step,
                            table,
                            from,
                            in_schema(table, old.raw()),
                            new.clone(),
                            fallback.clone(),
                        );
                    }
                }
                for h in &mut self.held {
                    if h.parent.as_ref() == Some(table) && h.column.as_ref() == Some(from) {
                        h.column = Some(to.clone());
                    }
                }
            }
            Change::DropColumn { column, .. } => self.release(step, |h| {
                h.parent.as_ref() == Some(&column.table) && h.column.as_ref() == Some(&column.name)
            }),
            Change::AlterColumnDefault {
                column, from, to, ..
            } => {
                if from.is_some() {
                    self.release(step, |h| {
                        h.parent.as_ref() == Some(&column.table)
                            && h.column.as_ref() == Some(&column.name)
                    });
                }
                if to.is_some() {
                    self.claim(step, default_of(&column.table, column.name.raw()));
                }
            }
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } if (match column.value_source {
                pbps_model::ValueSource::Default(_) => true,
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => false,
            }) =>
            {
                self.claim(step, default_of(table, name.raw()))
            }
            Change::DropUnique { table, name }
            | Change::DropForeignKey { table, name }
            | Change::DropCheck { table, name } => {
                let name = in_schema(table, name.raw());
                self.release(step, |h| h.name == name);
            }
            Change::AddUnique { table, name, .. } => {
                self.claim(step, constraint(table, name.raw(), "unique constraint"));
            }
            Change::AddForeignKey { table, name, .. } => {
                self.claim(
                    step,
                    constraint(table, name.raw(), "foreign key constraint"),
                );
            }
            Change::AddCheck { table, name, .. } => {
                self.claim(step, constraint(table, name.raw(), "check constraint"));
            }
            Change::SetPrimaryKey {
                table, from, to, ..
            } => {
                match from
                    .as_ref()
                    .map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    Some(Some(name)) => {
                        let name = in_schema(table, name);
                        self.release(step, |h| h.name == name);
                    }
                    // An unnamed key has the name the server gave it, which
                    // the emitter looks up by table; so does the walk.
                    Some(None) => self.release(step, |h| {
                        h.kind == "primary key constraint" && h.parent.as_ref() == Some(table)
                    }),
                    None => {}
                }
                if let Some(name) = to
                    .as_ref()
                    .and_then(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    self.claim(step, constraint(table, name, "primary key constraint"));
                }
            }
            Change::CreateTable { name, table, .. } => {
                self.claim(step, object(name, "user table"));
                // The history is created by the same statement (#1176).
                if let Some(v) = table
                    .system_time
                    .as_ref()
                    .and_then(|st| st.versioning.as_ref())
                {
                    self.claim(step, object(&v.history, "user table"));
                }
                // Its own constraints and generated defaults take their names
                // with it, for whatever the plan creates after.
                for (column, spec) in &table.columns {
                    if match spec.value_source {
                        pbps_model::ValueSource::Default(_) => true,
                        pbps_model::ValueSource::None
                        | pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => false,
                    } {
                        self.claim(step, default_of(name, column.raw()));
                    }
                }
                let named = table
                    .primary_key
                    .iter()
                    .filter_map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                    .map(|n| (n, "primary key constraint"))
                    .chain(table.unique.keys().map(|n| (n.raw(), "unique constraint")))
                    .chain(
                        table
                            .foreign_keys
                            .keys()
                            .map(|n| (n.raw(), "foreign key constraint")),
                    )
                    .chain(table.checks.keys().map(|n| (n.raw(), "check constraint")));
                for (n, kind) in named {
                    self.claim(step, constraint(name, n, kind));
                }
            }
            Change::CreateModule { id, module } => {
                self.claim(step, object(&module_object(id), &module.kind.to_string()));
            }
            _ => {}
        }
    }

    /// The refusal for the names this walk met held, over the `changes` it
    /// walked. A holder a later change of the plan moves away or removes is
    /// named with that change: the plan could not run it first (#1366).
    pub(crate) fn refusal(
        &self,
        changes: &[pbps_model::PlannedChange],
        label: &str,
    ) -> anyhow::Error {
        let mut later = false;
        let lines: Vec<String> = self
            .taken
            .iter()
            .map(|t| match self.freed.get(&t.holder) {
                Some(&at) if at > t.step => {
                    later = true;
                    let change = &changes[at].change;
                    format!(
                        "{}, which a later change frees: `{}` {}",
                        t.line,
                        pbps_model::escape::Escaped(&change.subject()),
                        crate::report::describe(change)
                    )
                }
                _ => t.line.clone(),
            })
            .collect();
        let mut message = format!(
            "`{label}` already uses {} name(s) this plan would create, for objects that share \
             one namespace per schema on SQL Server:\n  {}\nEach `CREATE` would fail at apply, \
             or replace an object this project does not record. Rename the declaration, drop or \
             rename the object in the database, or — for a table or module pbps can read — \
             adopt it as it stands with `pbps baseline --reason ...`.",
            self.taken.len(),
            lines.join("\n  "),
            label = pbps_model::escape::Escaped(label),
        );
        if later {
            message.push_str(
                "\nA name a later change frees is taken first only where no order of the plan \
                 frees it in time: the change that frees it runs after table renames and drops \
                 (DECISIONS 496), or two changes each wait for the other. Deploy the change \
                 that frees the name first, in a plan of its own, then this one (DEC-1366.2).",
            );
        }
        anyhow::anyhow!(message)
    }
}

/// Refuses a name this plan creates on SQL Server that another object in
/// the schema's `sys.objects` namespace holds when the `CREATE` runs (#1077):
/// a sequence, a synonym, a constraint, a default, or a table or module this
/// project does not record. `CREATE TABLE` there fails with Msg 2714, and
/// `CREATE OR ALTER` fails or replaces what is there.
///
/// The plan is walked in its own order, which is the order it runs in
/// (`order_key`), over what the catalog read found ([`Walk`]).
///
/// The connected check calls [`refuse_occupied_objects_under`] with the
/// database's answer on which names are one; this is that call with none,
/// which is how a case-sensitive database answers.
#[cfg(test)]
pub(crate) fn refuse_occupied_objects(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
    label: &str,
) -> anyhow::Result<()> {
    refuse_occupied_objects_under(cs, occupants, &[], label)
}

/// [`refuse_occupied_objects`], with `alike`: the pairs of names the plan
/// uses that the database reads as one name under its collation, as
/// `catalog::object_names_alike` answers (#1215). Two names the plan itself
/// adds have no catalog spelling to meet at, so without this `Ck_Name` and
/// `ck_name` passed as two names and the second failed at apply.
#[cfg(test)]
pub(crate) fn refuse_occupied_objects_under(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
    alike: &[(TableName, TableName)],
    label: &str,
) -> anyhow::Result<()> {
    let facts = NameFacts::new(occupants, alike);
    let mut walk = Walk::new(&facts);
    walk.run(&cs.changes);
    if walk.is_clear() {
        return Ok(());
    }
    Err(walk.refusal(&cs.changes, label))
}

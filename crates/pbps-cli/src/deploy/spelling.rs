//! Refusals for a declared name the database spells differently.

use super::*;

pub(crate) fn warn_unasked(unasked: &[String]) {
    for w in unasked {
        eprintln!("warning: {}", w);
    }
}

/// Refuses a declaration whose text the engine would not read back as
/// written, before anything is written (DECISIONS 101).
///
/// Asked of the engine rather than reasoned about: `"1.5"` in a
/// `decimal(5,2)` comes back `1.50`, `"ab "` in a `char(5)` comes back `ab`,
/// and a text the type cannot read at all comes back as a failed insert —
/// each a plan that never converges or never applies, and each a question
/// only the engine answers the same way it will answer at read time.
///
/// Hands back, as warnings, what it could not ask: a partition's own default it
/// could not deparse (DEC-1609.1). Not a finding and not a pass — the apply's
/// closing check still refuses one stored as the parent's, after its
/// statement — so each caller reports them where its other warnings go: a
/// connected plan in its JSON envelope (#1660), the rest on stderr through
/// [`warn_unasked`]. They go to `unasked` whether or not it refuses: beside
/// the error, never inside it, so a refusal is the plain error every caller
/// and every downcast already knows, and the caller writes the warnings
/// wherever it writes the refusal (#1702, DEC-1726.1).
///
/// What it hands back, when it does not refuse, is `schema` as the engine
/// reads it: every date or time value it read one way only in the engine's
/// own spelling, cells, keys and bounds alike (DEC-1883.1). Every caller
/// plans, compares and records that schema from here on, never the one it
/// passed in — a plan written from the declared text would restate, on every
/// plan after, a value the table already holds.
pub(crate) async fn refuse_misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &pbps_db::catalog::CatalogNames,
    unasked: &mut Vec<String>,
    type_routines_need_review: bool,
) -> anyhow::Result<Schema> {
    let found = crate::engine::misspelt(conn, schema, at, type_routines_need_review).await;
    refuse_misspelt_read(found, schema, unasked)
}

/// What [`refuse_misspelt`] does with the engine's answer, for a caller that
/// asked at its own position of the engine (`plan --db`, whose probe schema is
/// not the declared one and whose engine keeps the review flag itself). The
/// answer is `found`; `schema` is what gets read back.
pub(super) fn refuse_misspelt_read(
    found: Result<pbps_db::catalog::Spellings, pbps_db::catalog::RowsError>,
    schema: &Schema,
    unasked: &mut Vec<String>,
) -> anyhow::Result<Schema> {
    let mut found = found.context("cannot ask the engine how it reads the declared rows")?;
    unasked.extend(found.defaults_unasked.iter().map(|why| {
        format!(
            "not checked before the plan whether the engine stores it as the parent's \
             default: {why}"
        )
    }));
    // A date or time the engine reads, and reads one way only — the probe
    // has already moved every text with two readings to `ambiguous` — is the
    // value it reads (DEC-1883.1). Every other type keeps DECISIONS 101.
    let (cells, misspelt): (Vec<_>, Vec<_>) = std::mem::take(&mut found.misspelt)
        .into_iter()
        .partition(|m| m.temporal && m.canonical.is_some());
    found.misspelt = misspelt;
    let (bounds, misspelt_bounds): (Vec<_>, Vec<_>) = std::mem::take(&mut found.bounds)
        .into_iter()
        .partition(|b| b.temporal && b.canonical.is_some());
    found.bounds = misspelt_bounds;
    if found.misspelt.is_empty()
        && found.conflicts.is_empty()
        && found.bounds.is_empty()
        && found.list_collisions.is_empty()
        && found.defaults_as_parents.is_empty()
        && found.ambiguous.is_empty()
    {
        return Ok(as_read(schema, &cells, &bounds));
    }
    // Two keys the engine reads as one row would insert twice and fail on
    // the second; the alias check (74) cannot see them on a table that holds
    // neither yet (DECISIONS 106).
    let mut lines: Vec<String> = found.conflicts.iter().map(ToString::to_string).collect();
    lines.extend(
        found
            .misspelt
            .iter()
            .map(|m| match (&m.column, &m.canonical) {
                (None, None) => format!(
                    "{} row key `{}` cannot be read as {} by the engine",
                    m.table.shown(),
                    m.key,
                    m.ty
                ),
                (None, Some(canonical)) => format!(
                    "{} row key `{}` is read back by the engine as {:?}; write it that way",
                    m.table.shown(),
                    m.key,
                    canonical
                ),
                (Some(c), None) => format!(
                    "{} row `{}`: `{c}` = {:?} cannot be read as {} by the engine",
                    m.table.shown(),
                    m.key,
                    m.declared,
                    m.ty,
                    c = c.shown(),
                ),
                (Some(c), Some(canonical)) => format!(
                    "{} row `{}`: `{c}` is written {:?}, and the engine reads it back as {:?}; \
                 write it that way",
                    m.table.shown(),
                    m.key,
                    m.declared,
                    canonical,
                    c = c.shown(),
                ),
            }),
    );
    // A bound the engine stores in its own spelling would differ from the
    // declaration on every plan after (#1170).
    lines.extend(found.bounds.iter().map(|b| match &b.canonical {
        None => format!(
            "partition {} bound on `{}`: {:?} cannot be read as {} by the engine",
            b.partition.shown(),
            b.column.shown(),
            b.declared,
            b.ty
        ),
        Some(canonical) => format!(
            "partition {} bound on `{}` is written {:?}, and the engine reads it back as \
             {canonical:?}; write it that way",
            b.partition.shown(),
            b.column.shown(),
            b.declared
        ),
    }));
    // A partition's own default stored as its parent's reads back as none of
    // its own, so every plan would set it again (#1609).
    lines.extend(found.defaults_as_parents.iter().map(|d| {
        format!(
            "partition {} column `{}`: its own default {:?} is stored as {:?}, which is its \
             parent's; drop the partition's own default, it is the parent's",
            d.partition.shown(),
            d.column.shown(),
            d.declared,
            d.stored
        )
    }));
    let mut sections = Vec::new();
    if !lines.is_empty() {
        sections.push(format!(
            "{} declared value(s) would not come back as written:\n  {}\n\
             A declaration that disagrees with its own database on every plan is worse \
             than none; the engine's spelling is the one to write (DECISIONS 101, 106).",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    // Two partitions taking one value by the key type's own equality, which
    // validation's text comparison cannot see: the engine refuses the second
    // as overlapping the first. Its own section, because the remedy is not a
    // spelling: both texts may be the engine's own (#1632).
    if !found.list_collisions.is_empty() {
        let lines: Vec<String> = found
            .list_collisions
            .iter()
            .map(|c| {
                format!(
                    "`{}` takes {:?} and `{}` takes {:?} of `{}`",
                    c.first.0.shown(),
                    c.first.1,
                    c.second.0.shown(),
                    c.second.1,
                    c.parent.shown()
                )
            })
            .collect();
        sections.push(format!(
            "{} pair(s) of list values are one value of the partition key's type:\n  {}\n\
             A value routes to one partition, so the engine refuses the second; list it in \
             one partition only.",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    // A section of its own, because its remedy is the opposite of the one
    // above: not "write what the engine reads", which would name the pinned
    // reading as the meant one, but "write it so that there is one reading"
    // (#1756, DEC-1756.1).
    if !found.ambiguous.is_empty() {
        let lines: Vec<String> = found.ambiguous.iter().map(ambiguous_line).collect();
        sections.push(format!(
            "{} declared date/time value(s) can be read more than one way:\n  {}\n\
             What such a text means is decided by the settings of the session that reads \
             it; this tool's would store one reading without saying which (DEC-1756.1).",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    bail!("{}", sections.join("\n\n"))
}

/// `schema` with each date or time the engine read in place of the text that
/// declared it (DEC-1883.1): a cell's value, a row's key, a partition's bound.
///
/// The cells go first, found under the keys as declared, and only then are
/// the keys themselves replaced. Two declared keys the engine reads as one are
/// never here: they are a conflict, refused before this is reached (DECISIONS
/// 106), so a replaced key cannot land on another row.
pub(super) fn as_read(
    schema: &Schema,
    cells: &[pbps_db::catalog::Misspelt],
    bounds: &[pbps_db::catalog::MisspeltBound],
) -> Schema {
    let mut out = schema.clone();
    for m in cells {
        let (Some(column), Some(canonical)) = (&m.column, &m.canonical) else {
            continue;
        };
        if let Some(row) = out
            .tables
            .get_mut(&m.table)
            .and_then(|t| t.data.as_mut())
            .and_then(|d| d.rows.get_mut(&m.key))
        {
            row.0
                .insert(column.clone(), pbps_model::Value::Text(canonical.clone()));
        }
    }
    for m in cells {
        let (None, Some(canonical)) = (&m.column, &m.canonical) else {
            continue;
        };
        if let Some(data) = out.tables.get_mut(&m.table).and_then(|t| t.data.as_mut())
            && let Some(row) = data.rows.remove(&m.key)
        {
            data.rows
                .insert(pbps_model::RowKey::from(canonical.as_str()), row);
        }
    }
    for b in bounds {
        let Some(canonical) = &b.canonical else {
            continue;
        };
        let Some(of) = out
            .tables
            .get(&b.partition)
            .and_then(|t| t.partition_of.as_ref())
        else {
            continue;
        };
        let Some(position) = schema
            .tables
            .get(&of.parent)
            .and_then(|p| p.partition_by.as_ref())
            .and_then(|by| by.columns.iter().position(|c| *c == b.column))
        else {
            continue;
        };
        match out
            .tables
            .get_mut(&b.partition)
            .and_then(|t| t.partition_of.as_mut())
            .map(|of| &mut of.bound)
        {
            // Both ends, wherever the declared text stands: the same text is
            // the same reading.
            Some(pbps_model::PartitionBound::Range { from, to }) => {
                for datum in [from.get_mut(position), to.get_mut(position)]
                    .into_iter()
                    .flatten()
                {
                    if *datum == pbps_model::BoundDatum::Value(b.declared.clone()) {
                        *datum = pbps_model::BoundDatum::Value(canonical.clone());
                    }
                }
            }
            // Every value of a list is its one key column's (#1632).
            Some(pbps_model::PartitionBound::List { values }) if position == 0 => {
                if values.remove(&pbps_model::ListDatum::Value(b.declared.clone())) {
                    values.insert(pbps_model::ListDatum::Value(canonical.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

/// One ambiguous literal, where it is and what it can mean.
pub(super) fn ambiguous_line(a: &pbps_db::catalog::Ambiguous) -> String {
    use pbps_db::catalog::LiteralAt;
    let at = match &a.at {
        LiteralAt::Cell {
            table,
            key,
            column: None,
        } => format!("{table} row key `{key}`", table = table.shown()),
        LiteralAt::Cell {
            table,
            key,
            column: Some(c),
        } => format!(
            "{table} row `{key}`: `{c}`",
            table = table.shown(),
            c = c.shown(),
        ),
        LiteralAt::Bound { partition, column } => {
            format!(
                "partition {partition} bound on `{column}`",
                partition = partition.shown(),
                column = column.shown(),
            )
        }
        LiteralAt::Default { table, column } => {
            format!(
                "{table} column `{column}` default",
                table = table.shown(),
                column = column.shown(),
            )
        }
    };
    match &a.pinned {
        None => format!(
            "{at} {:?} is a {} decided by the moment the statement runs, not by its text; {}",
            a.declared, a.ty, a.remedy
        ),
        Some(pinned) => {
            let otherwise: Vec<String> = a
                .otherwise
                .iter()
                .map(|(reading, under)| format!("{reading} under {under}"))
                .collect();
            format!(
                "{at} {:?} is read as {pinned} under this tool's settings, but as {}; {}",
                a.declared,
                otherwise.join(", and as "),
                a.remedy
            )
        }
    }
}

/// Every schema name a declaration spells, and one declaration that spells it
/// that way — for the message, so the refusal names something the author can
/// find in a file rather than a bare schema name.
pub(crate) fn schemas_declared(schema: &Schema) -> BTreeMap<String, String> {
    let Schema {
        tables,
        types,
        search,
        modules,
        roles,
        extensions,
    } = schema;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for name in search.keys() {
        out.entry(name.name.schema().to_owned())
            .or_insert_with(|| format!("the schema of {}", name.shown()));
    }
    for (name, definition) in extensions {
        out.entry(definition.schema.clone())
            .or_insert_with(|| format!("the schema of extension `{}`", name.shown()));
    }
    for (name, role) in roles {
        for target in role.grants.keys() {
            if let pbps_model::GrantTarget::Schema(s) = target {
                out.entry(s.clone())
                    .or_insert_with(|| format!("granted to role `{name}`", name = name.shown(),));
            }
        }
    }
    // The schema half of a qualified name is text on both sides too, and the
    // uid that matches the *table* does not match it: the managed set is
    // scoped by name, so `DBO.customer` on a database whose schema is `dbo`
    // is bootstrapped, recorded as a state holding no tables at all, and
    // reported as drift by the `verify` immediately after.
    for name in tables.keys() {
        out.entry(name.schema.clone())
            .or_insert_with(|| format!("the schema of `{name}`", name = name.shown()));
    }
    for id in modules.keys() {
        out.entry(id.schema().to_owned())
            .or_insert_with(|| format!("the schema of `{id}`", id = id.shown()));
    }
    for name in types.keys() {
        out.entry(name.schema().to_owned())
            .or_insert_with(|| format!("the schema of type `{}`", name.shown()));
    }
    // A history is created in its own schema, spelled as declared, and read
    // back as the database spells it (#1176, #1501 review).
    for (name, table) in tables {
        if let Some(v) = table
            .system_time
            .as_ref()
            .and_then(|st| st.versioning.as_ref())
        {
            out.entry(v.history.schema.clone()).or_insert_with(|| {
                format!(
                    "the schema of `{name}`'s history table",
                    name = name.shown()
                )
            });
        }
    }
    out
}

/// The declared schema names the database spells differently, one line each.
///
/// Absent is not misspelt: `schema::legacy` on a database that has no such
/// schema is the pre-flight probe's refusal, and that one says to create it.
pub(super) fn wrongly_spelt(
    spelled: &BTreeMap<String, Option<String>>,
    declared_by: &BTreeMap<String, String>,
) -> Vec<String> {
    spelled
        .iter()
        .filter_map(|(declared, found)| match found {
            Some(found) if found != declared => Some(format!(
                "`{declared}` ({}) is written `{found}` by the database",
                declared_by.get(declared).map_or("declared", String::as_str)
            )),
            _ => None,
        })
        .collect()
}

/// Refuses a schema name the database spells differently, before a plan is
/// written that could never converge (DECISIONS 142).
///
/// The sibling of `refuse_misspelt`, for the names in a declaration with no
/// identity behind them. A table is matched by uid, so its own case is
/// whatever the ids file recorded; the schema it lives in, and the schema a
/// `schema::` grant names, are text on both sides. On a case-insensitive
/// database `schema::DBO` grants successfully, reads back as `dbo`, and is
/// revoked and granted again by every plan after; `DBO.customer` is created
/// as `dbo.customer` and then falls outside the managed set the state
/// records by name.
pub(crate) async fn refuse_wrongly_spelt_schemas(
    conn: &mut Conn,
    schema: &Schema,
) -> anyhow::Result<()> {
    let wanted = schemas_wanted(schema);
    let spelled = crate::engine::schema_spellings(conn, &wanted).await;
    refuse_wrongly_spelt(spelled, schema)
}

/// The schema names a declaration holds, which the database is asked to spell.
pub(super) fn schemas_wanted(schema: &Schema) -> std::collections::BTreeSet<String> {
    schemas_declared(schema).keys().cloned().collect()
}

/// What [`refuse_wrongly_spelt_schemas`] does with the answer, for a caller
/// that asked at its own position of the engine.
pub(super) fn refuse_wrongly_spelt(
    spelled: Result<std::collections::BTreeMap<String, Option<String>>, pbps_db::DbError>,
    schema: &Schema,
) -> anyhow::Result<()> {
    let declared_by = schemas_declared(schema);
    let spelled = spelled.context("cannot ask the engine how it spells the declared schemas")?;
    let wrong = wrongly_spelt(&spelled, &declared_by);
    if wrong.is_empty() {
        return Ok(());
    }
    bail!(
        "{} declared schema name(s) the database spells differently:\n  {}\n\
         The objects would be created, granted and read back under the database's spelling, \
         so every plan after would disagree with the declaration that produced it; write the \
         name the way the database does (DECISIONS 142).",
        wrong.len(),
        wrong.join("\n  ")
    );
}

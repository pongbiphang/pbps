//! DTO to domain model, extracting the one-shot intent annotations along the way.
//!
//! Errors are always **collected and returned together**, never aborted on the
//! first one — the user should see everything that needs fixing in one pass
//! instead of fixing one and running again.

use serde_saphyr::Spanned;
use std::str::FromStr;

use pbps_model::{
    CheckConstraint, Clustered, Column, ColumnType, DataMode, ForeignKey, GrantTarget, Identity,
    Index, IndexColumn, Intent, Module, ModuleId, ModuleKind, Permission, PrimaryKey,
    ReplicaIdentity, Role, Row, RowKey, Strategy, Table, TableData, TableName, UniqueConstraint,
    Value,
};

use crate::dto::{
    ClusteredDto, DataDto, ModuleDto, PrimaryKeyDto, ReplicaIdentityDto, RoleDto, StorageValueDto,
    SystemTimeDto, TableDto, UniqueDto, ValueDto,
};
use crate::error::{LoadError, SourceFile, to_span};

/// The result of loading one declaration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTable {
    pub name: TableName,
    pub table: Table,
    pub intents: Vec<Intent>,
    /// `None` when the file declares no `strategy:` block. Kept out of `table`
    /// so that `Schema` equality stays a question about the database alone.
    pub strategy: Option<Strategy>,
    /// The partitions this file declares under its table (#1170): each a
    /// table of its own, in the parent's schema unless named otherwise.
    pub partitions: Vec<(TableName, Table)>,
}

/// The result of loading one module declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModule {
    pub id: ModuleId,
    pub module: Module,
    /// Kept out of `module` so that `Schema` equality stays a question about
    /// the database alone: creation order is invisible there (ADR-0002).
    pub depends_on: std::collections::BTreeSet<ModuleId>,
    /// Whether the declaration asks for the engine's default `EXECUTE` to
    /// `PUBLIC` to be left standing. Kept out of `module` for the same
    /// reason, from the other side: what `PUBLIC` holds is never compared
    /// (ADR-0010 §5, DECISIONS 371), so a module carrying it would stop
    /// matching the identical module read back from the catalog.
    pub public_execute: bool,
}

/// The result of loading one role declaration (ADR-0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRole {
    pub name: String,
    pub role: Role,
    /// A `renamed_from:`, extracted rather than stored — the same one-shot
    /// rule tables follow.
    pub intents: Vec<Intent>,
}

/// DTO to domain model for one role.
///
/// The name is a bare identifier: a role is a database principal, not an
/// object in a schema, so a dot in it is almost certainly a table name typed
/// into the wrong file.
pub fn convert_role(src: &SourceFile, dto: RoleDto) -> Result<LoadedRole, Vec<LoadError>> {
    let mut errs = Vec::new();
    let mut intents = Vec::new();

    // Verbatim, and `trim()` only to ask whether there is a name at all.
    // Measured: SQL Server stores `CREATE ROLE [ app_pad ]` with its padding,
    // `needs_quotes` refuses any scalar that is not its own `trim()` so `pull`
    // writes it back quoted, and YAML hands it here intact — trimming it made
    // a freshly pulled project name a role the database does not have, while
    // the ids file named the one it does (DECISIONS 177).
    let name = dto.role.value.clone();
    // A dot is not refused: a role is not in a schema, but `[app.reader]`
    // is a legal principal name, the emitter quotes it, and `pull` writes
    // it back as it is — refusing it here made a freshly pulled project
    // fail to load.
    if name.trim().is_empty() {
        errs.push(LoadError::semantic(
            src,
            to_span(&dto.role.defined),
            "a role must have a name",
            "empty",
        ));
    }

    if let Some(from) = &dto.renamed_from {
        // The same, for the same reason: this names a role the database has,
        // and a trimmed one asks it to rename a principal that is not there.
        let from = from.value.clone();
        if !name.trim().is_empty() {
            intents.push(Intent::RenameRole {
                from,
                to: name.clone(),
            });
        }
    }

    let mut role = Role {
        description: dto.description,
        grants: Default::default(),
    };
    // The spelling each parsed target was first written in: `SCHEMA::dbo`
    // and `schema::dbo` are one target, and the map would keep whichever
    // came last — with the other's permissions gone, and the next connected
    // plan revoking them (DECISIONS 126).
    let mut spelled: std::collections::BTreeMap<GrantTarget, &str> = Default::default();
    for (target, permissions) in &dto.grants {
        let written = target.as_str();
        let target = match GrantTarget::from_str(target) {
            Ok(t) => t,
            Err(e) => {
                errs.push(
                    LoadError::semantic(
                        src,
                        to_span(&dto.role.defined),
                        format!("invalid grant target `{target}`: {e}"),
                        e.to_string(),
                    )
                    .with_help("a target is `schema.object` or `schema::name`"),
                );
                continue;
            }
        };
        if let Some(first) = spelled.insert(target.clone(), written) {
            errs.push(
                LoadError::semantic(
                    src,
                    to_span(&dto.role.defined),
                    format!("`{written}` and `{first}` name the same grant target"),
                    "listed twice",
                )
                .with_help("keep one entry per target, with every permission in its list"),
            );
            continue;
        }
        if permissions.is_empty() {
            errs.push(LoadError::semantic(
                src,
                to_span(&dto.role.defined),
                format!("`{target}` is listed with no permission"),
                "empty list",
            ));
            continue;
        }
        let mut set = std::collections::BTreeSet::new();
        for p in permissions {
            match parse_at::<Permission>(src, p, "invalid permission") {
                Ok(p) => {
                    set.insert(p);
                }
                Err(e) => errs.push(e),
            }
        }
        role.grants.insert(target, set);
    }

    if errs.is_empty() {
        Ok(LoadedRole {
            name,
            role,
            intents,
        })
    } else {
        Err(errs)
    }
}

/// Parses a `Spanned` string, labelling any failure on that value.
fn parse_at<T>(src: &SourceFile, v: &Spanned<String>, what: &str) -> Result<T, LoadError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    T::from_str(&v.value).map_err(|e| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("{what}: {e}"),
            e.to_string(),
        )
    })
}

pub fn convert(src: &SourceFile, dto: TableDto) -> Result<LoadedTable, Vec<LoadError>> {
    let mut errs = Vec::new();
    let mut intents = Vec::new();

    let name: Option<TableName> = match parse_at(src, &dto.table, "invalid table name") {
        Ok(n) => Some(n),
        Err(e) => {
            errs.push(e.with_help(
                "a table name must have the two parts `schema.table`, e.g. `dbo.customer`",
            ));
            None
        }
    };

    if let (Some(name), Some(from)) = (&name, &dto.renamed_from) {
        match parse_at::<TableName>(src, from, "invalid table name in renamed_from") {
            Ok(from) => intents.push(Intent::RenameTable {
                from,
                to: name.clone(),
            }),
            Err(e) => errs.push(e),
        }
    }

    let mut columns = indexmap::IndexMap::with_capacity(dto.columns.len());
    for (col_name, c) in dto.columns {
        // A column name is a YAML mapping key, never parsed the way `table:`
        // is, so a `.` in it reaches here unchecked. `ColumnRef` joins
        // `schema.table.column` with the same character it splits on to read
        // one back, and a column literally named `a.b` on `dbo.customer`
        // would serialize as `dbo.customer.a.b` — indistinguishable, once
        // written, from a mistyped five-part name (issue #108). Caught here,
        // at the one place this name is minted, rather than left for the ids
        // file or a saved plan to fail on reading its own output back with an
        // error that names neither the column nor this file.
        //
        // Asked before the type, and neither answer stops the other: the two
        // are independent, and a type error that `continue`d first hid a
        // bad name until the next validation cycle (#410).
        let named = pbps_model::check_segment(&col_name).map_err(|e| {
            errs.push(
                LoadError::semantic(
                    src,
                    to_span(&c.ty.defined),
                    format!("column `{col_name}`: {e}"),
                    e.to_string(),
                )
                .with_help(
                    "rename the column without a `.`; pbps uses it to separate schema, table \
                     and column and cannot store one inside a name",
                ),
            );
        });
        let ty = parse_at::<ColumnType>(src, &c.ty, "invalid type").map_err(|e| errs.push(e));
        let (Ok(()), Ok(ty)) = (named, ty) else {
            continue;
        };

        if let (Some(table), Some(from)) = (&name, &c.renamed_from) {
            intents.push(Intent::RenameColumn {
                table: table.clone(),
                from: from.value.clone(),
                to: col_name.clone(),
            });
        }

        columns.insert(
            col_name,
            Column {
                ty,
                nullable: c.nullable,
                default: c.default,
                identity: c
                    .identity
                    .map(|[seed, increment]| Identity { seed, increment }),
                description: c.description,
                deprecated: c.deprecated,
                collation: c.collation.map(pbps_model::Collation::new),
                generated: c.generated.map(|g| pbps_model::Generated {
                    expression: g.expression,
                    stored: g.stored,
                }),
            },
        );
    }

    // A key's and a unique constraint's index is a B-tree (#1442).
    let btree = |n: &str, v: &str| {
        pbps_model::storage::canonical_index(pbps_model::IndexMethod::Btree, n, v)
    };
    let btree_help = "a B-tree index parameter: `fillfactor` or `deduplicate_items`";
    let primary_key = match dto.primary_key {
        None => None,
        Some(PrimaryKeyDto::Columns(columns)) => Some(PrimaryKey {
            name: None,
            columns,
            storage_parameters: Default::default(),
        }),
        Some(PrimaryKeyDto::Spec(spec)) => Some(PrimaryKey {
            name: spec.name,
            columns: spec.columns,
            storage_parameters: storage_parameters_of(
                src,
                &spec.storage_parameters,
                &btree,
                btree_help,
                &mut errs,
            ),
        }),
    };

    let mut unique = std::collections::BTreeMap::new();
    for (k, u) in dto.unique {
        let constraint = match u {
            UniqueDto::Columns(columns) => UniqueConstraint {
                columns,
                storage_parameters: Default::default(),
            },
            UniqueDto::Spec(spec) => UniqueConstraint {
                columns: spec.columns,
                storage_parameters: storage_parameters_of(
                    src,
                    &spec.storage_parameters,
                    &btree,
                    btree_help,
                    &mut errs,
                ),
            },
        };
        unique.insert(k, constraint);
    }

    let mut foreign_keys = std::collections::BTreeMap::new();
    for (k, fk) in dto.foreign_keys {
        match parse_reference(src, &fk.references) {
            Ok((references_table, references_columns)) => {
                foreign_keys.insert(
                    k,
                    ForeignKey {
                        columns: fk.columns,
                        references_table,
                        references_columns,
                        on_delete: fk.on_delete,
                        on_update: fk.on_update,
                    },
                );
            }
            Err(e) => errs.push(e),
        }
    }

    let checks = dto
        .checks
        .into_iter()
        .map(|(k, expression)| (k, CheckConstraint { expression }))
        .collect();

    let indexes = convert_indexes(src, dto.indexes, &mut errs);

    let data = match dto.data {
        Some(d) => match convert_data(src, d) {
            Ok(d) => Some(d),
            Err(e) => {
                errs.extend(e);
                None
            }
        },
        None => None,
    };

    let storage_parameters = storage_parameters_of(
        src,
        &dto.storage_parameters,
        &|n, v| pbps_model::storage::canonical(n, v),
        "a PostgreSQL heap storage parameter such as `fillfactor: 70` or \
         `autovacuum_enabled: false`; `toast.*` parameters are not declared",
        &mut errs,
    );

    let system_time = dto.system_time.and_then(|st| {
        let SystemTimeDto {
            period: [start, end],
            hidden,
            versioning,
        } = *st;
        let versioning = match versioning {
            None => None,
            Some(v) => {
                let history = TableName::from_str(v.history.value.trim()).map_err(|e| {
                    LoadError::semantic(
                        src,
                        to_span(&v.history.defined),
                        format!("invalid history table: {e}"),
                        "not a `schema.table` name",
                    )
                });
                let retention = v
                    .retention
                    .as_ref()
                    .map(|r| {
                        r.value.parse::<pbps_model::Retention>().map_err(|e| {
                            LoadError::semantic(
                                src,
                                to_span(&r.defined),
                                e.clone(),
                                "not a retention period",
                            )
                        })
                    })
                    .transpose();
                match (history, retention) {
                    (Ok(history), Ok(retention)) => {
                        Some(pbps_model::SystemVersioning { history, retention })
                    }
                    (history, retention) => {
                        errs.extend(history.err());
                        errs.extend(retention.err());
                        return None;
                    }
                }
            }
        };
        Some(pbps_model::SystemTime {
            start,
            end,
            hidden,
            versioning,
        })
    });

    let partition_by = dto
        .partition_by
        .map(|columns| pbps_model::PartitionBy { columns });
    let mut partitions = Vec::new();
    if partition_by.is_none() && !dto.partitions.is_empty() {
        errs.push(LoadError::semantic(
            src,
            to_span(&dto.table.defined),
            "`partitions:` is declared without `partition_by:`",
            "a partition needs its parent's key",
        ));
    }
    if let Some(parent) = &name {
        for (child, partition) in dto.partitions {
            let child_name = if child.contains('.') {
                match TableName::from_str(&child) {
                    Ok(n) => n,
                    Err(e) => {
                        errs.push(LoadError::semantic(
                            src,
                            to_span(&dto.table.defined),
                            format!("invalid partition name `{child}`: {e}"),
                            "not a partition name",
                        ));
                        continue;
                    }
                }
            } else {
                TableName::new(parent.schema.clone(), child.clone())
            };
            let datum = |v: crate::dto::BoundValueDto| match v {
                crate::dto::BoundValueDto::Text(t) => pbps_model::BoundDatum::declared(&t),
                crate::dto::BoundValueDto::Int(i) => pbps_model::BoundDatum::Value(i.to_string()),
            };
            let (bound, own, columns) = match partition {
                crate::dto::PartitionDto::Default(word) if word.eq_ignore_ascii_case("default") => {
                    (
                        pbps_model::PartitionBound::Default,
                        Table::default(),
                        Default::default(),
                    )
                }
                crate::dto::PartitionDto::Default(word) => {
                    errs.push(LoadError::semantic(
                        src,
                        to_span(&dto.table.defined),
                        format!(
                            "partition `{child}` is `{word}`: write `default`, or a mapping of \
                             `from:` and `to:`"
                        ),
                        "not a partition bound",
                    ));
                    continue;
                }
                crate::dto::PartitionDto::Entry(e) => {
                    let bound = match (e.default, e.from, e.to) {
                        (false, Some(from), Some(to)) => pbps_model::PartitionBound::Range {
                            from: from.into_iter().map(datum).collect(),
                            to: to.into_iter().map(datum).collect(),
                        },
                        (true, None, None) => pbps_model::PartitionBound::Default,
                        (default, ..) => {
                            errs.push(LoadError::semantic(
                                src,
                                to_span(&dto.table.defined),
                                if default {
                                    format!(
                                        "partition `{child}` is `default: true` and also has \
                                         `from:` or `to:`"
                                    )
                                } else {
                                    format!(
                                        "partition `{child}` needs both `from:` and `to:`, or \
                                         `default: true`"
                                    )
                                },
                                "not a partition bound",
                            ));
                            continue;
                        }
                    };
                    let own = Table {
                        checks: e
                            .checks
                            .into_iter()
                            .map(|(k, expression)| (k, CheckConstraint { expression }))
                            .collect(),
                        indexes: convert_indexes(src, e.indexes, &mut errs),
                        ..Default::default()
                    };
                    let mut columns = std::collections::BTreeMap::new();
                    for (column, c) in e.columns {
                        // The engine refuses a partition dropping a NOT NULL
                        // its parent's column has, and `nullable: true` is
                        // otherwise the parent's, which is not an override.
                        if c.nullable == Some(true) {
                            errs.push(LoadError::semantic(
                                src,
                                to_span(&dto.table.defined),
                                format!(
                                    "partition `{child}` column `{column}` is `nullable: true`: \
                                     a partition takes its parent's nullability or adds NOT \
                                     NULL, so write `nullable: false` or nothing"
                                ),
                                "not a partition column",
                            ));
                            continue;
                        }
                        let own = pbps_model::PartitionColumn {
                            default: c.default,
                            not_null: c.nullable == Some(false),
                        };
                        if own == pbps_model::PartitionColumn::default() {
                            errs.push(LoadError::semantic(
                                src,
                                to_span(&dto.table.defined),
                                format!(
                                    "partition `{child}` column `{column}` declares nothing of \
                                     its own: write its `default:` or `nullable: false`, or \
                                     leave it out"
                                ),
                                "not a partition column",
                            ));
                            continue;
                        }
                        columns.insert(column, own);
                    }
                    (bound, own, columns)
                }
            };
            partitions.push((
                child_name,
                Table {
                    partition_of: Some(pbps_model::PartitionOf {
                        parent: parent.clone(),
                        bound,
                        columns,
                    }),
                    ..own
                },
            ));
        }
    }

    match (name, errs.is_empty()) {
        (Some(name), true) => Ok(LoadedTable {
            name,
            table: Table {
                description: dto.description,
                columns,
                computed: dto
                    .computed
                    .into_iter()
                    .map(|(name, c)| {
                        (
                            name,
                            pbps_model::ComputedColumn {
                                expression: c.expression,
                                persisted: c.persisted,
                                not_null: c.not_null,
                            },
                        )
                    })
                    .collect(),
                primary_key,
                unique,
                foreign_keys,
                checks,
                indexes,
                data,
                clustered: dto.clustered.map(|c| match c {
                    ClusteredDto::Heap => Clustered::Heap,
                    ClusteredDto::Unique(name) => Clustered::Unique(name),
                    ClusteredDto::Index(name) => Clustered::Index(name),
                }),
                replica_identity: dto.replica_identity.map(|r| match r {
                    ReplicaIdentityDto::Full => ReplicaIdentity::Full,
                    ReplicaIdentityDto::Nothing => ReplicaIdentity::Nothing,
                    ReplicaIdentityDto::PrimaryKey => ReplicaIdentity::PrimaryKey,
                    ReplicaIdentityDto::Unique(name) => ReplicaIdentity::Unique(name),
                    ReplicaIdentityDto::Index(name) => ReplicaIdentity::Index(name),
                }),
                storage_parameters,
                unlogged: dto.unlogged,
                system_time,
                partition_by,
                partition_of: None,
            },
            intents,
            strategy: dto.strategy.map(|s| Strategy { online: s.online }),
            partitions,
        }),
        _ => Err(errs),
    }
}

/// The `data:` block (ADR-0004).
///
/// Every problem is collected, like everywhere else here: a lookup table's
/// block is the one place a user writes dozens of similar lines, and reporting
/// the first bad one at a time would be the worst possible place to do it.
fn convert_data(src: &SourceFile, dto: DataDto) -> Result<TableData, Vec<LoadError>> {
    let mut errs = Vec::new();

    // The `mode:` scalar is the only spanned thing in the block, so it is also
    // where the cell diagnostics below point. They name the row and column in
    // the message; the span gets the reader to the right block.
    let at = to_span(&dto.mode.defined);

    let mode = match dto.mode.value.as_str() {
        "exact" => Some(DataMode::Exact),
        "ensure" => Some(DataMode::Ensure),
        other => {
            errs.push(
                LoadError::semantic(
                    src,
                    at,
                    format!("unknown data mode `{other}`"),
                    "not a mode",
                )
                .with_help(
                    "`exact` means the declared rows are the whole table and an undeclared row is deleted; \
                     `ensure` means they must exist and anything else is left alone",
                ),
            );
            None
        }
    };

    let mut rows = std::collections::BTreeMap::new();
    for (key, cells) in dto.rows {
        let mut row = std::collections::BTreeMap::new();
        for (column, cell) in cells {
            match convert_value(cell) {
                Ok(v) => {
                    row.insert(column, v);
                }
                // The message deliberately does not echo the number back.
                // It has already been through `f64` by the time it gets here,
                // so echoing it would print `1.5` at a user who wrote `1.50` —
                // demonstrating the bug in the middle of explaining it.
                Err(FloatCell) => errs.push(
                    LoadError::semantic(
                        src,
                        at,
                        format!("row `{key}`, column `{column}`: an unquoted decimal"),
                        "quote it",
                    )
                    .with_help(
                        "the exact form written is the literal that reaches the column, and reading it \
                         as a floating-point number would not give it back — `'1.50'` stays 1.50",
                    ),
                ),
            }
        }
        rows.insert(RowKey::from(key), Row(row));
    }

    match (mode, errs.is_empty()) {
        (Some(mode), true) => Ok(TableData { mode, rows }),
        _ => Err(errs),
    }
}

/// A YAML float, which the loader refuses.
struct FloatCell;

fn convert_value(dto: ValueDto) -> Result<Value, FloatCell> {
    Ok(match dto {
        ValueDto::Null => Value::Null,
        ValueDto::Bool(b) => Value::Bool(b),
        ValueDto::Int(i) => Value::Int(i),
        ValueDto::Float(_) => return Err(FloatCell),
        ValueDto::Text(t) => Value::Text(t),
    })
}

/// `dbo.region(region_id)` / `dbo.region(a, b)` into a table name and a column
/// list.
fn parse_reference(
    src: &SourceFile,
    v: &Spanned<String>,
) -> Result<(TableName, Vec<String>), LoadError> {
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("invalid foreign key target: {msg}"),
            msg,
        )
        .with_help("the format is `schema.table(column)`; separate multiple columns with commas")
    };

    let s = v.value.trim();
    let open = s.find('(').ok_or_else(|| bad("missing `(`"))?;
    let close = s.rfind(')').ok_or_else(|| bad("missing `)`"))?;
    if close < open || !s[close + 1..].trim().is_empty() {
        return Err(bad("the parentheses are misplaced"));
    }

    let table = TableName::from_str(s[..open].trim()).map_err(|e| bad(&e.to_string()))?;

    let mut columns = Vec::new();
    for raw in s[open + 1..close].split(',') {
        let c = raw.trim();
        if c.is_empty() {
            return Err(bad("the column list has an empty entry"));
        }
        columns.push(c.to_owned());
    }
    Ok((table, columns))
}

/// A table's or a partition's `indexes:` (#1577), each error pushed to `errs`
/// and its index left out.
fn convert_indexes(
    src: &SourceFile,
    dtos: std::collections::BTreeMap<String, crate::dto::IndexDto>,
    errs: &mut Vec<LoadError>,
) -> std::collections::BTreeMap<String, Index> {
    let mut indexes = std::collections::BTreeMap::new();
    for (k, ix) in dtos {
        let mut cols = Vec::with_capacity(ix.columns.len() + ix.keys.len());
        let mut ok = true;
        // One list or the other (DEC-1169.2): two would leave the order of
        // their keys unsaid, and the order is the index.
        if !ix.columns.is_empty() && !ix.keys.is_empty() {
            errs.push(LoadError::semantic(
                src,
                to_span(&ix.keys[0].defined),
                format!("index `{k}` names its keys under both `columns:` and `keys:`"),
                "one list or the other",
            ));
            ok = false;
        }
        for c in &ix.columns {
            match parse_index_column(src, c) {
                Ok(v) => cols.push(v),
                Err(e) => {
                    errs.push(e);
                    ok = false;
                }
            }
        }
        for c in &ix.keys {
            match parse_index_key(src, c) {
                Ok(v) => cols.push(v),
                Err(e) => {
                    errs.push(e);
                    ok = false;
                }
            }
        }
        let method = ix.method;
        let storage_parameters = storage_parameters_of(
            src,
            &ix.storage_parameters,
            &|n, v| pbps_model::storage::canonical_index(method, n, v),
            "a B-tree index takes `fillfactor` and `deduplicate_items`, a GIN index \
             `fastupdate` and `gin_pending_list_limit`",
            errs,
        );
        if ok {
            indexes.insert(
                k,
                Index {
                    columns: cols,
                    include: ix.include,
                    unique: ix.unique,
                    filter: ix.filter,
                    method,
                    storage_parameters,
                },
            );
        }
    }
    indexes
}

/// `created_at`, `created_at desc`, `body jsonb_path_ops`, or all three
/// parts: PostgreSQL's own order, the operator class before the direction.
///
/// A second part that is a direction is the direction; anything else there is
/// an operator class, which has to be a plain identifier. Whether the dialect
/// accepts that class for that column is its validator's question (DEC-1169.1).
fn parse_index_column(src: &SourceFile, v: &Spanned<String>) -> Result<IndexColumn, LoadError> {
    let parts: Vec<&str> = v.value.split_whitespace().collect();
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("invalid index column: {msg}"),
            msg,
        )
        .with_help("the format is `column`, then an optional operator class, then an optional `asc` or `desc`")
    };
    let direction = |d: &str| {
        if d.eq_ignore_ascii_case("desc") {
            Some(true)
        } else if d.eq_ignore_ascii_case("asc") {
            Some(false)
        } else {
            None
        }
    };
    let opclass = |o: &str| {
        if is_opclass_name(o) {
            Ok(o.to_owned())
        } else {
            Err(bad(&format!("`{o}` is not an operator class name")))
        }
    };
    let column = |name: &str, opclass: Option<String>, descending: bool| IndexColumn {
        key: pbps_model::IndexKey::Column(name.to_owned()),
        descending,
        opclass,
    };

    match parts.as_slice() {
        [name] => Ok(column(name, None, false)),
        [name, second] => match direction(second) {
            Some(descending) => Ok(column(name, None, descending)),
            None => Ok(column(name, Some(opclass(second)?), false)),
        },
        [name, class, dir] => match direction(dir) {
            Some(descending) => Ok(column(name, Some(opclass(class)?), descending)),
            None => Err(bad(&format!("`{dir}` is not asc or desc"))),
        },
        _ => Err(bad("wrong number of parts")),
    }
}

/// A plain identifier, which is every operator class name this tool accepts.
fn is_opclass_name(o: &str) -> bool {
    let mut chars = o.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `{column: id}` or `{expression: "lower(email)"}`, each with optional
/// `opclass:` and `order:`. An expression is kept verbatim, as a filter is:
/// whether it is one the engine accepts is the dialect's and then the
/// engine's question (DEC-1169.2); here it is only never the empty string.
/// Not "blank" by Rust's whitespace class: PostgreSQL reads a non-breaking
/// space as part of an identifier, so which texts hold no expression is the
/// dialect's lexis to say (DECISIONS 504), and its validator asks it.
/// Storage parameters as written, each in its canonical spelling by
/// `canonical`, and an error for each it refuses (#1441, #1442).
fn storage_parameters_of(
    src: &SourceFile,
    written: &std::collections::BTreeMap<String, Spanned<StorageValueDto>>,
    canonical: &dyn Fn(&str, &str) -> Result<String, String>,
    help: &str,
    errs: &mut Vec<LoadError>,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (parameter, value) in written {
        // A number as it is written in the file, not as YAML read it: the
        // engine reads the spelling, and an `f64` can have lost it (a bare
        // `1e-400` is already 0, which the engine refuses, #1477 review).
        let spelled = || {
            let span = to_span(&value.defined);
            src.text
                .get(span.offset()..span.offset() + span.len())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        let text = match &value.value {
            StorageValueDto::Bool(b) => b.to_string(),
            StorageValueDto::Int(i) => spelled().unwrap_or_else(|| i.to_string()),
            StorageValueDto::Real(r) => spelled().unwrap_or_else(|| r.to_string()),
            StorageValueDto::Text(t) => t.clone(),
        };
        match canonical(parameter, &text) {
            Ok(spelled) => {
                out.insert(parameter.clone(), spelled);
            }
            Err(why) => errs.push(
                LoadError::semantic(
                    src,
                    to_span(&value.defined),
                    format!("invalid storage parameter: {why}"),
                    "here",
                )
                .with_help(help.to_owned()),
            ),
        }
    }
    out
}

fn parse_index_key(
    src: &SourceFile,
    v: &Spanned<crate::dto::IndexKeyDto>,
) -> Result<IndexColumn, LoadError> {
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("invalid index key: {msg}"),
            msg,
        )
        .with_help(
            "the format is `{column: <name>}` or `{expression: \"...\"}`, with optional \
             `opclass:` and `order: asc|desc`",
        )
    };
    let e = &v.value;
    let key = match (&e.column, &e.expression) {
        (Some(column), None) => pbps_model::IndexKey::Column(column.clone()),
        (None, Some(expression)) if expression.is_empty() => {
            return Err(bad("the expression is empty"));
        }
        (None, Some(expression)) => pbps_model::IndexKey::Expression(expression.clone()),
        (Some(_), Some(_)) => return Err(bad("a key names a column or an expression, not both")),
        (None, None) => return Err(bad("a key names a column or an expression")),
    };
    if let Some(class) = &e.opclass
        && !is_opclass_name(class)
    {
        return Err(bad(&format!("`{class}` is not an operator class name")));
    }
    Ok(IndexColumn {
        key,
        descending: e.order == Some(crate::dto::IndexOrder::Desc),
        opclass: e.opclass.clone(),
    })
}

/// DTO to domain model for one module.
pub fn convert_module(src: &SourceFile, dto: ModuleDto) -> Result<LoadedModule, Vec<LoadError>> {
    let mut errs = Vec::new();

    // Exactly one leading key names the object, and it is also the kind. Two of
    // them is not a file the tool can guess its way through: which one is the
    // object and which one a stray line is precisely what it cannot know.
    let named: Vec<(ModuleKind, &Spanned<String>)> = [
        (ModuleKind::View, &dto.view),
        (ModuleKind::Procedure, &dto.procedure),
        (ModuleKind::Function, &dto.function),
        (ModuleKind::Trigger, &dto.trigger),
    ]
    .into_iter()
    .filter_map(|(kind, v)| v.as_ref().map(|v| (kind, v)))
    .collect();

    let (kind, name_value) = match named.as_slice() {
        [one] => *one,
        [] => {
            return Err(vec![LoadError::Yaml {
                path: std::path::PathBuf::from(&src.name),
                message: "a declaration file starts with `table:`, `view:`, `procedure:`, \
                          `function:` or `trigger:`"
                    .to_owned(),
            }]);
        }
        many => {
            let kinds: Vec<&str> = many.iter().map(|(k, _)| k.as_str()).collect();
            return Err(vec![LoadError::semantic(
                src,
                to_span(&many[0].1.defined),
                format!(
                    "this file declares {} objects at once: {}",
                    many.len(),
                    kinds.join(", ")
                ),
                "one object per file",
            )]);
        }
    };

    // The declared name, by shape alone: `app.v`, `app.f(int, text)` or, for a
    // trigger, the two-part name that goes with an `on:`. Which shapes this
    // engine allows for this kind is the dialect's answer, checked once the
    // whole schema is loaded (`pbps_dialect::check_module_names`).
    let declared: Option<ModuleId> = match parse_at(src, name_value, "invalid object name") {
        Ok(n) => Some(n),
        Err(e) => {
            errs.push(e.with_help(
                "an object name has the two parts `schema.object`, e.g. `dbo.active_customer`; a \
                 function or procedure may carry its argument types, e.g. `app.f(int, text)`",
            ));
            None
        }
    };

    let mut on = None;
    if let Some(v) = &dto.on {
        match parse_at::<TableName>(src, v, "invalid table name in `on`") {
            Ok(t) => on = Some(t),
            Err(e) => errs.push(e),
        }
    }
    if on.is_some() && kind != ModuleKind::Trigger {
        errs.push(LoadError::semantic(
            src,
            to_span(&name_value.defined),
            format!("a {kind} cannot be `on:` a table; only a trigger names one"),
            "remove the `on:` line",
        ));
    }

    // A trigger's identity is its table plus its own name (ADR-0009 §1,
    // measured: `DROP TRIGGER audit` is a syntax error on PostgreSQL). The
    // file keeps its two lines — `trigger: app.audit` and `on: app.orders` —
    // and they fold into one id here.
    let id: Option<ModuleId> = match (kind, declared, &on) {
        (ModuleKind::Trigger, Some(ModuleId::Named(name)), Some(table)) => {
            // The trigger's schema is not its own: SQL Server puts it in its
            // table's, and PostgreSQL gives it none. Two spellings that
            // disagree are a declaration whose halves mean different things,
            // and dropping one silently is how a trigger ends up managed on a
            // table nobody named.
            if name.schema != table.schema {
                errs.push(LoadError::semantic(
                    src,
                    to_span(&name_value.defined),
                    format!(
                        "trigger `{name}` is on `{table}`, which is in schema `{}`; a trigger \
                         lives in the schema of the table it is on",
                        table.schema
                    ),
                    format!("name it `{}.{}`", table.schema, name.name),
                ));
                None
            } else {
                Some(ModuleId::Trigger {
                    on: table.clone(),
                    name: name.name,
                })
            }
        }
        (ModuleKind::Trigger, Some(_), None) => {
            errs.push(LoadError::semantic(
                src,
                to_span(&name_value.defined),
                format!(
                    "trigger `{}` does not say which table it is on",
                    name_value.value
                ),
                "add `on: schema.table`",
            ));
            None
        }
        (ModuleKind::Trigger, Some(other), Some(_)) => {
            errs.push(LoadError::semantic(
                src,
                to_span(&name_value.defined),
                format!("`{other}` is not a trigger name"),
                "a trigger is named `schema.trigger`, with its table in `on:`",
            ));
            None
        }
        (_, Some(ModuleId::Trigger { .. }), _) => {
            errs.push(LoadError::semantic(
                src,
                to_span(&name_value.defined),
                format!("a {kind} name has the two parts `schema.object`"),
                "only a trigger is named for the table it is on",
            ));
            None
        }
        (_, declared, _) => declared,
    };

    let mut depends_on = std::collections::BTreeSet::new();
    for v in &dto.depends_on {
        match parse_at::<ModuleId>(src, v, "invalid object name in `depends_on`") {
            Ok(n) => {
                depends_on.insert(n);
            }
            Err(e) => errs.push(e),
        }
    }

    // Only a routine has an `EXECUTE` privilege to leave standing: a view is
    // selected from and a trigger is not invoked by anyone at all, so the key
    // on either is a decision that would never be acted on. Refused rather
    // than ignored — a line a reader takes for a security control and the
    // tool takes for nothing is the worst of the three readings.
    let public_execute = match &dto.public_execute {
        Some(v) if !matches!(kind, ModuleKind::Procedure | ModuleKind::Function) => {
            errs.push(LoadError::semantic(
                src,
                to_span(&v.defined),
                format!("`public_execute:` is meaningless on a {kind}"),
                "only a procedure or a function has an `EXECUTE` privilege; remove the key",
            ));
            false
        }
        Some(v) => v.value,
        None => false,
    };

    match (id, errs.is_empty()) {
        (Some(id), true) => Ok(LoadedModule {
            id,
            module: Module {
                kind,
                description: dto.description,
                definition: dto.definition,
            },
            depends_on,
            public_execute,
        }),
        _ => Err(errs),
    }
}

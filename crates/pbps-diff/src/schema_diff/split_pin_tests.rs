use super::*;
use crate::identity::Context;
use pbps_dialect::MinimalDialect;
use pbps_model::{
    Column, Conversion, ExclusionConstraint, ExclusionKey, ExclusionTiming, ForeignKey, Intent,
    ManagedType, OwnedSequence, PrimaryKey, Row, RowKey, TableData, Value,
};

fn ctx() -> Context {
    Context {
        operator: "leon".into(),
        today: "2026-11-21".into(),
    }
}

fn ty(text: &str) -> ColumnType {
    text.parse().unwrap()
}

fn name(text: &str) -> TableName {
    text.parse().unwrap()
}

fn module_id(text: &str) -> ModuleId {
    text.parse().unwrap()
}

fn column(ty_text: &str) -> Column {
    Column::new(ty(ty_text))
}

fn table_of(columns: &[(&str, Column)]) -> Table {
    Table {
        columns: columns
            .iter()
            .map(|(n, c)| ((*n).into(), c.clone()))
            .collect(),
        ..Table::default()
    }
}

/// A dialect that is `MinimalDialect` in everything but the capabilities a pin turns
/// on. A refusal can only be pinned in a call that reaches it, and some of the phases'
/// refusals sit behind an engine capability the minimal dialect lacks.
#[derive(Default, Clone)]
struct Engine {
    /// An owned sequence is renamed in place, so the sequence-rename cycle is reachable.
    numbering: bool,
    /// A table's last column cannot be dropped, so replacing it can be unorderable.
    last_column: bool,
    /// An `AlterModule` is a drop and a create, as on PostgreSQL.
    rebuilds_modules: bool,
    /// What `rebound_modules` answers for an unchanged module whenever anything arrives.
    rebinds: BTreeSet<ModuleId>,
    /// Two columns join only if they have one base type, so a retype of one end of a key is
    /// a key between types the engine cannot join.
    strict_keys: bool,
    /// A module the engine binds to a column type, torn down around a retype in the plan.
    bound_to_retypes: BTreeSet<ModuleId>,
}

impl Dialect for Engine {
    fn name(&self) -> &'static str {
        "minimal"
    }
    fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
        MinimalDialect.quote_ident(ident)
    }
    fn intermediate_tables(
        &self,
        change: &Change,
        strategy: pbps_model::Strategy,
    ) -> Result<Vec<TableName>, pbps_dialect::DialectError> {
        MinimalDialect.intermediate_tables(change, strategy)
    }
    fn normalize_type(&self, t: &ColumnType) -> Result<ColumnType, pbps_dialect::DialectError> {
        MinimalDialect.normalize_type(t)
    }
    fn type_change_risk(&self, a: &ColumnType, b: &ColumnType) -> pbps_dialect::TypeChangeRisk {
        MinimalDialect.type_change_risk(a, b)
    }
    fn fold_ident<'a>(&self, i: &'a str) -> std::borrow::Cow<'a, str> {
        MinimalDialect.fold_ident(i)
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        MinimalDialect.lexicon()
    }
    fn validate_table(&self, n: &TableName, t: &Table) -> Vec<pbps_dialect::DialectError> {
        MinimalDialect.validate_table(n, t)
    }
    fn transaction_framing(&self, z: &pbps_model::SessionZone) -> pbps_dialect::TransactionFraming {
        MinimalDialect.transaction_framing(z)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        MinimalDialect.probe_framing()
    }
    fn foreign_key_types_compatible(
        &self,
        referencing: &ColumnType,
        referenced: &ColumnType,
    ) -> Option<bool> {
        self.strict_keys
            .then(|| referencing.base == referenced.base)
    }
    fn changes_numbering(&self) -> bool {
        self.numbering
    }
    fn refuses_dropping_last_column(&self) -> bool {
        self.last_column
    }
    fn rebuilds_modules(&self) -> bool {
        self.rebuilds_modules
    }
    fn rebound_modules(
        &self,
        _declared: &Schema,
        arriving: &[ModuleId],
        already_changed: &BTreeSet<ModuleId>,
    ) -> BTreeSet<ModuleId> {
        if arriving.is_empty() {
            return BTreeSet::new();
        }
        self.rebinds
            .iter()
            .filter(|id| !already_changed.contains(*id))
            .cloned()
            .collect()
    }
    fn schema_bound_teardown(
        &self,
        _base: &Schema,
        _declared: &Schema,
        changes: &[Change],
    ) -> BTreeSet<ModuleId> {
        // Reads the plan the earlier passes built: the question SQL Server's binding rule
        // asks, in its simplest form.
        if changes.iter().any(|c| {
            matches!(
                c,
                Change::Column(ColumnChange::AlterColumnType { .. })
                    | Change::TypeValue(TypeValueChange::EnumRebuild {
                        step: pbps_model::EnumRebuildStep::Retype { .. },
                        ..
                    })
            )
        }) {
            self.bound_to_retypes.clone()
        } else {
            BTreeSet::new()
        }
    }
}

/// The variant a refusal is, with a short tag for the one variant that several phases use.
fn label(error: &DiffError) -> String {
    let debug = format!("{error:?}");
    let variant = debug
        .split([' ', '{', '('])
        .next()
        .unwrap_or_default()
        .to_owned();
    if let DiffError::TypeLifecycleUnsupported { name, reason } = error {
        let tag = if reason.contains("rename cycle") {
            "rename cycle"
        } else if reason.contains("domain base dependency cycle") {
            "domain cycle"
        } else {
            "labels"
        };
        return format!("{variant}({}, {tag})", name.shown());
    }
    variant
}

fn labels(errors: &[DiffError]) -> Vec<String> {
    errors.iter().map(label).collect()
}

/// The base and each revision resolved in turn, as the project's files accumulate them,
/// then diffed once from the base to the last revision.
fn through_revisions(
    dialect: &Engine,
    hints: &Hints,
    base: &Schema,
    revisions: &[(Schema, Vec<Intent>)],
    edit_ids: impl FnOnce(&mut IdsFile, &mut IdsFile),
) -> Diffed {
    let mut base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let mut ids = base_ids.clone();
    for (schema, intents) in revisions {
        ids = crate::resolve(schema, &ids, intents, &ctx()).unwrap().ids;
    }
    edit_ids(&mut base_ids, &mut ids);
    diff_partial(
        Side {
            schema: base,
            ids: &base_ids,
        },
        Side {
            schema: &revisions.last().unwrap().0,
            ids: &ids,
        },
        dialect,
        hints,
    )
}

fn rename_column(table: &str, from: &str, to: &str) -> Intent {
    Intent::RenameColumn {
        table: name(table),
        from: from.into(),
        to: to.into(),
    }
}

fn rename_table(from: &str, to: &str) -> Intent {
    Intent::RenameTable {
        from: name(from),
        to: name(to),
    }
}

fn rename_type(from: &str, to: &str) -> Intent {
    Intent::RenameType {
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
    }
}

fn drop_column(column: &str) -> Intent {
    Intent::DropColumn {
        column: column.parse().unwrap(),
        reason: "not needed".parse().unwrap(),
    }
}

fn enum_of(labels: &[&str]) -> ManagedType {
    ManagedType::Enum {
        values: labels.iter().map(|l| (*l).to_owned()).collect(),
    }
}

fn domain_over(base: &str) -> ManagedType {
    ManagedType::Domain {
        base: ty(base),
        not_null: false,
        default: None,
        checks: Default::default(),
        collation: None,
    }
}

fn serial_named(sequence: &str) -> Column {
    let mut c = column("int");
    c.value_source = ValueSource::Sequence(OwnedSequence {
        name: Some(sequence.to_owned()),
        ..OwnedSequence::serial()
    });
    c
}

fn exclusion_over(column: &str) -> ExclusionConstraint {
    ExclusionConstraint {
        method: IndexMethod::Gist,
        keys: vec![ExclusionKey {
            key: pbps_model::IndexColumn::column(column),
            operator: pbps_model::OperatorRef::symbol("=").unwrap(),
        }],
        include: Vec::new(),
        filter: None,
        storage_parameters: Default::default(),
        timing: ExclusionTiming::Immediate,
    }
}

/// One project that every phase of the differ has something to refuse in, as a base (step
/// 0), two intermediate revisions and the declarations (step 3). The cycles need the
/// revisions: `resolve` accepts each step, and only the net of the three is a cycle.
fn every_phase_refuses(step: usize) -> Schema {
    let mut schema = Schema::default();
    let mut put = |n: &str, t: Table| {
        schema.tables.insert(name(n), t);
    };
    // Sort facts: two columns trading names through a third.
    let [first, second] = [["a", "b"], ["c", "b"], ["c", "a"], ["b", "a"]][step];
    put(
        "app.cols",
        table_of(&[(first, column("int")), (second, column("bigint"))]),
    );
    // Finish of the sort: two tables trading names through a third.
    let [left, right] = [
        ["app.ta", "app.tb"],
        ["app.tc", "app.tb"],
        ["app.tc", "app.ta"],
        ["app.tb", "app.ta"],
    ][step];
    put(left, table_of(&[("x", column("int"))]));
    put(right, table_of(&[("y", column("int"))]));
    // Sort facts: two owned sequences trading names, in one step.
    let (p, q) = if step == 3 {
        ("s_b", "s_a")
    } else {
        ("s_a", "s_b")
    };
    put(
        "app.ser",
        table_of(&[("p", serial_named(p)), ("q", serial_named(q))]),
    );
    // Tables (survivor): a table whose filegroup changes.
    let mut fg = table_of(&[("id", column("int"))]);
    if step > 0 {
        fg.filegroup = Some(pbps_model::Filegroup::new("fast"));
    }
    put("app.fg", fg);
    // Finish of the sort: a table whose only column is replaced by a new one of the same name.
    put("app.last", table_of(&[("a", column("int"))]));
    // Passes: a column dropped under an exclusion constraint that holds it.
    let mut booking = table_of(&[("a", column("int")), ("b", column("int"))]);
    booking
        .exclude
        .insert("no_clash".into(), exclusion_over("a"));
    if step > 0 {
        booking.columns.shift_remove("a");
    }
    put("app.booking", booking);
    // Passes: a foreign key between two types the engine cannot join, once one end is retyped.
    put(
        "app.parent",
        table_of(&[(
            "id",
            column(if step == 0 { "int" } else { "uuid" }).not_null(),
        )]),
    );
    let mut child = table_of(&[("pid", column("int"))]);
    child.foreign_keys.insert(
        "fk_parent".into(),
        ForeignKey {
            columns: vec!["pid".into()],
            references_table: name("app.parent"),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    put("app.child", child);
    // Finish of the sort: a label added to an enum in this plan, and a default that names it.
    schema.types.insert(
        "app.mood".parse().unwrap(),
        enum_of(if step == 0 { &["ok"] } else { &["ok", "new"] }),
    );
    if step > 0 {
        let mut default = column("app.mood");
        default.value_source = ValueSource::Default("'new'".into());
        put("app.moody", table_of(&[("m", default)]));
        // Tables (created): rows, and no key to hold them by.
        let mut keyless = table_of(&[("v", column("int"))]);
        keyless.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(RowKey::from("1"), Row::default())].into(),
        });
        put("app.keyless", keyless);
        // Tables (unlogged): a permanent table with a foreign key to an unlogged one.
        let mut volatile = table_of(&[("id", column("int").not_null())]);
        volatile.unlogged = true;
        put("app.volatile", volatile);
        let mut durable = table_of(&[("id", column("int")), ("v", column("int"))]);
        durable.foreign_keys.insert(
            "fk_v".into(),
            ForeignKey {
                columns: vec!["v".into()],
                references_table: name("app.volatile"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        put("app.durable", durable);
    }
    // Types: two types trading names through a third.
    let [one, two] = [
        ["app.e1", "app.e2"],
        ["app.e3", "app.e2"],
        ["app.e3", "app.e1"],
        ["app.e2", "app.e1"],
    ][step];
    schema.types.insert(one.parse().unwrap(), enum_of(&["x"]));
    schema.types.insert(two.parse().unwrap(), enum_of(&["y"]));
    // Types: a type no identity names, whose labels change.
    schema.types.insert(
        "app.loose".parse().unwrap(),
        enum_of(if step == 0 { &["x"] } else { &["x", "y"] }),
    );
    if step > 0 {
        // Sort facts: domains over each other.
        schema
            .types
            .insert("app.d1".parse().unwrap(), domain_over("app.d2"));
        schema
            .types
            .insert("app.d2".parse().unwrap(), domain_over("app.d1"));
        // Prelude: a package on an engine that is not PostgreSQL, a standalone sequence the
        // target lacks, and two modules that wait on each other.
        schema.extensions.insert(
            "citext".into(),
            pbps_model::Extension {
                schema: "public".into(),
                version: "1.6".into(),
            },
        );
        schema.sequences.insert(
            "app.numbers".parse().unwrap(),
            pbps_model::Sequence {
                ty: ty("bigint"),
                start: 1,
                increment: 1,
                options: Default::default(),
            },
        );
        // Prelude: a configuration mapping a dictionary nothing declares.
        schema.search.insert(
            "config app.docs".parse().unwrap(),
            pbps_model::SearchObject::Config {
                parser: "pg_catalog.\"default\"".parse().unwrap(),
                mappings: [("asciiword".to_owned(), vec!["app.words".parse().unwrap()])].into(),
            },
        );
        for id in ["app.va", "app.vb"] {
            schema.modules.insert(
                module_id(id),
                Module {
                    kind: pbps_model::ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".into(),
                    options: Default::default(),
                },
            );
        }
    }
    schema
}

fn every_phase_refuses_intents(step: usize) -> Vec<Intent> {
    match step {
        1 => vec![
            rename_column("app.cols", "a", "c"),
            rename_table("app.ta", "app.tc"),
            rename_type("app.e1", "app.e3"),
            drop_column("app.booking.a"),
        ],
        2 => vec![
            rename_column("app.cols", "b", "a"),
            rename_table("app.tb", "app.ta"),
            rename_type("app.e2", "app.e1"),
        ],
        3 => vec![
            rename_column("app.cols", "c", "b"),
            rename_table("app.tc", "app.tb"),
            rename_type("app.e3", "app.e2"),
        ],
        _ => Vec::new(),
    }
}

/// What a change is, for reading a plan's order: its kind and the object it names, and
/// where the name alone does not say which of two changes of one kind it is, the column or
/// the direction.
fn kind(change: &Change) -> String {
    let debug = format!("{change:?}");
    let variant = debug.split(['(', ' ', '{']).next().unwrap_or_default();
    let more = if let Change::Column(
        ColumnChange::DropColumn { column, .. }
        | ColumnChange::AlterColumnType { column, .. }
        | ColumnChange::AlterColumnDefault { column, .. }
        | ColumnChange::AlterColumnNullability { column, .. }
        | ColumnChange::AlterColumnNumbering { column, .. },
    ) = change
    {
        format!(".{}", column.name.shown())
    } else if let Change::Column(ColumnChange::AddColumn { name, .. }) = change {
        format!(".{}", name.shown())
    } else if let Change::Column(ColumnChange::RenameColumn { from, to, .. }) = change {
        format!(".{}->{}", from.shown(), to.shown())
    } else if let Change::Constraint(ConstraintChange::SetPrimaryKey { to, .. }) = change {
        if to.is_some() { " add" } else { " drop" }.to_owned()
    } else if let Change::TypeValue(TypeValueChange::AlterDomain { half, .. }) = change {
        format!(" {half:?}")
    } else if let Change::TypeValue(TypeValueChange::EnumRebuild { step, .. }) = change {
        let step = format!("{step:?}");
        format!(" {}", step.split([' ', '{']).next().unwrap_or_default())
    } else if let Change::Index(
        IndexChange::AddIndex { name, .. } | IndexChange::DropIndex { name, .. },
    ) = change
    {
        format!(" {}", name.shown())
    } else {
        String::new()
    };
    format!("{variant} {}{more}", change.subject())
}

fn kinds(diffed: &Diffed) -> Vec<String> {
    diffed
        .changes
        .changes
        .iter()
        .map(|p| kind(&p.change))
        .collect()
}

fn keyed(columns: &[(&str, Column)], key: &str) -> Table {
    let mut t = table_of(columns);
    t.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec![key.into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    t
}

fn with_rows(mut t: Table, mode: DataMode, rows: &[(&str, &[(&str, &str)])]) -> Table {
    t.data = Some(TableData {
        mode,
        rows: rows
            .iter()
            .map(|(key, cells)| {
                (
                    RowKey::from(*key),
                    Row(cells
                        .iter()
                        .map(|(c, v)| ((*c).into(), Value::Text((*v).into())))
                        .collect()),
                )
            })
            .collect(),
    });
    t
}

fn view(text: &str) -> Module {
    Module {
        kind: pbps_model::ModuleKind::View,
        description: None,
        definition: text.into(),
        options: Default::default(),
    }
}

fn role_with(grants: &[(&str, &[pbps_model::Permission])]) -> pbps_model::Role {
    let mut r = pbps_model::Role::default();
    for (target, permissions) in grants {
        r.grants.insert(
            target.parse::<pbps_model::GrantTarget>().unwrap(),
            permissions.iter().copied().collect(),
        );
    }
    r
}

fn foreign_key(column: &str, to: &str) -> ForeignKey {
    ForeignKey {
        columns: vec![column.into()],
        references_table: name(to),
        references_columns: vec!["id".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    }
}

fn plain_index(column: &str) -> Index {
    Index {
        columns: vec![pbps_model::IndexColumn::column(column)],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

/// A project in two states whose plan holds a change of every class of the sort, built
/// from schemas only: the intents are the ones a human would write.
fn every_class(declared: bool) -> Schema {
    use pbps_model::Permission::{Insert, Select};
    let mut schema = Schema::default();
    let mut put = |n: &str, t: Table| {
        schema.tables.insert(name(n), t);
    };
    // A renamed table with a renamed column, and drops that therefore wait for the rename.
    if declared {
        put(
            "app.new_name",
            table_of(&[("id", column("int")), ("r2", column("int"))]),
        );
    } else {
        let mut generated = column("int");
        generated.value_source = ValueSource::Generated(pbps_model::Generated {
            expression: "id + 1".into(),
            stored: true,
        });
        let mut t = table_of(&[
            ("id", column("int")),
            ("r1", column("int")),
            ("d1", column("int")),
            ("g1", generated),
        ]);
        t.computed.insert(
            "cc".into(),
            pbps_model::ComputedColumn {
                expression: "id * 2".into(),
                persisted: false,
                not_null: false,
            },
        );
        put("app.old_name", t);
    }
    // The survivor: every alteration of a column, constraint and row.
    let mut alter = keyed(
        &[
            ("id", column("int").not_null()),
            ("n", column("int")),
            ("d", column("text")),
            ("gone", column("int")),
            ("dep", column("int")),
            ("req", column("int")),
            ("s", serial_named("s_x")),
            ("m2", column("app.mood2")),
        ],
        "id",
    );
    if declared {
        alter.columns.get_mut("s").unwrap().value_source = ValueSource::None;
        let mut generated = column("int");
        generated.value_source = ValueSource::Generated(pbps_model::Generated {
            expression: "n + 1".into(),
            stored: true,
        });
        alter.columns.insert("ga".into(), generated);
        alter.computed.insert(
            "cc2".into(),
            pbps_model::ComputedColumn {
                expression: "n * 2".into(),
                persisted: false,
                not_null: false,
            },
        );
        alter.columns.get_mut("n").unwrap().ty = ty("bigint");
        alter.columns.get_mut("d").unwrap().value_source = ValueSource::Default("'y'".into());
        alter.columns.shift_remove("gone");
        alter.columns.insert("fresh".into(), column("int"));
        alter.columns.get_mut("dep").unwrap().deprecated = Some("old".into());
        alter.columns.get_mut("req").unwrap().nullable = false;
        alter
            .storage_parameters
            .insert("fillfactor".into(), "70".into());
        alter.indexes.insert("ix_new".into(), plain_index("n"));
        alter
            .foreign_keys
            .insert("fk_new".into(), foreign_key("n", "app.modal"));
        alter.exclude.insert("x_new".into(), exclusion_over("n"));
        alter.unique.insert(
            "uq_new".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["d".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        alter.checks.insert(
            "ck_new".into(),
            pbps_model::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        alter.row_policies.insert(
            "p_new".into(),
            pbps_model::RowPolicy {
                using: Some("true".into()),
                ..Default::default()
            },
        );
        alter.row_security.enabled = true;
        alter = with_rows(
            alter,
            DataMode::Exact,
            &[
                ("1", &[("d", "changed")]),
                ("3", &[("d", "kept")]),
                ("4", &[("d", "new")]),
            ],
        );
    } else {
        alter.indexes.insert("ix_old".into(), plain_index("n"));
        alter
            .foreign_keys
            .insert("fk_old".into(), foreign_key("n", "app.modal"));
        alter.unique.insert(
            "uq_old".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["gone".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        alter.checks.insert(
            "ck_old".into(),
            pbps_model::CheckConstraint {
                expression: "n < 9".into(),
            },
        );
        alter.row_policies.insert(
            "p_old".into(),
            pbps_model::RowPolicy {
                using: Some("true".into()),
                ..Default::default()
            },
        );
        alter = with_rows(
            alter,
            DataMode::Exact,
            &[
                ("1", &[("d", "was")]),
                ("2", &[("d", "doomed")]),
                ("3", &[("d", "kept")]),
            ],
        );
    }
    put("app.alter", alter);
    // A key replaced by another: its drop and its add.
    let mut keyed_table = keyed(
        &[
            ("id", column("int").not_null()),
            ("code", column("int").not_null()),
        ],
        if declared { "code" } else { "id" },
    );
    keyed_table.unlogged = declared;
    if declared {
        keyed_table.replica_identity = Some(pbps_model::ReplicaIdentity::Full);
    }
    put("app.keyed", keyed_table);
    // A table whose rows change mode.
    put(
        "app.modal",
        with_rows(
            keyed(&[("id", column("int").not_null())], "id"),
            if declared {
                DataMode::Ensure
            } else {
                DataMode::Exact
            },
            &[("1", &[])],
        ),
    );
    if declared {
        // Created, with a policy and row security of its own.
        let mut created = table_of(&[("id", column("int"))]);
        created.row_policies.insert(
            "p_created".into(),
            pbps_model::RowPolicy {
                using: Some("true".into()),
                ..Default::default()
            },
        );
        created.row_security.enabled = true;
        put("app.created", created);
    } else {
        put("app.dropped", table_of(&[("id", column("int"))]));
    }
    // Types and a domain: created, renamed, extended, dropped, loosened and tightened.
    if declared {
        schema
            .types
            .insert("app.new_t".parse().unwrap(), enum_of(&["x"]));
        schema
            .types
            .insert("app.renamed_t".parse().unwrap(), enum_of(&["x"]));
        schema
            .types
            .insert("app.mood".parse().unwrap(), enum_of(&["a", "b"]));
        schema
            .types
            .insert("app.mood2".parse().unwrap(), enum_of(&["b"]));
    } else {
        schema
            .types
            .insert("app.mood2".parse().unwrap(), enum_of(&["a", "b"]));
        schema
            .types
            .insert("app.old_t".parse().unwrap(), enum_of(&["x"]));
        schema
            .types
            .insert("app.ren_t".parse().unwrap(), enum_of(&["x"]));
        schema
            .types
            .insert("app.mood".parse().unwrap(), enum_of(&["a"]));
    }
    let checks = |names: &[&str]| {
        names
            .iter()
            .map(|n| {
                (
                    pbps_model::ObjectLocalName::from(*n),
                    format!("VALUE <> '{n}'"),
                )
            })
            .collect()
    };
    schema.types.insert(
        "app.dom".parse().unwrap(),
        ManagedType::Domain {
            base: ty("text"),
            not_null: false,
            default: None,
            checks: checks(if declared {
                &["c2", "c3"]
            } else {
                &["c1", "c2"]
            }),
            collation: None,
        },
    );
    // Modules: one dropped, one edited, one created.
    if declared {
        schema
            .modules
            .insert(module_id("app.v_alter"), view("SELECT 2"));
        schema
            .modules
            .insert(module_id("app.v_new"), view("SELECT 3"));
    } else {
        schema
            .modules
            .insert(module_id("app.v_alter"), view("SELECT 1"));
        schema
            .modules
            .insert(module_id("app.v_drop"), view("SELECT 0"));
    }
    // Roles: one dropped, one renamed, one created, and a grant given up and one made.
    let roles = &mut schema.roles;
    if declared {
        roles.insert("ren_to".into(), role_with(&[]));
        roles.insert("new_role".into(), role_with(&[("app.alter", &[Select])]));
        roles.insert("writer".into(), role_with(&[("app.alter", &[Select])]));
    } else {
        roles.insert("old_role".into(), role_with(&[]));
        roles.insert("ren_from".into(), role_with(&[]));
        roles.insert(
            "writer".into(),
            role_with(&[("app.alter", &[Select, Insert])]),
        );
    }
    schema
}

fn every_class_intents() -> Vec<Intent> {
    vec![
        rename_table("app.old_name", "app.new_name"),
        rename_column("app.new_name", "r1", "r2"),
        drop_column("app.new_name.d1"),
        drop_column("app.new_name.g1"),
        drop_column("app.alter.gone"),
        Intent::DropTable {
            table: name("app.dropped"),
            reason: "gone".parse().unwrap(),
        },
        rename_type("app.ren_t", "app.renamed_t"),
        Intent::DropType {
            name: "app.old_t".parse().unwrap(),
            reason: "gone".parse().unwrap(),
        },
        Intent::RenameRole {
            from: "ren_from".into(),
            to: "ren_to".into(),
        },
        Intent::DropRole {
            role: "old_role".into(),
            reason: "gone".parse().unwrap(),
        },
    ]
}

/// A parent with a default partition beside ordinary tables, for the ranks inside the
/// classes that only partitions reach. `partitioned` says whether the table declared beside
/// the parent is one of its partitions: the same names, one with a parent.
fn partition_tree(declared: bool, partitioned: bool) -> Schema {
    use pbps_model::{BoundDatum, PartitionBound, PartitionBy, PartitionOf};
    let mut schema = Schema::default();
    let mut parent = table_of(&[
        ("id", column("int").not_null()),
        ("ts", column("date").not_null()),
        ("n", column("int")),
    ]);
    if partitioned {
        parent.partition_by = Some(PartitionBy {
            external: false,
            columns: vec!["ts".into()],
            strategy: Default::default(),
        });
    }
    if declared {
        parent.indexes.insert("ix_p".into(), plain_index("n"));
    }
    schema.tables.insert(name("app.ev"), parent);
    let mut ordinary = table_of(&[("id", column("int"))]);
    if declared {
        ordinary.indexes.insert("ix_a".into(), plain_index("id"));
        // Named to sort before the ordinary table created beside it, and declared as a
        // partition of the parent or not.
        let mut leaf = table_of(&[("id", column("int"))]);
        if partitioned {
            leaf.partition_of = Some(PartitionOf {
                parent: name("app.ev"),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::Value("2030-01-01".into())],
                    to: vec![BoundDatum::Value("2031-01-01".into())],
                },
                columns: Default::default(),
            });
        }
        schema.tables.insert(name("app.ev_new"), leaf);
        schema
            .tables
            .insert(name("app.mid"), table_of(&[("id", column("int"))]));
    } else {
        schema
            .tables
            .insert(name("app.zzz"), table_of(&[("id", column("int"))]));
    }
    schema.tables.insert(name("app.aaa"), ordinary);
    schema
}

/// A parent and a partition of it that the declarations make an ordinary table, beside a
/// table the plan drops.
fn partition_left(declared: bool) -> Schema {
    use pbps_model::{BoundDatum, PartitionBound, PartitionBy, PartitionOf};
    let columns = [
        ("id", column("int").not_null()),
        ("ts", column("date").not_null()),
    ];
    let mut parent = table_of(&columns);
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["ts".into()],
        strategy: Default::default(),
    });
    let mut schema = Schema::default();
    schema.tables.insert(name("app.ev"), parent);
    let mut old = table_of(&columns);
    if !declared {
        old.partition_of = Some(PartitionOf {
            parent: name("app.ev"),
            bound: PartitionBound::Range {
                from: vec![BoundDatum::Value("2020-01-01".into())],
                to: vec![BoundDatum::Value("2021-01-01".into())],
            },
            columns: Default::default(),
        });
        old.columns.clear();
    }
    schema.tables.insert(name("app.ev_old"), old);
    if !declared {
        schema
            .tables
            .insert(name("app.zzz"), table_of(&[("id", column("int"))]));
    }
    schema
}

/// The base and the declarations resolved with the intents a human would write, and the
/// plan between them with the engine given.
fn plan_between(dialect: &Engine, base: &Schema, declared: &Schema, intents: &[Intent]) -> Diffed {
    let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let ids = crate::resolve(declared, &base_ids, intents, &ctx())
        .unwrap()
        .ids;
    diff_partial(
        Side {
            schema: base,
            ids: &base_ids,
        },
        Side {
            schema: declared,
            ids: &ids,
        },
        dialect,
        &Hints::default(),
    )
}

fn drop_table(table: &str) -> Intent {
    Intent::DropTable {
        table: name(table),
        reason: "gone".parse().unwrap(),
    }
}

fn column_ref(text: &str) -> pbps_model::ColumnRef {
    text.parse().unwrap()
}

fn local(text: &str) -> pbps_model::ObjectLocalName {
    text.into()
}

/// One value of every variant of [`DiffError`], each with the fields that reach its words.
fn every_refusal() -> Vec<DiffError> {
    use pbps_dialect::last_column::Block;
    let t = || name("app.t");
    let c = || column_ref("app.t.c");
    vec![
        DiffError::DropRequirement {
            reason: "reason".into(),
        },
        DiffError::IndexParameterRefused {
            table: t(),
            target: "index ix".into(),
            method: "btree",
            parameter: "fillfactor".into(),
        },
        DiffError::FilegroupChangeRefused {
            table: t(),
            target: "the table".into(),
            was: "`a`".into(),
            now: "`b`".into(),
        },
        DiffError::ExclusionRefused {
            table: t(),
            name: local("x"),
            reason: "reason".into(),
        },
        DiffError::SearchRefused {
            name: "config app.docs".parse().unwrap(),
            reason: "reason".into(),
        },
        DiffError::SequenceRefused {
            name: "app.numbers".parse().unwrap(),
            reason: "reason".into(),
        },
        DiffError::ExtensionLifecycleUnsupported {
            name: local("citext"),
            action: "action".into(),
        },
        DiffError::ExtensionDropNeedsDatabase {
            name: local("citext"),
        },
        DiffError::ExtensionDropRefused {
            name: local("citext"),
            found: vec!["one".into(), "two".into()],
        },
        DiffError::ExtensionUpdateNeedsDatabase {
            name: local("citext"),
            from: "1.5".into(),
            to: "1.6".into(),
        },
        DiffError::ExtensionCreateNeedsDatabase {
            name: local("citext"),
        },
        DiffError::ExtensionMoveNeedsDatabase {
            name: local("citext"),
            from: "public".into(),
            to: "ext".into(),
        },
        DiffError::EnumValueUse {
            name: "app.mood".parse().unwrap(),
            label: "new".into(),
            subject: "app.t".into(),
        },
        DiffError::TypeLifecycleUnsupported {
            name: "app.mood".parse().unwrap(),
            reason: "reason".into(),
        },
        DiffError::PartitionDefault {
            column: c(),
            reason: "reason".into(),
        },
        DiffError::IdentityChangeUnsupported { column: c() },
        DiffError::ConversionFromMismatch {
            column: c(),
            recorded: "int".into(),
            actual: "text".into(),
            to: "bigint".into(),
        },
        DiffError::ConversionWithRename { column: c() },
        DiffError::ForeignKeyTypesDiffer {
            table: t(),
            constraint: local("fk"),
            referencing: c(),
            referenced: column_ref("app.u.id"),
            referencing_type: "int".into(),
            referenced_type: "uuid".into(),
            next: "Next.".into(),
        },
        DiffError::ConversionsUnsupported { count: 2 },
        DiffError::SequenceChangeUnsupported { column: c() },
        DiffError::NumberingChangeOnPartitionedTable { column: c() },
        DiffError::NumberingDirectionChange { column: c() },
        DiffError::RowsReferenceLateIdentityRows {
            table: t(),
            referenced: name("app.u"),
        },
        DiffError::GenerationChangeUnsupported { column: c() },
        DiffError::ComputedInputChanged {
            computed: c(),
            column: "d".into(),
            change: "drops",
        },
        DiffError::ComputedFunctionChanged {
            computed: c(),
            function: "app.f".into(),
            change: "alters",
        },
        DiffError::Annotations("annotation".into()),
        DiffError::DataWithoutKey { table: t() },
        DiffError::PermanentReferencesUnlogged {
            table: t(),
            key: local("fk"),
            target: name("app.u"),
        },
        DiffError::PermanentReferencesUnloggedPartition {
            table: t(),
            key: local("fk"),
            target: name("app.u"),
            partition: name("app.u_1"),
        },
        DiffError::TemporalTableChange {
            table: t(),
            what: vec!["one".into(), "two".into()],
        },
        DiffError::PartitionedTableChange {
            table: t(),
            what: vec!["one".into(), "two".into()],
        },
        DiffError::RebuildTable {
            table: t(),
            why: "why".into(),
        },
        DiffError::RebuildIdentity {
            uid: "t_aaaaaa".parse().unwrap(),
        },
        DiffError::DetachedShape {
            table: t(),
            parent: name("app.p"),
            what: vec!["one".into(), "two".into()],
        },
        DiffError::AttachedShape {
            table: t(),
            parent: name("app.p"),
            what: vec!["one".into(), "two".into()],
        },
        DiffError::DataKeyColumnChanged { table: t() },
        DiffError::DataBaselineKeyAbsent { table: t() },
        DiffError::ColumnRenameCycle {
            table: t(),
            columns: ["a".into(), "b".into()].into(),
        },
        DiffError::TableRenameCycle {
            renames: vec![
                (name("app.a"), name("app.b")),
                (name("app.b"), name("app.a")),
            ],
        },
        DiffError::LastColumnReplacementUnorderable {
            table: t(),
            column: "a".into(),
            block: Block::SameName,
        },
        DiffError::LastColumnReplacementUnorderable {
            table: t(),
            column: "a".into(),
            block: Block::Identity,
        },
        DiffError::DependsOnCycle {
            modules: vec![module_id("app.va"), module_id("app.vb")],
        },
        DiffError::SequenceRenameCycle {
            renames: vec![
                (name("app.s_a"), name("app.s_b")),
                (name("app.s_b"), name("app.s_a")),
            ],
        },
        DiffError::DataBaselineKeyOnNewColumn {
            table: t(),
            column: "c".into(),
        },
        DiffError::DataBaselineKeyNotSingle {
            table: t(),
            columns: vec!["a".into(), "b".into()],
        },
    ]
}

/// What each refusal says. The pin sits in the arm of the variant it describes, and the
/// `match` has no wildcard arm, so a variant added to [`DiffError`] fails to compile here
/// until its words are pinned: a message is what a person reads when a plan is refused, and
/// moving the type or a pass must not rephrase one. The number says which variant an arm
/// is, so that the test can tell that every variant was built and not only matched.
fn pinned(error: &DiffError) -> (usize, &'static str) {
    use pbps_dialect::last_column::Block;
    match error {
        DiffError::DropRequirement { .. } => (0, r#"reason"#),
        DiffError::IndexParameterRefused { .. } => (
            1,
            r#"index ix on `app.t`: storage parameter `fillfactor` of a btree index cannot be changed in place, and this model does not rebuild the index for it (DEC-2448.1)"#,
        ),
        DiffError::FilegroupChangeRefused { .. } => (
            2,
            r#"the table on `app.t`: its filegroup would change from `a` to `b`; moving data between filegroups is a rebuild, and no plan does that yet (DEC-2813.1)"#,
        ),
        DiffError::ExclusionRefused { .. } => (
            3,
            r#"exclusion constraint `x` on `app.t`: reason (DEC-2448.3)"#,
        ),
        DiffError::SearchRefused { .. } => (
            4,
            r#"text search configuration `app.docs`: reason (DEC-2728.1)"#,
        ),
        DiffError::SequenceRefused { .. } => (5, r#"sequence `app.numbers`: reason (DEC-2721.1)"#),
        DiffError::ExtensionLifecycleUnsupported { .. } => (
            6,
            r#"extension `citext`: action is unsupported in the read-only extension phase (DEC-2447.1)"#,
        ),
        DiffError::ExtensionDropNeedsDatabase { .. } => (
            7,
            r#"extension `citext`: DROP EXTENSION deletes the package's members and what depends on it, so it is planned only against a database (`plan --db`), where the plan reads and pins what the drop reaches (DEC-2447.8)"#,
        ),
        DiffError::ExtensionDropRefused { .. } => (
            8,
            r#"extension `citext`: DROP EXTENSION would reach what this plan does not remove: one; two; drop or release each in an earlier plan (`ALTER ... NO DEPENDS ON EXTENSION` for an object that is not declared), then drop the package (DEC-2447.8)"#,
        ),
        DiffError::ExtensionUpdateNeedsDatabase { .. } => (
            9,
            r#"extension `citext`: ALTER EXTENSION UPDATE from `1.5` to `1.6` runs the package's update scripts, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what they leave (DEC-2447.10)"#,
        ),
        DiffError::ExtensionCreateNeedsDatabase { .. } => (
            10,
            r#"extension `citext`: CREATE EXTENSION runs the package's install script, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what the script installs (DEC-2447.6)"#,
        ),
        DiffError::ExtensionMoveNeedsDatabase { .. } => (
            11,
            r#"extension `citext`: ALTER EXTENSION SET SCHEMA from `public` to `ext` is judged against the members, owners and relocatability the target holds, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what it leaves (DEC-2447.11)"#,
        ),
        DiffError::EnumValueUse { .. } => (
            12,
            r#"type `app.mood`: new enum label `new` is used by app.t; apply the label first, then the rest"#,
        ),
        DiffError::TypeLifecycleUnsupported { .. } => (13, r#"type `app.mood`: reason"#),
        DiffError::PartitionDefault { .. } => (
            14,
            r#"column app.t.c: its partition default cannot be planned: reason"#,
        ),
        DiffError::IdentityChangeUnsupported { .. } => (
            15,
            r#"the IDENTITY property of app.t.c changed, but IDENTITY cannot be modified with ALTER"#,
        ),
        DiffError::ConversionFromMismatch { .. } => (
            16,
            r#"column app.t.c: a conversion is recorded from `int` to `bigint`, but this column is `text`. An expression written for one starting type is never applied to another. Record one for this start: `pbps convert app.t.c --using - --from 'text'`"#,
        ),
        DiffError::ConversionWithRename { .. } => (
            17,
            r#"column app.t.c: a conversion is recorded for it, and this plan also renames it. The expression names the column once, so apply the rename in a plan of its own first and then plan the type change"#,
        ),
        DiffError::ForeignKeyTypesDiffer { .. } => (
            18,
            r#"foreign key fk of app.t would join app.t.c (`int`) to app.u.id (`uuid`) once this plan has run, and the engine cannot join those two types, so it would refuse the key at the apply. Next."#,
        ),
        DiffError::ConversionsUnsupported { .. } => (
            19,
            r#"the identity file records 2 conversion(s), but this engine has no `USING` clause and converts a column's values itself, so nothing would run the expression. Remove the `conversions` entries from the identity file"#,
        ),
        DiffError::SequenceChangeUnsupported { .. } => (
            20,
            r#"column app.t.c: a `serial` put on an UNLOGGED table, or an owned sequence changed where this engine has none, is not planned yet. Make the change by hand, then pull."#,
        ),
        DiffError::NumberingChangeOnPartitionedTable { .. } => (
            21,
            r#"column app.t.c: its identity changes on a partitioned table, or its numbering source changes on a partition, which is not planned yet (issue #1681). Make the change by hand, then pull."#,
        ),
        DiffError::NumberingDirectionChange { .. } => (
            22,
            r#"column app.t.c: its identity or `serial` sequence changes direction, from counting up to counting down or back, which is not planned. Give the column a new source in two steps, removing the old one first, or make the change by hand, then pull."#,
        ),
        DiffError::RowsReferenceLateIdentityRows { .. } => (
            23,
            r#"rows of app.t reference app.u, whose rows this plan inserts after putting a numbering source on it, once its deleted rows are gone; app.t's rows would be written first. Apply the row changes of app.u in a plan of their own first."#,
        ),
        DiffError::GenerationChangeUnsupported { .. } => (
            24,
            r#"column app.t.c: a change between generated and not generated, or between stored and virtual, has no in-place form. To change it, add a new column and drop this one."#,
        ),
        DiffError::ComputedInputChanged { .. } => (
            25,
            r#"computed column app.t.c may read `d`, which this plan drops, and SQL Server refuses that while the computed column stands. Drop the computed column, or change its expression, in a plan of its own first, then this one."#,
        ),
        DiffError::ComputedFunctionChanged { .. } => (
            26,
            r#"computed column app.t.c may call `app.f`, which this plan alters. Apply the function change and the computed column change in separate plans."#,
        ),
        DiffError::Annotations(_) => (27, r#"annotation"#),
        DiffError::DataWithoutKey { .. } => (
            28,
            r#"app.t declares `data:` but has no single-column primary key, so its rows have no identity"#,
        ),
        DiffError::PermanentReferencesUnlogged { .. } => (
            29,
            r#"app.t: its foreign key `fk` references app.u, which is unlogged, and PostgreSQL lets only an unlogged table reference an unlogged one. Make app.t unlogged too, or app.u permanent."#,
        ),
        DiffError::PermanentReferencesUnloggedPartition { .. } => (
            30,
            r#"app.t: its foreign key `fk` references app.u, whose partition app.u_1 is unlogged; a crash would empty app.u_1 and leave rows of app.t referencing nothing, which PostgreSQL does not refuse through a partitioned table. Make app.u_1 permanent, or app.t unlogged."#,
        ),
        DiffError::TemporalTableChange { .. } => (
            31,
            r#"app.t has `system_time`, and the only change pbps makes to such a table is adding a nullable column; this plan would also one, two. SQL Server refuses some of these and does others by rewriting or deleting the table's history. Declare app.t as it is recorded apart from nullable columns it gains, and make the rest by hand."#,
        ),
        DiffError::PartitionedTableChange { .. } => (
            32,
            r#"app.t is a partitioned table or a partition, and pbps can create one but not change it yet; this plan would one, two. Declare app.t as it is recorded, and make the change by hand."#,
        ),
        DiffError::RebuildTable { .. } => (
            33,
            r#"cannot plan the blocking rewrite of app.t: why. Table rebuild support is tracked in issue #2298"#,
        ),
        DiffError::RebuildIdentity { .. } => (
            34,
            r#"the rebuild intent for t_aaaaaa has no source parent identity"#,
        ),
        DiffError::DetachedShape { .. } => (
            35,
            r#"app.t is detached from app.p, and declared as other than its parent's shape: one; two. Declare it with its parent's columns, keys, constraints and indexes (under any names), and change it in a later revision."#,
        ),
        DiffError::AttachedShape { .. } => (
            36,
            r#"app.t is attached to app.p as a partition, but one; two. Bring the table to its parent's shape in an earlier plan, then attach it."#,
        ),
        DiffError::DataKeyColumnChanged { .. } => (
            37,
            r#"app.t declares `data:` and its primary key moved to another column, so the declared rows cannot be matched to the recorded ones. Remove the block, apply the key change, then declare the rows again — or rename the column instead of replacing it"#,
        ),
        DiffError::DataBaselineKeyAbsent { .. } => (
            38,
            r#"app.t has no baseline primary key and this plan does not restore the declared key, so its data rows cannot be matched"#,
        ),
        DiffError::ColumnRenameCycle { .. } => (
            39,
            r#"app.t renames columns `a`, `b` into each other's names in a cycle, and no order of those renames is one the engine performs. Apply the revisions that form it in separate deployments, or rename one of the columns through a temporary name first"#,
        ),
        DiffError::TableRenameCycle { .. } => (
            40,
            r#"the table renames `app.a -> app.b`, `app.b -> app.a` each need a name another of them still holds, in a cycle, and no order of those renames is one the engine performs. Apply the revisions that form it in separate deployments, or rename one of the tables through a temporary name first"#,
        ),
        DiffError::LastColumnReplacementUnorderable {
            block: Block::SameName,
            ..
        } => (
            41,
            r#"app.t replaces every one of its columns, and no order of those changes is one the engine performs: its only column `a` is replaced by a new column of the same name, and the engine will not drop a table's last column, nor add a name that is still there. Keep the existing column instead of replacing it, or make the change in two revisions: add a column of another kind or name first, then replace it"#,
        ),
        DiffError::LastColumnReplacementUnorderable {
            block: Block::Identity,
            ..
        } => (
            41,
            r#"app.t replaces every one of its columns, and no order of those changes is one the engine performs: its IDENTITY column `a` is replaced by a new IDENTITY column, and the engine will not drop a table's last column, nor hold two IDENTITY columns. Keep the existing column instead of replacing it, or make the change in two revisions: add a column of another kind or name first, then replace it"#,
        ),
        DiffError::DependsOnCycle { .. } => (
            42,
            r#"the `depends_on` declarations of `app.va`, `app.vb` close a cycle: each asks to be created after another of them, and no order honours them all. Remove the one that does not hold"#,
        ),
        DiffError::SequenceRenameCycle { .. } => (
            43,
            r#"the sequence renames `app.s_a -> app.s_b`, `app.s_b -> app.s_a` each need a name another of them still holds, in a cycle, and no order of those renames is one the engine performs. Apply the revisions that form it in separate deployments, or rename one of the sequences through a temporary name first"#,
        ),
        DiffError::DataBaselineKeyOnNewColumn { .. } => (
            44,
            r#"app.t has no baseline primary key and this plan restores it on `c`, a column the baseline does not have, so the recorded data rows cannot be matched to the declared ones. Restore the key on a column the table already has, or remove the block, apply the key change, then declare the rows again"#,
        ),
        DiffError::DataBaselineKeyNotSingle { .. } => (
            45,
            r#"app.t has a baseline primary key with 2 columns (["a", "b"]); pbps matches data rows only on a single-column key"#,
        ),
    }
}

/// The count of variants of [`DiffError`]: the arms of [`pinned`] number them from zero.
const REFUSALS: usize = 46;

#[test]
fn every_refusal_says_exactly_what_it_said_and_every_variant_is_built() {
    let mut built = [0usize; REFUSALS];
    for error in every_refusal() {
        let (slot, words) = pinned(&error);
        assert_eq!(error.to_string(), words);
        built[slot] += 1;
    }
    // Negative: a variant no example builds would leave its arm untested.
    assert!(built.iter().all(|n| *n > 0), "{built:?}");
    // The one variant whose words depend on a field beyond its object is built both ways.
    let unorderable =
        |e: &DiffError| matches!(e, DiffError::LastColumnReplacementUnorderable { .. });
    assert_eq!(every_refusal().iter().filter(|e| unorderable(e)).count(), 2);
}

// ---- A. the order of the refusals across the phases ----

/// What every phase of the differ refuses in `every_phase_refuses`, in the order the call
/// reports it. The marks name the phase of `diff_partial_rebuilding` that finds each: the
/// prelude (extensions, sequences, conversions, text search, cycles of `depends_on`), the
/// types, the tables, the passes over what the tables made, the facts the sort is built
/// from, and the finish of the sort.
const EVERY_PHASE: [&str; 20] = [
    // prelude
    "DependsOnCycle",
    "ExtensionLifecycleUnsupported",
    "SequenceRefused",
    "ConversionsUnsupported",
    "SearchRefused",
    // types
    "TypeLifecycleUnsupported(app.loose, labels)",
    "TypeLifecycleUnsupported(app.e1, rename cycle)",
    "TypeLifecycleUnsupported(app.e2, rename cycle)",
    // tables: created, unlogged, then the surviving tables
    "DataWithoutKey",
    "PermanentReferencesUnlogged",
    "FilegroupChangeRefused",
    // passes over the changes the tables made
    "ForeignKeyTypesDiffer",
    "ExclusionRefused",
    // facts of the sort
    "ColumnRenameCycle",
    "SequenceRenameCycle",
    "TypeLifecycleUnsupported(app.d1, domain cycle)",
    "TypeLifecycleUnsupported(app.d2, domain cycle)",
    // finish of the sort
    "TableRenameCycle",
    "LastColumnReplacementUnorderable",
    "EnumValueUse",
];

fn cyclic_modules() -> Hints {
    let mut hints = Hints::default();
    hints
        .annotations
        .declare_module(module_id("app.va"), [module_id("app.vb")].into());
    hints
        .annotations
        .declare_module(module_id("app.vb"), [module_id("app.va")].into());
    hints
}

/// The project of `every_phase_refuses` planned from its base through its three revisions,
/// with the hand edits to the identities that no `resolve` makes: a conversion on an
/// engine without `USING`, a type and a text search object nothing records, and a column
/// replaced by a new one of its own name.
fn plan_every_phase(dialect: &Engine, hints: &Hints) -> Diffed {
    let revisions: Vec<_> = (1..=3)
        .map(|s| (every_phase_refuses(s), every_phase_refuses_intents(s)))
        .collect();
    through_revisions(
        dialect,
        hints,
        &every_phase_refuses(0),
        &revisions,
        |base_ids, declared| {
            let uid = declared.columns.keys().next().unwrap().clone();
            declared.conversions.insert(
                uid,
                Conversion {
                    from: "int".into(),
                    to: "bigint".into(),
                    using: "x::bigint".into(),
                    allowed_at: "2026-11-21".into(),
                    operator: "leon".into(),
                },
            );
            // `resolve` matches a declared name to the baseline's column of that name, so
            // the new identity under the old name is made by hand.
            let at = pbps_model::ColumnRef::new(name("app.last"), "a");
            let old = declared.column_uid(&at).cloned().unwrap();
            declared.columns.remove(&old);
            declared.columns.insert("c_new000".parse().unwrap(), at);
            declared.tombstones.insert(
                old,
                pbps_model::Tombstone {
                    was: "app.last.a".into(),
                    dropped_at: "2026-11-20".into(),
                    reason: "gone".into(),
                    operator: "operator".into(),
                    require: None,
                },
            );
            // A type no identity names is one the differ cannot match by uid.
            let loose: pbps_model::TypeRef = "app.loose".parse().unwrap();
            declared.types.retain(|_, n| *n != loose);
            base_ids.types.retain(|_, n| *n != loose);
            declared.search.clear();
        },
    )
}

fn full_engine() -> Engine {
    Engine {
        numbering: true,
        last_column: true,
        strict_keys: true,
        ..Engine::default()
    }
}

/// Every phase of the differ refuses something in one call, and the refusals come out in
/// the order of the phases: the order a pass pushes is the order a person reads, and a pass
/// that returned its refusals to be appended later would move them. Planned afresh each
/// time, since the identities are minted and a pass that read them in their order would
/// pass once in a while for the wrong reason.
#[test]
fn refusals_of_every_phase_come_out_in_the_order_of_the_phases() {
    for _ in 0..8 {
        let planned = plan_every_phase(&full_engine(), &cyclic_modules());
        assert_eq!(labels(&planned.errors), EVERY_PHASE, "{:?}", planned.errors);
    }
}

/// Negative: a refusal whose trigger is absent is absent, and the others keep their order.
/// An engine that does not refuse a table's last column, that holds no table to a key's
/// types, and a project whose modules do not wait on each other, lose three refusals and
/// move none of the rest.
#[test]
fn a_refusal_whose_trigger_is_absent_leaves_the_others_where_they_were() {
    let engine = Engine {
        numbering: true,
        ..Engine::default()
    };
    let planned = plan_every_phase(&engine, &Hints::default());
    let expected: Vec<&str> = EVERY_PHASE
        .iter()
        .copied()
        .filter(|label| {
            ![
                "DependsOnCycle",
                "ForeignKeyTypesDiffer",
                "LastColumnReplacementUnorderable",
            ]
            .contains(label)
        })
        .collect();
    assert_eq!(labels(&planned.errors), expected, "{:?}", planned.errors);
}

/// The first phase can end the call: a lineage record the engine cannot hold is the one
/// refusal, and the plan is empty, whatever the later phases would have found. This is the
/// pair of the first phase and the prelude; the second early return (an approved rebuild,
/// which answers with the package differences alone) needs a qualified rebuild of a
/// partitioned table and is not pinned here.
#[test]
fn a_lineage_refusal_ends_the_call_before_any_later_phase_reports() {
    let record = |declared: &mut IdsFile| {
        let root = declared.tables.keys().next().unwrap().clone();
        declared.rebuilds.insert(
            "t_aaaaaa".parse().unwrap(),
            pbps_model::Rebuild {
                source: pbps_model::RebuildIdentities {
                    tables: Default::default(),
                    columns: Default::default(),
                },
                replacement: pbps_model::RebuildIdentities {
                    tables: Default::default(),
                    columns: Default::default(),
                },
                replacement_root: root,
                continues: Default::default(),
                target_checksum: String::new(),
                max_rows: 1,
                allowed_at: "2026-11-21".into(),
                reason: "why".parse().unwrap(),
                operator: "leon".into(),
                types: Default::default(),
                conversions: Default::default(),
            },
        );
    };
    let revisions: Vec<_> = (1..=3)
        .map(|s| (every_phase_refuses(s), every_phase_refuses_intents(s)))
        .collect();
    let planned = through_revisions(
        &full_engine(),
        &cyclic_modules(),
        &every_phase_refuses(0),
        &revisions,
        |_, declared| record(declared),
    );
    assert_eq!(
        labels(&planned.errors),
        ["RebuildTable"],
        "{:?}",
        planned.errors
    );
    assert!(planned.changes.changes.is_empty());
    // Negative: without the record the same call reports the prelude first.
    let planned = plan_every_phase(&full_engine(), &cyclic_modules());
    assert_eq!(label(&planned.errors[0]), "DependsOnCycle");
    assert!(!planned.changes.changes.is_empty());
}

// ---- C. the sort ----

/// The plan of `every_class`, in the order of the classes of `order_key` and the ranks
/// inside them that `sort_class` gives. The comments say which (class, rank) each change is
/// in where the class orders within itself.
const EVERY_CLASS: [&str; 56] = [
    // class 0: a policy and a module before any module drop's rank, a role, and a replica
    // identity that stands first, at (0, 1)
    "DropPolicy app.alter",
    "DropModule app.v_drop",
    "DropRole role old_role",
    "SetReplicaIdentity app.keyed",
    // class 1
    "RenameTable app.old_name",
    "RenameType app.ren_t",
    "RenameRole role ren_from",
    // class 2: constraint, index and domain drops (2, 1); the generated column's drop
    // that frees a renamed column's name (2, 2), then the ordinary one (2, 3); the
    // computed column's at the class's end (2, 4)
    "DropForeignKey app.alter",
    "DropCheck app.alter",
    "DropIndex app.alter ix_old",
    "DropUnique app.alter",
    "AlterDomain app.dom Loosen",
    "SetPrimaryKey app.keyed drop",
    "DropColumn app.new_name.g1",
    "DropColumn app.new_name.d1",
    "DropComputedColumn app.new_name",
    // class 3
    "RenameColumn app.new_name.r1->r2",
    // class 4
    "Revoke role writer",
    // class 5
    "DropColumn app.alter.gone",
    // class 6: a numbering source given up at (6, 1), with the table drop
    "AlterColumnNumbering app.alter.s",
    "DropTable app.dropped",
    // class 7: a label and a type first (7, 0), then the table (7, 1)
    "AddEnumValue app.mood",
    "EnumRebuild app.mood2 Create",
    "CreateType app.new_t",
    "CreateTable app.created",
    // class 8
    "AddColumn app.alter.fresh",
    // class 9: the rebuilt enum's retype and swap (9, 1), then the alterations, the
    // generated column's add (9, 2), the computed column's (9, 3) and the persistence
    // switches (9, 10 + depth)
    "EnumRebuild app.mood2 Retype",
    "EnumRebuild app.mood2 Swap",
    "AlterColumnType app.alter.n",
    "AlterColumnDefault app.alter.d",
    "AddColumn app.alter.ga",
    "AddComputedColumn app.alter",
    "SetTablePersistence app.keyed",
    // class 10
    "SetColumnDeprecated app.alter",
    "SetStorageParameters app.alter",
    // class 11
    "InsertRow app.alter",
    "UpdateRow app.alter",
    // class 12: the delete, then the tightening of a table whose rows the plan writes (12, 2)
    "DeleteRow app.alter",
    "AlterColumnNullability app.alter.req",
    // class 13: a key and constraints, an index and an exclusion; a domain's tightening;
    // the foreign key after the key it references
    "SetPrimaryKey app.keyed add",
    "AddCheck app.alter",
    "AddIndex app.alter ix_new",
    "AddUnique app.alter",
    "SetExclusion app.alter",
    "AlterDomain app.dom Tighten",
    "AddForeignKey app.alter",
    // class 14: modules, then a policy (14, 2) and the row security flag (14, 3)
    "AlterModule app.v_alter",
    "CreateModule app.v_new",
    "CreatePolicy app.alter",
    "CreatePolicy app.created",
    "SetRowSecurity app.alter",
    "SetRowSecurity app.created",
    // class 15, 16, 17, 18
    "CreateRole role new_role",
    "Grant role new_role",
    "SetDataMode app.modal",
    "DropType app.old_t",
];

fn plan_every_class(base: &Schema, declared: &Schema) -> Diffed {
    let engine = Engine {
        numbering: true,
        ..Engine::default()
    };
    plan_between(&engine, base, declared, &every_class_intents())
}

/// One plan with a change of every class of the sort that a schema pair can reach through
/// the entry point, in the exact order of their kinds, including the changes that tie on a
/// class and are ordered inside it. Planned afresh each time: the order among changes that
/// tie to the last key is the rendering's, and the uids in it are minted.
#[test]
fn one_plan_holds_a_change_of_every_class_in_the_order_of_the_sort() {
    for _ in 0..5 {
        let planned = plan_every_class(&every_class(false), &every_class(true));
        assert!(planned.errors.is_empty(), "{:?}", planned.errors);
        assert_eq!(kinds(&planned), EVERY_CLASS);
    }
}

/// Negative: a plan without the roles, which a different pass makes, keeps every other
/// change in the order it had: the order is the sort's and does not come from the changes
/// that happen to be beside it.
#[test]
fn a_plan_without_the_roles_keeps_the_order_of_the_rest() {
    let (mut base, mut declared) = (every_class(false), every_class(true));
    base.roles.clear();
    declared.roles.clear();
    let intents: Vec<Intent> = every_class_intents()
        .into_iter()
        .filter(|i| !matches!(i, Intent::RenameRole { .. } | Intent::DropRole { .. }))
        .collect();
    let planned = plan_between(
        &Engine {
            numbering: true,
            ..Engine::default()
        },
        &base,
        &declared,
        &intents,
    );
    assert!(planned.errors.is_empty(), "{:?}", planned.errors);
    let expected: Vec<&str> = EVERY_CLASS
        .iter()
        .copied()
        .filter(|k| !k.contains(" role "))
        .collect();
    assert_eq!(kinds(&planned), expected);
}

/// A partition is created after every other create of its class, though its name sorts
/// first, and a partitioned parent's index is added before the others, though its table
/// sorts after theirs: both by a rank inside the class, which a rename of the key's
/// arguments would lose. The same names with no partition in them are ordered by name.
#[test]
fn a_partition_is_created_last_in_its_class_and_a_parents_index_first_in_its() {
    let planned = plan_between(
        &Engine::default(),
        &partition_tree(false, true),
        &partition_tree(true, true),
        &[drop_table("app.zzz")],
    );
    assert!(planned.errors.is_empty(), "{:?}", planned.errors);
    assert_eq!(
        kinds(&planned),
        [
            "DropTable app.zzz",
            "CreateTable app.mid",
            "CreateTable app.ev_new",
            "AddIndex app.ev ix_p",
            "AddIndex app.aaa ix_a",
        ]
    );
    // Negative: with no partition in the plan, the name decides inside both classes.
    let planned = plan_between(
        &Engine::default(),
        &partition_tree(false, false),
        &partition_tree(true, false),
        &[drop_table("app.zzz")],
    );
    assert!(planned.errors.is_empty(), "{:?}", planned.errors);
    assert_eq!(
        kinds(&planned),
        [
            "DropTable app.zzz",
            "CreateTable app.ev_new",
            "CreateTable app.mid",
            "AddIndex app.aaa ix_a",
            "AddIndex app.ev ix_p",
        ]
    );
}

/// A detach comes after every table drop of its class, though its name sorts first: it
/// claims the names of what it keeps, and a dropped table's key may hold one.
#[test]
fn a_detach_follows_every_table_drop_of_its_class() {
    let planned = plan_between(
        &Engine::default(),
        &partition_left(false),
        &partition_left(true),
        &[drop_table("app.zzz")],
    );
    assert!(planned.errors.is_empty(), "{:?}", planned.errors);
    assert_eq!(
        kinds(&planned),
        ["DropTable app.zzz", "DetachPartition app.ev_old"]
    );
}

// ---- B. a later pass reads what an earlier one pushed ----
//
// Four readers, and where each is pinned:
//
// 1. The rows of a table are compared as the plan's label renames leave them: the prelude
//    pushes the renames and the table pass reads them. Pinned already by
//    `rows_that_follow_a_label_rename_are_no_change_of_rows` and
//    `a_bound_that_follows_a_label_rename_is_not_a_change_of_partitioning` (`tests.rs`).
// 2. The exclusion pass reads the retypes of the whole plan, an enum rebuild's among them,
//    which the prelude pushed. Pinned below.
// 3. The engine's teardown of a bound module reads the whole plan. Pinned in `pbps-mssql`
//    (`engine_effects.rs`: `a_bound_view_is_dropped_before_and_created_after_a_retype_of_a_column_it_reads`,
//    `a_bound_view_is_rebuilt_only_around_the_changes_the_engine_refuses`,
//    `a_bound_view_goes_before_the_drop_or_rename_of_its_table`,
//    `a_bound_view_edited_beside_a_retype_is_dropped_and_created_not_altered`); pinned below
//    with a plain dialect too, for the retypes of both the table pass and the prelude.
// 4. A module the dialect and the caller both ask to rebuild, or that the declarations also
//    changed, is rebuilt once: the rebuild reads what the module pass pushed. Pinned in
//    `enum_labels.rs` for the enum dependents
//    (`a_module_the_plan_also_changes_comes_down_before_the_retypes_and_returns_with_its_new_text`),
//    and below for the dialect's candidates and the caller's own set.

fn mood_slot(labels: &[&str], held: &str) -> Schema {
    let mut t = keyed(
        &[
            ("id", column("int").not_null()),
            ("m", column("app.mood")),
            ("o", column("int")),
        ],
        "id",
    );
    t.exclude.insert("no_clash".into(), exclusion_over(held));
    let mut schema = Schema::default();
    schema.tables.insert(name("app.slot"), t);
    schema
        .types
        .insert("app.mood".parse().unwrap(), enum_of(labels));
    schema
}

/// An exclusion constraint holding a column an enum rebuild retypes is dropped before the
/// retype and added after it. The retype is pushed by the prelude, a pass before the
/// tables, and the exclusion pass reads the whole plan once the tables are in.
#[test]
fn an_exclusion_is_stated_around_the_retype_an_enum_rebuild_pushed_earlier() {
    let stated = |old: &[&str], new: &[&str], held: &str| -> Vec<String> {
        kinds(&plan_between(
            &Engine::default(),
            &mood_slot(old, held),
            &mood_slot(new, held),
            &[],
        ))
    };
    let around = stated(&["a", "b"], &["b"], "m");
    let position = |what: &str| around.iter().position(|k| k.starts_with(what)).unwrap();
    assert_eq!(
        around
            .iter()
            .filter(|k| k.starts_with("SetExclusion"))
            .count(),
        2,
        "{around:?}"
    );
    assert!(position("EnumRebuild app.mood Create") < position("EnumRebuild app.mood Retype"));
    let teardown = around
        .iter()
        .position(|k| k.starts_with("SetExclusion"))
        .unwrap();
    let restored = around
        .iter()
        .rposition(|k| k.starts_with("SetExclusion"))
        .unwrap();
    assert!(
        teardown < position("EnumRebuild app.mood Retype"),
        "{around:?}"
    );
    assert!(
        restored > position("EnumRebuild app.mood Swap"),
        "{around:?}"
    );
    // Negative: a constraint on a column the rebuild does not retype is left alone.
    let apart = stated(&["a", "b"], &["b"], "o");
    assert!(
        !apart.iter().any(|k| k.starts_with("SetExclusion")),
        "{apart:?}"
    );
    // Negative: an added label is no retype, so nothing is stated.
    let added = stated(&["a"], &["a", "b"], "m");
    assert!(
        !added.iter().any(|k| k.starts_with("SetExclusion")),
        "{added:?}"
    );
    assert!(
        added.iter().any(|k| k.starts_with("AddEnumValue")),
        "{added:?}"
    );
}

fn bound_world(bound_text: &str, retyped: bool, label_removed: bool) -> Schema {
    let mut t = table_of(&[
        ("id", column("int").not_null()),
        ("a", column(if retyped { "bigint" } else { "int" })),
    ]);
    if label_removed {
        t.columns.insert("m".into(), column("app.mood"));
    }
    let mut schema = Schema::default();
    schema.tables.insert(name("app.t"), t);
    schema
        .modules
        .insert(module_id("app.bound"), view(bound_text));
    if label_removed {
        schema
            .types
            .insert("app.mood".parse().unwrap(), enum_of(&["b"]));
    }
    schema
}

/// A module the engine binds to a column's type is dropped and created around a retype in
/// the plan, whichever pass made the retype, and is not also altered: the teardown reads the
/// plan after the table pass and the module pass, and takes the module's own edit with it.
#[test]
fn a_bound_module_is_torn_down_around_a_retype_a_pass_before_it_made() {
    let engine = Engine {
        bound_to_retypes: [module_id("app.bound")].into(),
        ..Engine::default()
    };
    let plan =
        |base: &Schema, declared: &Schema| kinds(&plan_between(&engine, base, declared, &[]));
    let modules = |order: &[String]| -> Vec<String> {
        order
            .iter()
            .filter(|k| k.contains("app.bound"))
            .cloned()
            .collect()
    };
    let torn = ["DropModule app.bound", "CreateModule app.bound"];
    // The table pass's retype.
    let order = plan(
        &bound_world("SELECT a FROM app.t", false, false),
        &bound_world("SELECT a FROM app.t", true, false),
    );
    assert_eq!(modules(&order), torn, "{order:?}");
    // And with the module edited in the same plan: a drop and a create, never an alter.
    let order = plan(
        &bound_world("SELECT a FROM app.t", false, false),
        &bound_world("SELECT a, id FROM app.t", true, false),
    );
    assert_eq!(modules(&order), torn, "{order:?}");
    // Negative: no retype, no teardown, and the edit alone is an alter.
    let order = plan(
        &bound_world("SELECT a FROM app.t", false, false),
        &bound_world("SELECT a FROM app.t", false, false),
    );
    assert!(modules(&order).is_empty(), "{order:?}");
    let order = plan(
        &bound_world("SELECT a FROM app.t", false, false),
        &bound_world("SELECT a, id FROM app.t", false, false),
    );
    assert_eq!(modules(&order), ["AlterModule app.bound"], "{order:?}");
}

/// The retype of an enum rebuild is pushed by the prelude; the teardown sees it too.
#[test]
fn a_bound_module_is_torn_down_around_the_retype_of_an_enum_rebuild() {
    let engine = Engine {
        bound_to_retypes: [module_id("app.bound")].into(),
        ..Engine::default()
    };
    let mut base = bound_world("SELECT m FROM app.t", false, true);
    base.types
        .insert("app.mood".parse().unwrap(), enum_of(&["a", "b"]));
    let declared = bound_world("SELECT m FROM app.t", false, true);
    let order = kinds(&plan_between(&engine, &base, &declared, &[]));
    assert!(
        order
            .iter()
            .any(|k| k.starts_with("EnumRebuild app.mood Retype")),
        "{order:?}"
    );
    let torn: Vec<_> = order.iter().filter(|k| k.contains("app.bound")).collect();
    assert_eq!(
        torn,
        ["DropModule app.bound", "CreateModule app.bound"],
        "{order:?}"
    );
    // Negative: a label added instead of removed is no retype.
    let mut base = bound_world("SELECT m FROM app.t", false, true);
    base.types
        .insert("app.mood".parse().unwrap(), enum_of(&["b"]));
    let mut declared = bound_world("SELECT m FROM app.t", false, true);
    declared
        .types
        .insert("app.mood".parse().unwrap(), enum_of(&["b", "c"]));
    let order = kinds(&plan_between(&engine, &base, &declared, &[]));
    assert!(!order.iter().any(|k| k.contains("app.bound")), "{order:?}");
}

/// A module the dialect asks to rebuild whenever something arrives, one the caller asks
/// for, and one the declarations edit, are rebuilt once each however many of the three ask:
/// the rebuild reads which modules the module pass already changed.
#[test]
fn a_module_both_asked_for_and_already_changed_is_rebuilt_once() {
    let world = |edited: bool, arrives: bool| {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(name("app.t"), table_of(&[("id", column("int"))]));
        if arrives {
            schema
                .tables
                .insert(name("app.arrival"), table_of(&[("id", column("int"))]));
        }
        schema.modules.insert(module_id("app.va"), view("SELECT 1"));
        schema.modules.insert(
            module_id("app.vb"),
            view(if edited { "SELECT 22" } else { "SELECT 2" }),
        );
        schema.modules.insert(module_id("app.vc"), view("SELECT 3"));
        schema
    };
    let plan = |dialect: &Engine, also: &[&str], base: &Schema, declared: &Schema| -> Vec<String> {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let ids = crate::resolve(declared, &base_ids, &[], &ctx())
            .unwrap()
            .ids;
        let also: BTreeSet<ModuleId> = also.iter().map(|m| module_id(m)).collect();
        let plan = diff_rebuilding(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &ids,
            },
            dialect,
            &Hints::default(),
            &also,
        )
        .unwrap();
        plan.changes()
            .changes
            .iter()
            .map(|p| kind(&p.change))
            .filter(|k| k.contains("Module"))
            .collect()
    };
    // `va` is asked for by the dialect and by the caller, `vb` is edited and asked for by
    // both, `vc` by the caller alone; a name no side declares is asked for and ignored.
    let engine = Engine {
        rebinds: [module_id("app.va"), module_id("app.vb")].into(),
        ..Engine::default()
    };
    let rebuilt = plan(
        &engine,
        &["app.va", "app.vb", "app.vc", "app.ghost"],
        &world(false, false),
        &world(true, true),
    );
    assert_eq!(
        rebuilt,
        [
            "AlterModule app.va",
            "AlterModule app.vb",
            "AlterModule app.vc"
        ],
        "{rebuilt:?}"
    );
    // Negative: with nothing asked for, the unchanged modules stay, and the edit is one alter.
    let rebuilt = plan(
        &Engine::default(),
        &[],
        &world(false, false),
        &world(true, true),
    );
    assert_eq!(rebuilt, ["AlterModule app.vb"], "{rebuilt:?}");
    // Negative: nothing arrives, so the dialect's candidates are not asked for.
    let rebuilt = plan(&engine, &[], &world(false, false), &world(true, false));
    assert_eq!(rebuilt, ["AlterModule app.vb"], "{rebuilt:?}");
}

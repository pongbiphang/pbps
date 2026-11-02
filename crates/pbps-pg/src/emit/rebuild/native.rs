use super::*;
use pbps_db::{Conn, Driver};
use pbps_diff::{
    Side,
    order::{catalog::graph::*, rebuild::*},
};
use pbps_model::*;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
mod cases;
mod snapshot {
    include!("../../../tests/support/catalog_snapshot.rs");
}

fn owned_snapshot(
    mut pulled: crate::introspect::Pulled,
    role_prefix: &str,
) -> crate::introspect::Pulled {
    pulled.schema = snapshot::owned_roles(pulled.schema, |name| name.starts_with(role_prefix));
    pulled
}

#[derive(Clone)]
struct Case {
    name: &'static str,
    tree: bool,
    before: Json,
    after: Json,
    rows: Json,
    cap: u64,
}
impl Case {
    fn member(name: &'static str, before: Json, after: Json, rows: Json) -> Self {
        Self {
            name,
            tree: false,
            before,
            after,
            cap: rows
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|r| r[1].as_array().unwrap())
                .count()
                .max(1) as u64,
            rows,
        }
    }
}
#[derive(Clone, Copy)]
enum Selector {
    Copy,
    Index,
    Check,
    Swap,
    Restore,
}
impl Selector {
    fn matches(self, change: &Change, purpose: Purpose) -> bool {
        match self {
            Self::Copy => matches!(change, Change::CopyTableRows(_)),
            Self::Index => matches!(change, Change::BuildPartitionedIndex(_)),
            Self::Check => matches!(change, Change::AddCheck { .. }),
            Self::Swap => purpose == Purpose::Swap,
            Self::Restore => {
                purpose == Purpose::Restoration && matches!(change, Change::CreateModule { .. })
            }
        }
    }
}
#[derive(Clone, Copy)]
enum Fault {
    Natural,
    Interrupt,
    Denied,
}
type FailureCase = (Case, Selector, usize, Fault, &'static str);
fn patch(value: &mut Json, overlay: &Json) {
    if let Json::Object(fields) = overlay {
        for (key, child) in fields {
            if child.is_null() {
                value.as_object_mut().unwrap().remove(key);
            } else {
                if value.get(key).is_none() {
                    value[key] = json!({});
                }
                patch(&mut value[key], child);
            }
        }
    } else {
        *value = overlay.clone();
    }
}
fn side<'a>(schema: &'a Schema, ids: &'a IdsFile) -> Side<'a> {
    Side { schema, ids }
}
fn context() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "maintainer".into(),
        today: "2026-10-24".into(),
    }
}
async fn execute(conn: &mut Conn, sql: &str) {
    conn.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}
async fn value(conn: &mut Conn, sql: &str) -> Json {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    assert_eq!(rows.len(), 1);
    serde_json::from_str(rows[0].try_get::<&str>("v").unwrap().unwrap()).unwrap()
}
async fn rows(conn: &mut Conn, name: &TableName, projection: &str, only: bool) -> Json {
    value(conn, &format!("SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text),'[]')::text AS v FROM (SELECT {projection} FROM {}{} AS src) r", if only {"ONLY "}else{""}, qualified(name).unwrap())).await
}
async fn insert(conn: &mut Conn, data: &Json) {
    for row in data.as_array().unwrap() {
        let name: TableName = row[0].as_str().unwrap().parse().unwrap();
        let name = qualified(&name).unwrap();
        execute(conn,&format!(
            "INSERT INTO {name} SELECT * FROM jsonb_populate_recordset(NULL::{name}, {}::jsonb)",
            literal(&row[1].to_string())
        )).await;
    }
}
async fn bootstrap(conn: &mut Conn, schema: &Schema, hints: &Hints) {
    let namespaces: BTreeSet<_> = schema.tables.keys().map(|n| &n.schema).collect();
    for namespace in namespaces {
        execute(
            conn,
            &format!("CREATE SCHEMA IF NOT EXISTS {}", quote(namespace).unwrap()),
        )
        .await;
    }
    let mut empty = Schema::default();
    for role in schema.roles.keys() {
        empty.roles.insert(role.clone(), Role::default());
    }
    let base_ids = pbps_diff::resolve(&empty, &IdsFile::default(), &[], &context())
        .unwrap()
        .ids;
    let ids = pbps_diff::resolve(schema, &base_ids, &[], &context())
        .unwrap()
        .ids;
    // Full catalog fixtures include preinstalled packages, not install requests.
    let problems = crate::resolver::scratch::extension_prerequisites(conn, &schema.extensions)
        .await
        .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(side(schema, &ids));
    empty.extensions = prerequisites.extensions;
    let mut base_ids = base_ids;
    base_ids.extensions = prerequisite_ids.extensions;
    let plan = pbps_diff::diff(
        side(&empty, &base_ids),
        side(schema, &ids),
        &Postgres::new(),
        hints,
    )
    .unwrap();
    for change in &plan.changes().changes {
        for statement in emit(&Postgres::new(), &change.change, change.strategy).unwrap() {
            execute(conn, &statement.sql).await;
        }
    }
}
async fn manifest(conn: &mut Conn, schema: &Schema) -> Json {
    let mut snapshot = value(conn, r#"SELECT jsonb_build_object(
        'classes',(SELECT jsonb_agg(to_jsonb(c)-ARRAY['relpages','reltuples','relallvisible','relallfrozen','relfrozenxid','relminmxid'] ORDER BY c.oid) FROM pg_class c),
        'attributes',(SELECT jsonb_agg(to_jsonb(a) ORDER BY attrelid,attnum) FROM pg_attribute a),
        'types',(SELECT jsonb_agg(to_jsonb(t) ORDER BY oid) FROM pg_type t),
        'constraints',(SELECT jsonb_agg(to_jsonb(c) ORDER BY oid) FROM pg_constraint c),
        'indexes',(SELECT jsonb_agg(to_jsonb(i) ORDER BY indexrelid) FROM pg_index i),
        'inherits',(SELECT jsonb_agg(to_jsonb(i) ORDER BY inhrelid,inhseqno) FROM pg_inherits i),
        'dependencies',(SELECT jsonb_agg(to_jsonb(d) ORDER BY classid,objid,objsubid,refclassid,refobjid,refobjsubid,deptype) FROM pg_depend d),
        'routines',(SELECT jsonb_agg(to_jsonb(p) ORDER BY oid) FROM pg_proc p),
        'triggers',(SELECT jsonb_agg(to_jsonb(t) ORDER BY oid) FROM pg_trigger t)
    )::text AS v"#).await;
    for name in schema.tables.keys() {
        snapshot["rows"][name.raw().to_string()] = rows(conn, name, "src.*", true).await;
    }
    snapshot
}
async fn owned(case: Case, failure: Option<(Selector, usize, Fault, &'static str)>) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("owned PostgreSQL setting");
        let token = crate::catalog::probe_token().replace('-', "_");
        let db = format!("pbps_pr3_{token}");
        let reference = format!("{db}_ref");
        let role = format!("{db}_reader");
        let denied = format!("{db}_denied");
        let unrelated = format!("pbps_noise_{token}");
        let role_prefix = format!("{db}_");
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        for name in [&db, &reference] {
            execute(&mut admin, &format!("CREATE DATABASE {name}")).await;
        }
        execute(
            &mut admin,
            &format!("CREATE ROLE {role}; CREATE ROLE {denied}"),
        )
        .await;
        let connection = format!("{base} dbname={db}");
        let ref_connection = format!("{base} dbname={reference}");
        let label = format!("{variable}: {}", case.name);
        let case = case.clone();
        let roles = (role.clone(), denied.clone(), unrelated.clone());
        let result = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(exercise(
                    connection,
                    ref_connection,
                    roles,
                    role_prefix,
                    case,
                    failure,
                ))
                .await
            })
            .await;
        for name in [&db, &reference] {
            execute(&mut admin, &format!("DROP DATABASE {name} WITH (FORCE)")).await;
        }
        execute(&mut admin, &format!("DROP ROLE {role}, {denied}")).await;
        execute(&mut admin, &format!("DROP ROLE IF EXISTS {unrelated}")).await;
        result.unwrap_or_else(|e| panic!("{label}: {e}"));
    }
}

#[tokio::test]
#[ignore = "needs owned PostgreSQL 16 and 18"]
async fn replacement_preserves_declared_rows_ownership_and_external_bindings() {
    for case in cases::successes() {
        success_oracle(case).await;
    }
}
#[tokio::test]
#[ignore = "needs owned PostgreSQL 16 and 18"]
async fn failed_physical_prefix_restores_the_exact_opening_manifest() {
    for failure in cases::failures() {
        failure_oracle(failure).await;
    }
}
async fn success_oracle(case: Case) {
    owned(case, None).await;
}
async fn failure_oracle((case, selector, statement, fault, state): FailureCase) {
    owned(case, Some((selector, statement, fault, state))).await;
}

async fn exercise(
    connection: String,
    reference: String,
    (role, denied, unrelated): (String, String, String),
    role_prefix: String,
    case: Case,
    failure: Option<(Selector, usize, Fault, &'static str)>,
) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    let mut reference = Conn::connect(Driver::Postgres, &reference).await.unwrap();
    let mut before = if case.tree {
        execute(
            &mut reference,
            include_str!("../../../tests/support/rebuild_tree.sql"),
        )
        .await;
        let schema = owned_snapshot(
            crate::catalog::introspect(&mut reference).await.unwrap(),
            &role_prefix,
        )
        .schema;
        execute(&mut reference, "DROP SCHEMA app CASCADE").await;
        schema
    } else {
        pbps_diff::rebuild::member_cases::source()
    };
    let table = TableName::new("app", if case.tree { "parent" } else { "p" });
    let mut wire = serde_json::to_value(&before).unwrap();
    if !case.tree {
        patch(
            &mut wire,
            &json!({"tables":{"app.p":{"columns":{"k":{"nullable":false},"bucket":{"nullable":false},"payload":{"type":"text"},"doc":{"type":"jsonb","nullable":true}}}}}),
        );
    }
    patch(&mut wire, &case.before);
    before = serde_json::from_value(wire.clone()).unwrap();
    let key = before.tables[&table].partition_by.as_ref().unwrap().columns[0].clone();
    wire["tables"][table.raw().to_string()]["columns"][key.raw()]["type"] = json!("bigint");
    patch(&mut wire, &case.after);
    let mut desired: Schema = serde_json::from_value(wire).unwrap();
    if case.tree {
        let grants = ["app.parent", "app.left_view", "app.composite(app.parent)"]
            .into_iter()
            .map(|target| {
                let permission = if target.contains('(') {
                    Permission::Execute
                } else {
                    Permission::Select
                };
                (target.parse().unwrap(), [permission].into())
            })
            .collect();
        let grant = Role {
            grants,
            ..Role::default()
        };
        before.roles.insert(role.clone().into(), grant.clone());
        desired.roles.insert(role.into(), grant);
    }
    let hints = Hints {
        module_deps: cases::module_edges()
            .into_iter()
            .filter(|(id, _)| before.modules.contains_key(id))
            .collect(),
        public_execute: before
            .modules
            .iter()
            .filter(|(_, m)| m.kind == ModuleKind::Function)
            .map(|(id, _)| id.clone())
            .collect(),
        ..Hints::default()
    };
    bootstrap(&mut conn, &before, &hints).await;
    bootstrap(&mut reference, &desired, &hints).await;
    insert(&mut conn, &case.rows).await;
    let before = owned_snapshot(
        crate::catalog::introspect(&mut conn).await.unwrap(),
        &role_prefix,
    )
    .schema;
    let expected = owned_snapshot(
        crate::catalog::introspect(&mut reference).await.unwrap(),
        &role_prefix,
    );
    // Deterministic witness for concurrent cluster-role churn after the reference
    // snapshot: this role belongs to neither database fixture's namespace.
    execute(&mut reference, &format!("CREATE ROLE {unrelated}")).await;
    let declared = desired;
    let mut desired = expected.schema.clone();
    for (name, table) in &mut desired.tables {
        if let (Some(observed), Some(wanted)) =
            (&mut table.primary_key, &declared.tables[name].primary_key)
        {
            observed.name.clone_from(&wanted.name);
        }
    }
    let pg = Postgres::new();
    let mut ids = pbps_diff::resolve(&before, &IdsFile::default(), &[], &context())
        .unwrap()
        .ids;
    // Explicit casts in these native copy oracles need the same recorded
    // conversion intent that the connected route now requires.
    for (name, column) in &before.tables[&table].columns {
        if let Some(target) = desired.tables[&table].columns.get(name)
            && pbps_dialect::Dialect::type_change_needs_using(&pg, &column.ty, &target.ty)
        {
            ids.conversions.insert(
                ids.column_uid(&table.column(name)).unwrap().clone(),
                Conversion {
                    from: column.ty.raw().to_string(),
                    to: target.ty.raw().to_string(),
                    using: format!("{}::{}", quote(name.raw()).unwrap(), target.ty.raw()),
                    allowed_at: context().today,
                    operator: context().operator,
                },
            );
        }
    }
    let recorded = pbps_diff::rebuild::record(
        side(&before, &ids),
        &desired,
        &ids,
        &table,
        case.cap,
        &"native lowering".parse().unwrap(),
        &context(),
        &pg,
    )
    .unwrap();
    let root =
        pbps_diff::rebuild::qualify(side(&before, &ids), side(&desired, &recorded), &table, &pg)
            .unwrap();
    let opening = manifest(&mut conn, &before).await;
    execute(&mut conn, "BEGIN").await;
    let catalog = crate::modules::dependencies::collect(
        &mut conn,
        &Root::Table(ids.table_uid(&table).unwrap().clone()),
        side(&before, &ids),
    )
    .await
    .unwrap();
    let schemas = desired.tables.keys().map(|n| n.schema.clone()).collect();
    let naming = crate::partition_index::collect_names(&mut conn, &schemas)
        .await
        .unwrap();
    let input = Input {
        root: &root,
        before: side(&before, &ids),
        desired: side(&desired, &recorded),
        catalog: &catalog,
        naming: &naming,
        hints: &hints,
        mode: PlanMode::Transactional,
    };
    let prepared = prepare(&input, &pg, &mut |slots, target, released| {
        let mut allocation = crate::partition_index::Allocation::new(
            &root, &catalog, &naming, slots, target, released,
        );
        index_definitions(&desired.tables[&table])
            .into_iter()
            .map(|(part, mut definition)| {
                let name = match &mut definition {
                    Definition::PrimaryKey(pk) => {
                        let name = pk.name.clone().unwrap_or_else(|| {
                            crate::partition_index::clone_name(
                                &table,
                                &part,
                                &Fact::Absent,
                                &Default::default(),
                            )
                            .unwrap()
                        });
                        pk.name = Some(name.clone());
                        name
                    }
                    Definition::Index(_) | Definition::Unique(_) => match &part {
                        IndexPart::Index(name) | IndexPart::Unique(name) => name.clone(),
                        IndexPart::PrimaryKey => unreachable!(),
                    },
                };
                Ok(BuildPartitionedIndex {
                    parent: RebuildRelation {
                        uid: target.table_uid(&table).unwrap().clone(),
                        name: table.clone(),
                    },
                    clones: allocation.clones(&part, &definition)?,
                    definition,
                    name,
                })
            })
            .collect()
    })
    .unwrap();
    let kept: Vec<_> = catalog
        .keys
        .values()
        .filter(|k| !prepared.closure().nodes.contains_key(&k.index))
        .map(|k| k.index.id.to_string())
        .collect();
    let kept_sql = format!(
        "SELECT COALESCE(jsonb_agg(to_jsonb(i) ORDER BY indexrelid),'[]')::text AS v FROM pg_index i WHERE indexrelid=ANY(ARRAY[{}]::oid[])",
        kept.join(",")
    );
    let kept_keys = value(&mut conn, &kept_sql).await;
    let canonical = prepared.operations();
    let purposes: BTreeMap<_, _> = canonical
        .iter()
        .zip(prepared.parts())
        .map(|(o, p)| (o.id(), p.purpose))
        .collect();
    let done = expand(&input, prepared, &pg).unwrap();
    let mut expected_rows = Json::Null;
    if failure.is_none() {
        let copy = done
            .operations
            .iter()
            .find_map(|o| {
                if let Change::CopyTableRows(c) = o.change() {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        let projection = copy
            .columns
            .iter()
            .map(|c| {
                let source = c.source.as_ref().map_or("NULL".to_owned(), |s| {
                    format!("src.{}", quote(s.column.name.raw()).unwrap())
                });
                format!(
                    "({source})::{}{} AS {}",
                    sql_type(&c.cast).unwrap(),
                    c.collation.as_ref().map_or(String::new(), |v| format!(
                        " COLLATE {}",
                        quote(v.as_str()).unwrap()
                    )),
                    quote(c.target.name.raw()).unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        expected_rows = rows(&mut conn, &copy.source.name, &projection, false).await;
    }
    let selected = failure.map(|(selector, statement, _, _)| {
        let op = done
            .operations
            .iter()
            .find(|o| selector.matches(o.change(), purposes[&o.id()]))
            .expect("fault selector must resolve");
        (op.id(), statement)
    });
    let mut reached = false;
    for op in done.operations.iter() {
        let original = canonical.iter().find(|o| o.id() == op.id()).unwrap();
        assert_eq!(op.origin(), original.origin());
        assert_eq!(op.change(), original.change());
        for (position, statement) in emit(&pg, op.change(), op.planned().strategy)
            .unwrap()
            .iter()
            .enumerate()
        {
            assert!(statement.transactional && statement.index_build.is_none());
            let at_fault = selected == Some((op.id(), position));
            if at_fault && matches!(failure.unwrap().2, Fault::Denied) {
                execute(&mut conn, &format!("SET LOCAL ROLE {denied}")).await;
            }
            let result = conn.execute(&statement.sql).await;
            let result = if at_fault && matches!(failure.unwrap().2, Fault::Interrupt) {
                result.unwrap();
                conn.execute("SELECT 1/0").await
            } else {
                result
            };
            if let Err(error) = result {
                assert!(
                    at_fault,
                    "operation {:?}, statement {position}: {error:?}",
                    op.id()
                );
                assert_eq!(
                    error.server_error_code().as_deref(),
                    Some(failure.unwrap().3)
                );
                reached = true;
                break;
            }
        }
        if reached {
            break;
        }
    }
    if failure.is_some() {
        assert!(reached, "selected statement never failed");
        execute(&mut conn, "ROLLBACK").await;
        assert_eq!(manifest(&mut conn, &before).await, opening);
    } else {
        assert_eq!(rows(&mut conn, &table, "src.*", false).await, expected_rows);
        let final_state = owned_snapshot(
            crate::catalog::introspect_within_transaction(&mut conn)
                .await
                .unwrap(),
            &role_prefix,
        );
        assert_eq!(final_state.schema, expected.schema);
        assert_eq!(value(&mut conn, &kept_sql).await, kept_keys);
        final_contract(&mut conn, &mut reference, &done, &catalog, &root).await;
        assert_eq!(final_state.public_execute, expected.public_execute);
        assert!(
            pbps_diff::diff(
                side(&final_state.schema, &done.target),
                side(&desired, &done.target),
                &pg,
                &hints
            )
            .unwrap()
            .changes()
            .is_empty()
        );
        execute(&mut conn, "COMMIT").await;
    }
}

async fn bindings(conn: &mut Conn) -> Json {
    value(conn,r#"WITH endpoints AS (
        SELECT jsonb_build_array(a.type,a.schema,a.name,a.identity,b.type,b.schema,b.name,b.identity,d.deptype) AS edge
        FROM pg_depend d
        CROSS JOIN LATERAL pg_identify_object(d.classid,d.objid,d.objsubid) a
        CROSS JOIN LATERAL pg_identify_object(d.refclassid,d.refobjid,d.refobjsubid) b
        WHERE (d.classid='pg_proc'::regclass AND EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE p.oid=d.objid AND n.nspname NOT LIKE 'pg_%' AND n.nspname<>'information_schema'))
        OR (d.classid='pg_rewrite'::regclass AND EXISTS(SELECT 1 FROM pg_rewrite r JOIN pg_class c ON c.oid=r.ev_class JOIN pg_namespace n ON n.oid=c.relnamespace WHERE r.oid=d.objid AND n.nspname NOT LIKE 'pg_%' AND n.nspname<>'information_schema'))
        OR (d.classid='pg_trigger'::regclass AND EXISTS(SELECT 1 FROM pg_trigger t WHERE t.oid=d.objid AND NOT t.tgisinternal))
        OR (d.classid='pg_constraint'::regclass AND EXISTS(SELECT 1 FROM pg_constraint k WHERE k.oid=d.objid AND k.contype='f'))
    ) SELECT COALESCE(jsonb_agg(edge ORDER BY edge::text),'[]')::text AS v FROM endpoints"#).await
}
async fn final_contract(
    conn: &mut Conn,
    reference: &mut Conn,
    done: &Expanded,
    catalog: &Catalog,
    root: &RebuildPartitionedTable,
) {
    for op in done.operations.iter() {
        if let Change::BuildPartitionedIndex(recipe) = op.change() {
            let parent = &done.target.tables[&recipe.parent.uid];
            let parent_index =
                qualified(&TableName::new(&parent.schema, recipe.name.raw())).unwrap();
            let primary = matches!(recipe.definition, Definition::PrimaryKey(_));
            let key = !matches!(recipe.definition, Definition::Index(_));
            let unique = match &recipe.definition {
                Definition::Index(i) => i.unique,
                Definition::PrimaryKey(_) | Definition::Unique(_) => true,
            };
            for (uid, name, leaf) in std::iter::once((&recipe.parent.uid, &recipe.name, false))
                .chain(recipe.clones.iter().map(|c| (&c.leaf.uid, &c.name, true)))
            {
                let table = &done.target.tables[uid];
                let index = qualified(&TableName::new(&table.schema, name.raw())).unwrap();
                let index_parent = if leaf {
                    format!("to_regclass({})", literal(&parent_index))
                } else {
                    "0".into()
                };
                let constraint_parent = if leaf {
                    format!(
                        "(SELECT oid FROM pg_constraint WHERE conindid=to_regclass({}) AND contype IN('p','u'))",
                        literal(&parent_index)
                    )
                } else {
                    "0".into()
                };
                assert_eq!(value(conn,&format!("SELECT to_jsonb(i.indrelid=to_regclass({}) AND i.indisunique={unique} AND i.indisprimary={primary} AND i.indisvalid AND i.indisready AND COALESCE(h.inhparent,0)={index_parent} AND ((k.oid IS NOT NULL)={key}) AND (NOT {key} OR (k.conparentid={constraint_parent} AND k.conrelid=i.indrelid)))::text AS v FROM pg_index i LEFT JOIN pg_inherits h ON h.inhrelid=i.indexrelid LEFT JOIN pg_constraint k ON k.conindid=i.indexrelid AND k.contype IN('p','u') AND k.conrelid=i.indrelid WHERE i.indexrelid=to_regclass({})",literal(&qualified(table).unwrap()),literal(&index))).await,json!(true));
            }
        }
        if let Change::AnalyzeTableTree(tree) = op.change() {
            for relation in std::iter::once(&tree.parent).chain(&tree.partitions) {
                let name = qualified(&done.target.tables[&relation.uid]).unwrap();
                assert_eq!(value(conn,&format!("SELECT to_jsonb(CASE WHEN (SELECT count(*) FROM {qualified})=0 THEN c.reltuples IN(-1,0) ELSE c.reltuples=(SELECT count(*) FROM {qualified}) AND EXISTS(SELECT 1 FROM pg_statistic s WHERE s.starelid=c.oid AND s.stainherit=(c.relkind='p')) END)::text AS v FROM pg_class c WHERE c.oid={name}::regclass",qualified=name,name=literal(&name))).await,json!(true));
            }
        }
    }
    for address in &done.addresses {
        assert_eq!(
            value(
                conn,
                &format!(
                    "SELECT to_jsonb(to_regclass({}) IS NULL)::text AS v",
                    literal(&qualified(&address.address).unwrap())
                )
            )
            .await,
            json!(true)
        );
    }
    for (object, relation) in &catalog.relations {
        if root.before.contains_key(&relation.name) {
            assert_eq!(value(conn,&format!("SELECT to_jsonb(NOT EXISTS(SELECT 1 FROM pg_class WHERE oid={}) AND NOT EXISTS(SELECT 1 FROM pg_type WHERE oid IN({},{})))::text AS v",object.id,relation.row_type.id,relation.array_type.id)).await,json!(true));
        }
    }
    assert_eq!(bindings(conn).await, bindings(reference).await);
}

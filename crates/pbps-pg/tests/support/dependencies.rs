//! New root evidence never changes the legacy module route's bytes.
use super::TestDb;
use pbps_diff::{
    Side,
    order::catalog::{Holds, Part, graph::*},
};
use pbps_model::{IdsFile, ModuleId, Schema};
use pbps_pg::modules::dependencies::collect;

fn source<'a>(schema: &'a Schema, ids: &'a IdsFile) -> Side<'a> {
    Side { schema, ids }
}
fn root() -> Root {
    Root::Table("t_aaaaaa".parse().unwrap())
}
fn ids() -> IdsFile {
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), "app.parent".parse().unwrap());
    ids
}
const TREE: &str = "CREATE SCHEMA app;
CREATE TABLE app.lookup(id integer PRIMARY KEY);
CREATE TABLE app.unrelated(id integer REFERENCES app.lookup(id));
CREATE TABLE app.parent(id integer NOT NULL, code integer NOT NULL, memo text, CONSTRAINT positive CHECK(id>=0), PRIMARY KEY(id), UNIQUE(id,code), FOREIGN KEY(code) REFERENCES app.lookup(id)) PARTITION BY RANGE(id);
CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM(0) TO(100);
CREATE TABLE app.incoming(id integer NOT NULL, parent_id integer REFERENCES app.parent(id)) PARTITION BY RANGE(id);
CREATE TABLE app.incoming_child PARTITION OF app.incoming FOR VALUES FROM(0) TO(100);
CREATE VIEW app.left_view AS SELECT id,code FROM app.parent;
CREATE VIEW app.right_view AS SELECT id FROM app.parent;
CREATE VIEW app.diamond AS SELECT l.id FROM app.left_view l JOIN app.right_view r USING(id);
CREATE FUNCTION app.composite(app.parent) RETURNS app.parent LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION app.composite_array(app.parent[]) RETURNS app.parent[] LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION app.on_row() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER parent_trigger BEFORE INSERT ON app.parent FOR EACH ROW EXECUTE FUNCTION app.on_row();";

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn table_catalog_keeps_physical_edges_bindings_and_clone_owners() {
    let mut db = TestDb::create("catalog2298b2").await;
    db.conn.execute(TREE).await.unwrap();
    let pulled = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
    let ids = ids();
    db.conn.execute("BEGIN").await.unwrap();
    let found = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    let closure = found.checked(&root(), &pulled.schema).unwrap();
    assert!(found.graph.nodes.values().all(
        |n| !matches!(n, Fact::Present(Node { described, .. }) if described.contains("unrelated"))
    ));
    assert!(
        found
            .relations
            .values()
            .any(|r| r.name == "app.parent".parse().unwrap()
                && r.attributes.values().any(|(n, _)| n.raw() == "id")
                && r.row_type.id != 0
                && r.array_type.id != 0)
    );
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::ConstraintTrigger,
            ..
        }
    )));
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::PartitionClone,
            ..
        }
    )));
    let version = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap()[0]
        .try_get::<i32>("version")
        .unwrap()
        .unwrap();
    if version >= 180000 {
        assert!(
            closure
                .nodes
                .values()
                .any(|n| n.described.contains("not_null")
                    && matches!(
                        n.held,
                        Held::Owned {
                            kind: Ownership::Attribute,
                            ..
                        }
                    ))
        );
    }
    for predicate in [
        Binding::Argument {
            position: 0,
            array: false,
        },
        Binding::Result { array: false },
        Binding::Argument {
            position: 0,
            array: true,
        },
        Binding::Result { array: true },
    ] {
        assert!(
            closure.edges.iter().any(|(_, e)| e.binding == predicate),
            "{predicate:?}"
        );
    }
    assert!(
        closure
            .edges
            .iter()
            .any(|(_, e)| matches!(e.binding, Binding::ForeignKey { .. }))
    );
    assert!(
        found
            .keys
            .values()
            .any(|key| !key.referenced_columns.is_empty() && key.columns.iter().all(|c| c.id != 0))
    );
    assert!(
        !closure
            .surfaces
            .values()
            .any(|d| d.described.contains("incoming_child") || d.described.contains("unrelated")),
        "clones are not independent restorations"
    );
    assert!(closure.surfaces.values().any(|d| matches!(
        d.holds,
        Holds::TablePart {
            part: Part::ForeignKey(_),
            ..
        }
    ) && d.described.contains("incoming")));
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::Storage,
            ..
        }
    )));
    assert_eq!(
        closure
            .surfaces
            .values()
            .filter(|d| matches!(
                d.holds,
                Holds::TablePart {
                    part: Part::Check(_),
                    ..
                }
            ))
            .count(),
        1
    );
    let diamond = closure
        .nodes
        .iter()
        .find(|(_, n)| n.described == "view app.diamond")
        .unwrap()
        .0;
    let rule = closure
        .nodes
        .iter()
        .find(|(_, n)| matches!(&n.held,Held::Owned{by,kind:Ownership::ReturnRule} if by==diamond))
        .unwrap()
        .0;
    assert!(
        closure
            .edges
            .iter()
            .filter(|(_, e)| &e.dependent == rule)
            .count()
            >= 2
    );
    let mut unmanaged = pulled.schema.clone();
    unmanaged
        .modules
        .remove(&"app.diamond".parse::<ModuleId>().unwrap());
    assert!(
        found
            .checked(&root(), &unmanaged)
            .unwrap_err()
            .contains("app.diamond")
    );
    let module: ModuleId = "app.left_view".parse().unwrap();
    let before = pbps_pg::modules::dependents(&mut db.conn, &module, pbps_model::ModuleKind::View)
        .await
        .unwrap();
    let module_root = Root::Module(module.clone());
    let module_facts = collect(&mut db.conn, &module_root, source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(module_facts.checked(&module_root, &pulled.schema).is_ok());
    assert_eq!(
        before,
        pbps_pg::modules::dependents(&mut db.conn, &module, pbps_model::ModuleKind::View)
            .await
            .unwrap()
    );
    db.conn
        .execute("SAVEPOINT missing; DROP TABLE app.child CASCADE")
        .await
        .unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("app.child")
    );
    db.conn.execute("ROLLBACK TO missing").await.unwrap();
    db.conn
        .execute("CREATE MATERIALIZED VIEW app.unsupported AS SELECT id FROM app.parent; CREATE STATISTICS app.unsupported_stats ON id,code FROM app.parent")
        .await
        .unwrap();
    let unknown = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(unknown.graph.nodes.values().any(|n| matches!(n,Fact::Present(Node{described,held:Held::Dependent(Holds::Unrepresentable(_)),..}) if described.contains("app.unsupported_stats"))));
    assert!(
        unknown
            .checked(&root(), &pulled.schema)
            .unwrap_err()
            .contains("app.unsupported")
    );
    db.conn.execute("DROP MATERIALIZED VIEW app.unsupported; DROP STATISTICS app.unsupported_stats; CREATE OR REPLACE VIEW app.left_view AS SELECT id, 1 AS code FROM app.diamond").await.unwrap();
    let cyclic = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(
        cyclic
            .checked(&root(), &pulled.schema)
            .unwrap_err()
            .contains("cycle")
    );
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn publication_modes_missing_trees_and_unreadable_catalogs_are_named() {
    let mut db = TestDb::create("catalog2298pub").await;
    db.conn.execute("CREATE SCHEMA app; CREATE TABLE app.parent(id integer NOT NULL) PARTITION BY RANGE(id)").await.unwrap();
    let pulled = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
    let ids = ids();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("transaction")
    );
    db.conn.execute("BEGIN").await.unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap()
            .checked(&root(), &pulled.schema)
            .is_ok()
    );
    for definition in [
        "FOR TABLE app.parent WITH (publish_via_partition_root=true)",
        "FOR ALL TABLES",
        "FOR TABLES IN SCHEMA app",
    ] {
        db.conn
            .execute(&format!("CREATE PUBLICATION pub2298 {definition}"))
            .await
            .unwrap();
        let found = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap();
        let error = found.checked(&root(), &pulled.schema).unwrap_err();
        assert!(
            error.contains("pub2298") && error.contains("app.parent"),
            "{error}"
        );
        db.conn.execute("DROP PUBLICATION pub2298").await.unwrap();
    }
    db.conn.execute("CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM(0) TO(100); CREATE PUBLICATION pub2298 FOR TABLE app.parent WITH (publish_via_partition_root=true)").await.unwrap();
    let mut child_ids = IdsFile::default();
    child_ids
        .tables
        .insert("t_aaaaaa".parse().unwrap(), "app.child".parse().unwrap());
    let child_schema = pbps_pg::catalog::introspect_within_transaction(&mut db.conn)
        .await
        .unwrap()
        .schema;
    assert!(
        collect(&mut db.conn, &root(), source(&child_schema, &child_ids))
            .await
            .unwrap()
            .checked(&root(), &child_schema)
            .unwrap_err()
            .contains("pub2298")
    );
    db.conn.execute("DROP PUBLICATION pub2298").await.unwrap();
    let role = format!("catalog2298_reader_{}", std::process::id());
    for catalog in ["pg_depend", "pg_publication"] {
        db.conn.execute("SAVEPOINT permissions").await.unwrap();
        db.conn.execute(&format!("CREATE ROLE {role}; GRANT USAGE ON SCHEMA app TO {role}; REVOKE SELECT ON pg_catalog.{catalog} FROM PUBLIC; SET LOCAL ROLE {role}")).await.unwrap();
        let error = collect(&mut db.conn, &root(), source(&child_schema, &ids))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("42501"), "{error:?}");
        let mut cause = &error;
        while let pbps_db::DbError::Context { source, .. } = cause {
            cause = source;
        }
        assert!(
            matches!(cause,pbps_db::DbError::Driver{code:Some(code),..} if code=="42501"),
            "{error:?}"
        );
        db.conn.execute("ROLLBACK TO permissions").await.unwrap();
    }
    db.conn
        .execute("DROP TABLE app.parent CASCADE")
        .await
        .unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("app.parent")
    );
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

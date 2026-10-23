//! Slice-one catalog projection, without a type lifecycle emitter.
use super::user_types_tests::{Scratch, servers};

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn managed_types_and_their_columns_survive_pull_recreation_and_repull() {
    for server in servers() {
        let mut db = Scratch::create(&server, "managed1635").await;
        let definitions = "CREATE EXTENSION IF NOT EXISTS plpgsql; CREATE COLLATION app._phone FROM pg_catalog.\"C\"; CREATE TYPE app.mood AS ENUM ('sad', 'ok', 'happy');
            CREATE DOMAIN app.phone AS varchar(20) COLLATE app._phone DEFAULT '0' NOT NULL
                CONSTRAINT phone_digits CHECK (VALUE ~ '^[0-9+ -]+$');
            CREATE DOMAIN app.int_range AS int4range;
            CREATE DOMAIN app.int_multirange AS int4multirange;
            CREATE DOMAIN app.range_array AS int4range[];
            CREATE TABLE app.t (mood app.mood, phone app.phone);
            CREATE TYPE app.pair AS (n integer);
            CREATE TABLE app.composite_ref (pair app.pair);
            CREATE TYPE app.span AS RANGE (subtype = integer);
            CREATE TABLE app.unsupported (span app.span);
            CREATE TYPE app.extension_mood AS ENUM ('external');
            ALTER EXTENSION plpgsql ADD TYPE app.extension_mood;";
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute(definitions).await.unwrap();
        let first = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
        let mood = pbps_model::TypeRef::new("app", "mood").unwrap();
        let phone = pbps_model::TypeRef::new("app", "phone").unwrap();
        assert_eq!(first.schema.types.len(), 5, "{:?}", first.schema.types);
        for (name, expected) in [
            ("app.int_range", "pg_catalog.int4range"),
            ("app.int_multirange", "pg_catalog.int4multirange"),
            ("app.range_array", "pg_catalog.int4range[]"),
        ] {
            let definition = &first.schema.types[&name.parse().unwrap()];
            assert!(
                matches!(definition, pbps_model::ManagedType::Domain { base, .. } if *base == expected.parse().unwrap())
            );
            assert!(
                pbps_dialect::Dialect::validate_type(
                    &pbps_pg::Postgres::new(),
                    &name.parse().unwrap(),
                    definition
                )
                .is_empty()
            );
        }
        assert!(
            matches!(&first.schema.types[&mood], pbps_model::ManagedType::Enum { values } if values == &["sad", "ok", "happy"])
        );
        assert!(
            matches!(&first.schema.types[&phone], pbps_model::ManagedType::Domain { base, not_null: true, default: Some(_), checks, collation: Some(c) } if base.raw().to_string() == "character varying(20)" && checks.contains_key("phone_digits") && c.name() == "_phone")
        );
        let table = &first.schema.tables[&"app.t".parse().unwrap()];
        assert_eq!(table.columns["mood"].ty.type_ref(), Some(&mood));
        assert_eq!(table.columns["phone"].ty.type_ref(), Some(&phone));
        assert!(
            first
                .schema
                .tables
                .contains_key(&"app.composite_ref".parse().unwrap())
        );
        assert!(
            !first
                .schema
                .tables
                .contains_key(&"app.unsupported".parse().unwrap())
        );
        assert!(
            first
                .warnings
                .iter()
                .any(|warning| warning.contains("app.unsupported"))
        );
        db.conn
            .execute("DROP SCHEMA app CASCADE; CREATE SCHEMA app")
            .await
            .unwrap();
        db.conn.execute(definitions).await.unwrap();
        let second = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
        // Roles are cluster-wide, and the suite's other tests create theirs
        // between the two reads; what recreation must preserve is the schema's
        // own objects.
        assert_eq!(first.schema.tables, second.schema.tables);
        assert_eq!(first.schema.types, second.schema.types);
        assert_eq!(first.schema.modules, second.schema.modules);
        db.drop().await;
    }
}

use super::*;
use pbps_db::{Conn, Driver};

fn assert_closed(value: &Value, major: u32) {
    assert!(
        references(value, major).is_ok(),
        "PG{major}: the actual bound must qualify"
    );
    if let Value::Node(node) = value {
        if matches!(
            node.tag.as_str(),
            "PARTITIONBOUNDSPEC" | "PARTITIONRANGEDATUM"
        ) {
            let mut unknown = node.clone();
            unknown.fields.insert("future_field".into(), Value::Null);
            assert_eq!(
                references(&Value::Node(unknown), major),
                Err(Uncovered::Field)
            );
            for field in node.fields.keys() {
                let mut missing = node.clone();
                missing.fields.remove(field);
                assert_eq!(
                    references(&Value::Node(missing), major),
                    Err(Uncovered::Field),
                    "PG{major}: missing {field}"
                );
            }
        }
        for child in node.fields.values() {
            assert_closed(child, major);
        }
    } else if let Value::List(children) = value {
        for child in children {
            assert_closed(child, major);
        }
    }
}

#[test]
fn resolved_partition_bound_layouts_are_closed() {
    // This is the resolved shape observed on both supported majors. A nested
    // literal must still expose its type and collation to the existing walker.
    let text = "{PARTITIONBOUNDSPEC :strategy r :is_default false :modulus 0 :remainder 0 :listdatums <> :lowerdatums ({PARTITIONRANGEDATUM :kind 0 :value {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 0 0 0 0 0 0 0 0 ]} :location -1}) :upperdatums ({PARTITIONRANGEDATUM :kind 1 :value <> :location -1}) :location -1}";
    for major in [16, 18] {
        let tree = decode(text, major).unwrap();
        assert_closed(&tree, major);
        assert!(
            references(&tree, major)
                .unwrap()
                .iter()
                .any(|r| r.class == ReferenceClass::Type && r.oid == 23)
        );
        let Value::Node(mut wrong_child) = tree else {
            panic!("bound root")
        };
        wrong_child
            .fields
            .insert("lowerdatums".into(), Value::Atom("unreadable".into()));
        assert_eq!(
            references(&Value::Node(wrong_child), major),
            Err(Uncovered::Field)
        );
    }
    for major in [15, 17, 19] {
        assert!(decode(text, major).is_err());
    }
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn actual_partition_bounds_have_closed_layouts_on_both_majors() {
    let mut outcomes = Vec::new();
    for (variable, major) in [("PBPS_TEST_PG_OLD_DB", 16), ("PBPS_TEST_PG_DB", 18)] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let name = format!(
            "pbps_bound1566_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let result = tokio::task::LocalSet::new().run_until(async move {
            tokio::task::spawn_local(async move {
                let mut conn = Conn::connect(Driver::Postgres,&connection).await.unwrap();
                let version = conn.query("SELECT current_setting('server_version_num') AS v").await.unwrap();
                let actual:u32 = version[0].try_get::<&str>("v").unwrap().unwrap().parse().unwrap();
                assert_eq!(actual/10000,major);
                conn.execute("CREATE SCHEMA app; CREATE TABLE app.r(id integer) PARTITION BY RANGE(id); CREATE TABLE app.r0 PARTITION OF app.r FOR VALUES FROM (MINVALUE) TO (0); CREATE TABLE app.r1 PARTITION OF app.r FOR VALUES FROM (0) TO (10); CREATE TABLE app.rd PARTITION OF app.r DEFAULT; CREATE TABLE app.l(id integer) PARTITION BY LIST(id); CREATE TABLE app.l1 PARTITION OF app.l FOR VALUES IN (1,NULL); CREATE TABLE app.ld PARTITION OF app.l DEFAULT; CREATE TABLE app.h(id integer) PARTITION BY HASH(id); CREATE TABLE app.h0 PARTITION OF app.h FOR VALUES WITH (MODULUS 2, REMAINDER 0)").await.unwrap();
                let rows=conn.query("SELECT c.relpartbound::text AS bound FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='app' AND c.relpartbound IS NOT NULL ORDER BY c.relname").await.unwrap();
                assert_eq!(rows.len(),6);
                for row in rows {
                    let text=row.try_get::<&str>("bound").unwrap().unwrap();
                    let tree=decode(text,major).unwrap();
                    assert_closed(&tree,major);
                }
                // Capture uses the same rule identity as sealed prerequisites;
                // qualification must reach real engine rendering as well.
                let scope=super::super::CaptureScope {
                    retained: ["r0","r1","rd","l1","ld","h0"].map(|name| pbps_db::resolver::capture::ObjectIdentity { class:"pg_class".into(),name:vec!["app".into(),name.into()],signature:vec![] }).into(),
                    candidates:Default::default(),
                };
                super::super::capture(&mut conn,&scope).await.expect("all measured partition-bound layouts must pass capture");
            }).await
        }).await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        println!(
            "PG{major}: six actual partition-bound layouts checked; passed={}",
            result.is_ok()
        );
        outcomes.push((major, result.is_ok()));
    }
    assert!(
        outcomes.iter().all(|(_, ok)| *ok),
        "closed bound coverage after cleanup: {outcomes:?}"
    );
}

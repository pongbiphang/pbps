use super::*;
#[path = "../../../pbps-diff/tests/fixtures/creation2469_nullability.rs"]
mod blockers;
use super::inline::required;
use pbps_model::operation::{ModuleEdits, Snapshot};

#[test]
fn tightening_blockers_follow_the_extracted_not_null_on_the_canonical_route() {
    fn check<E: Dialect>(engine: E) {
        for kind in blockers::BLOCKERS {
            let f = blockers::fixture(kind, engine.name());
            let plan = f.compile(&engine).unwrap_or_else(|e| panic!("{kind}: {e}"));
            blockers::assert_order(plan.changes(), kind, engine.name());
            let mut cyclic = f.clone();
            let provider = match *kind {
                "unique" => "unique::app.t.uq_nn",
                "primary_key" => "primary_key::app.t",
                _ => "index::app.t.ix_nn",
            };
            cyclic.deps.0[1].1 = [provider.parse().unwrap()].into();
            if engine.name() == "postgres" && *kind != "primary_key" {
                // PostgreSQL permits tightening under an already installed key.
                assert!(cyclic.compile(&engine).is_ok(), "{kind}");
                cyclic
                    .deps
                    .0
                    .push((provider.parse().unwrap(), [f.owner.parse().unwrap()].into()));
            }

            assert!(cyclic.compile(&engine).is_err(), "{kind}: genuine cycle");
        }
    }
    check(pbps_mssql::Mssql);
    check(pbps_pg::Postgres::new().with_server_version(160015));
    check(pbps_pg::Postgres::new().with_server_version(180006));
}

#[test]
fn unrelated_keys_checks_and_defaults_do_not_block_tightening() {
    for kind in ["unrelated", "check", "default"] {
        let f = blockers::fixture(kind, "mssql");
        let plan = f.compile(&pbps_mssql::Mssql).unwrap();
        let prefix = blockers::prefix(plan.changes());
        match kind {
            "unrelated" => assert!(prefix.unique.contains_key("uq_nn")),
            "check" => assert!(prefix.checks.contains_key("ck_nn")),
            "default" => assert!(matches!(
                prefix.columns["x"].value_source,
                pbps_model::ValueSource::Default(_)
            )),
            _ => unreachable!(),
        }
    }
    let f = required::require(required::temporal(true), "not_null::app.t.anchor");
    let error = f.compile(&pbps_mssql::Mssql).unwrap_err();
    assert!(
        error.contains("inline") && error.contains("primary key"),
        "{error}"
    );
}

#[test]
fn nullability_blockers_keep_empty_demand_bytes() {
    fn check<E: Dialect>(engine: E) {
        for kind in blockers::BLOCKERS {
            let f = blockers::fixture(kind, engine.name());
            let empty = Default::default();
            let empty_ids = Default::default();
            let input = pbps_diff::diff(
                pbps_diff::Side {
                    schema: &empty,
                    ids: &empty_ids,
                },
                pbps_diff::Side {
                    schema: &f.schema,
                    ids: &f.ids,
                },
                &engine,
                &Default::default(),
            )
            .unwrap();
            let mut legacy = pbps_diff::order::propose(input.changes().clone());
            let mut annotated = legacy
                .clone()
                .with_annotations(
                    Snapshot {
                        schema: &empty,
                        ids: &empty_ids,
                    },
                    Snapshot {
                        schema: &f.schema,
                        ids: &f.ids,
                    },
                    &ObjectDeps::default(),
                    &ObjectDeps::default(),
                    if engine.rebuilds_modules() {
                        ModuleEdits::Rebuilt
                    } else {
                        ModuleEdits::InPlace
                    },
                )
                .unwrap();
            pbps_diff::order::generated::expand(&mut legacy, &f.ids, &Default::default(), &engine)
                .unwrap();
            pbps_diff::order::generated::expand(
                &mut annotated,
                &f.ids,
                &Default::default(),
                &engine,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_vec(&(legacy.changes(), legacy.proof())).unwrap(),
                serde_json::to_vec(&(annotated.changes(), annotated.proof())).unwrap(),
                "{kind}"
            );
        }
    }
    check(pbps_mssql::Mssql);
    check(pbps_pg::Postgres::new());
}

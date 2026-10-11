include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../pbps-dialect/tests/support/plan_ids.rs"));

pub trait FixturePreflight: pbps_dialect::Dialect {
    fn fixture_preflight(&self, changes: &ChangeSet) -> pbps_dialect::Preflight {
        self.fixture_preflight_with(changes, &pbps_model::Partitions::default())
    }
    fn fixture_preflight_with(&self, changes: &ChangeSet, partitions: &pbps_model::Partitions) -> pbps_dialect::Preflight {
        let ids = ids(changes);
        let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).expect("consistent fixture identities");
        self.preflight_with(&names, partitions)
    }
}
impl<T: pbps_dialect::Dialect + ?Sized> FixturePreflight for T {}

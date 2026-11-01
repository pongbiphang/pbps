//! Check the shared restoration decision against real before/after ledger state.
use pbps_diff::{
    Side,
    schema_diff::{ModuleDelta, module_delta, restoration_comparison},
};
use pbps_model::{Change, ChangeSet, ModuleId, StateSnapshot, TableName};

pub fn check(
    before: &StateSnapshot,
    after: &StateSnapshot,
    changes: &ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
) {
    let modules: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|p| {
            p.change
                .module_id()
                .filter(|id| matches!(id, ModuleId::Trigger { .. }))
                .map(|id| (&p.change, id))
        })
        .collect();
    assert_eq!(modules.len(), 2, "{changes:?}");
    assert!(matches!(modules[0].0, Change::DropModule { .. }));
    assert!(matches!(modules[1].0, Change::CreateModule { .. }));
    let base = Side {
        schema: &before.schema,
        ids: &before.ids,
    };
    let declared = Side {
        schema: &after.schema,
        ids: &after.ids,
    };
    let unchanged = restoration_comparison(base, declared, dialect);
    let retained = restoration_comparison(declared, declared, dialect);
    for (change, id) in modules {
        let ModuleId::Trigger { on, .. } = id else {
            unreachable!()
        };
        let table = TableName::new(on.schema.clone(), on.name.clone());
        assert_ne!(before.ids.table_uid(&table), after.ids.table_uid(&table));
        // Text alone is unchanged: the different answer must come from identity.
        assert_eq!(
            module_delta(
                &before.schema.modules[id],
                &after.schema.modules[id],
                dialect
            ),
            ModuleDelta::Unchanged
        );
        assert!(!unchanged(change, None), "replacement: {change:?}");
        assert!(retained(change, None), "same owner: {change:?}");
    }
}

use super::*;
use std::cell::Cell;

thread_local! {
    static PROJECTIONS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record_projection() {
    PROJECTIONS.set(PROJECTIONS.get() + 1);
}

#[test]
fn dependent_checks_project_each_saved_step_once() {
    let root: ModuleId = "app.root0".parse().unwrap();
    let mut changes = ChangeSet {
        changes: (0..32)
            .map(|i| {
                PlannedChange::new(Change::DropModule {
                    id: format!("app.root{i}").parse().unwrap(),
                    kind: ModuleKind::View,
                })
            })
            .collect(),
    };
    let found = BTreeMap::from([(
        root,
        (0..32)
            .map(|i| Dependent {
                described: format!("view app.dependent{i}"),
                holds: Holds::Module(format!("app.dependent{i}").parse().unwrap()),
            })
            .collect(),
    )]);
    let mut counts = Vec::new();
    for removed in [false, true] {
        if removed {
            changes.changes.insert(
                0,
                PlannedChange::new(Change::DropModule {
                    id: "app.dependent0".parse().unwrap(),
                    kind: ModuleKind::View,
                }),
            );
        }
        PROJECTIONS.set(0);
        let refused = fixture::unaccounted(&changes, &found, &Partitions::default());
        assert_eq!(refused.len(), if removed { 31 } else { 32 });
        assert_eq!(
            refused
                .iter()
                .any(|r| r.starts_with("view app.dependent0 ")),
            !removed
        );
        let projections = PROJECTIONS.get();
        println!(
            "steps={} dependents=32 projections={projections}",
            changes.changes.len()
        );
        counts.push((changes.changes.len(), projections));
    }
    for (steps, projections) in counts {
        assert_eq!(projections, steps);
    }
}

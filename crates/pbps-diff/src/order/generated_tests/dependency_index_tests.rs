//! Pin membership work and the pre-fix edge/settlement byte order.
use super::super::*;
use std::cell::Cell;
use std::cmp::Ordering;

thread_local! {
    static COMPARISONS: Cell<usize> = const { Cell::new(0) };
    static LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
}
#[derive(Clone, Copy, Debug)]
struct Position(usize);
fn comparison() {
    COMPARISONS.with(|count| {
        count.set(count.get() + 1);
        LIMIT.with(|limit| {
            assert!(
                count.get() <= limit.get(),
                "module-edge membership work exceeded its bound"
            )
        });
    });
}
impl PartialEq for Position {
    fn eq(&self, other: &Self) -> bool {
        comparison();
        self.0 == other.0
    }
}
impl Eq for Position {}
impl PartialOrd for Position {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Position {
    fn cmp(&self, other: &Self) -> Ordering {
        comparison();
        self.0.cmp(&other.0)
    }
}

fn module(definition: String) -> Module {
    Module {
        kind: ModuleKind::View,
        definition,
        description: None,
        options: Default::default(),
    }
}

#[test]
fn dense_module_edges_use_bounded_membership_work_with_and_without_annotations() {
    let n = 200;
    let ids: Vec<ModuleId> = (0..n)
        .map(|i| format!("app.m{i:04}").parse().unwrap())
        .collect();
    let definitions: Vec<_> = (0..n)
        .map(|i| {
            module(format!(
                "SELECT 1{}",
                (0..i)
                    .map(|j| format!(", app.m{j:04}()"))
                    .collect::<String>()
            ))
        })
        .collect();
    let modules: Vec<_> = ids
        .iter()
        .zip(&definitions)
        .enumerate()
        .map(|(i, (id, module))| (Position(i), id, module))
        .collect();
    for annotated in [false, true] {
        let mut edges = Vec::new();
        if annotated {
            for before in 0..n {
                for after in before + 1..n {
                    edges.push((Position(before), Position(after), true));
                }
            }
        }
        let budget = 64 * n * n;
        COMPARISONS.set(0);
        LIMIT.set(budget);
        infer_module_dependencies(&modules, &pbps_dialect::MinimalDialect, false, &mut edges);
        let work = COMPARISONS.get();
        eprintln!("annotated={annotated}, modules={n}, comparisons={work}, bound={budget}");
        LIMIT.set(usize::MAX);
        assert!(work > n * n && work <= budget, "{annotated}: {work}");
        assert_eq!(edges.len(), n * (n - 1) / 2);
        assert!(
            edges
                .iter()
                .all(|(a, b, explicit)| a.0 < b.0 && *explicit == annotated)
        );
    }
}

// Frozen bee05f28 relation: retained only for small compatibility fixtures.
fn before_fix(
    modules: &[ModuleStep<'_>],
    deps: &ModuleDeps,
    scratch: bool,
) -> Vec<(usize, usize, bool)> {
    let dialect = pbps_dialect::MinimalDialect;
    let mut edges: Vec<_> =
        crate::order::annotations::modules(modules.iter().map(|(at, id, _)| (*at, *id)), deps)
            .into_iter()
            .map(|e| (e.before, e.after, true))
            .collect();
    for (i, id, module) in modules {
        let text = named_text(module, &dialect, scratch);
        for (j, provider, _) in modules {
            if i == j || id.name() == provider.name() {
                continue;
            }
            if !edges.iter().any(|&(a, b, _)| a == *j && b == *i)
                && dialect.generation_may_call(&text, provider.name())
            {
                edges.push((*j, *i, false));
            }
        }
    }
    edges
}
fn settled_bytes(changes: &ChangeSet, edges: &[(usize, usize, bool)]) -> Vec<u8> {
    let mut schedule = Schedule::new(changes.changes.len());
    for &(before, after, explicit) in edges {
        schedule.edge(
            before,
            after,
            if explicit {
                Source::Annotation
            } else {
                Source::Inferred
            },
            OrderReason::Structural,
        );
    }
    match schedule.settle(crate::order::propose(changes.clone())) {
        Ok(s) => serde_json::to_vec(&(s.sequence, s.changes.changes(), s.proof)).unwrap(),
        Err(e) => e.describe(changes).into_bytes(),
    }
}

#[test]
fn indexed_module_edges_preserve_explicit_inferred_and_same_leaf_fixture_bytes() {
    let ids: Vec<ModuleId> = ["left.v", "right.v", "app.reader", "app.last"]
        .into_iter()
        .map(|id| id.parse().unwrap())
        .collect();
    let definitions: Vec<_> = [
        "SELECT 1",
        "SELECT left.v()",
        "SELECT right.v()",
        "SELECT app.reader()",
    ]
    .into_iter()
    .map(|s| module(s.into()))
    .collect();
    let changes = ChangeSet {
        changes: ids
            .iter()
            .zip(&definitions)
            .map(|(id, module)| {
                PlannedChange::new(Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                })
            })
            .collect(),
    };
    let modules = modules(&changes.changes);
    let explicit: ModuleDeps = [
        (ids[1].clone(), [ids[0].clone()].into()),
        (ids[3].clone(), [ids[0].clone()].into()),
    ]
    .into();
    let cycle: ModuleDeps = [
        (ids[0].clone(), [ids[3].clone()].into()),
        (ids[3].clone(), [ids[0].clone()].into()),
    ]
    .into();
    for deps in [ModuleDeps::default(), explicit, cycle] {
        for scratch in [false, true] {
            let old = before_fix(&modules, &deps, scratch);
            let new = module_dependencies(&modules, &deps, &pbps_dialect::MinimalDialect, scratch);
            assert_eq!(
                serde_json::to_vec(&new).unwrap(),
                serde_json::to_vec(&old).unwrap()
            );
            assert_eq!(settled_bytes(&changes, &new), settled_bytes(&changes, &old));
        }
    }
    let mut duplicate_pair = vec![
        (0, &ids[1], &definitions[1]),
        (1, &ids[2], &definitions[2]),
        (1, &ids[2], &definitions[2]),
    ];
    let old = before_fix(&duplicate_pair, &ModuleDeps::default(), false);
    assert_eq!(old, [(0, 1, false)]);
    assert_eq!(
        module_dependencies(
            &duplicate_pair,
            &ModuleDeps::default(),
            &pbps_dialect::MinimalDialect,
            false
        ),
        old
    );
    duplicate_pair.clear();
    assert!(
        module_dependencies(
            &duplicate_pair,
            &ModuleDeps::default(),
            &pbps_dialect::MinimalDialect,
            false
        )
        .is_empty()
    );
}

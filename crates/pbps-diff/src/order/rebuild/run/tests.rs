use super::*;
use crate::order::rebuild_tests::fixture;
use pbps_dialect::MinimalDialect;

#[test]
fn inventory_checks_risk_strategy_scope_progress_and_budget() {
    let (root, base, target, addresses) = fixture::approval();
    let schema = Schema::default();
    let input = Input {
        root: &root,
        before: crate::Side {
            schema: &schema,
            ids: &base,
        },
        desired: crate::Side {
            schema: &schema,
            ids: &base,
        },
        catalog: &Catalog::default(),
        naming: &Fact::Absent,
        hints: &Hints::default(),
        mode: PlanMode::Transactional,
    };
    let planned = |change| PlannedChange {
        risks: MinimalDialect.change_risks(&change),
        ..PlannedChange::new(change)
    };
    let prepared = Prepared {
        logical: vec![planned(Change::RebuildPartitionedTable(Box::new(
            root.clone(),
        )))],
        target: target.validated().unwrap(),
        addresses,
        parts: fixture::changes()
            .into_iter()
            .skip(1)
            .zip([Purpose::Copy, Purpose::Analyze])
            .map(|(c, purpose)| PartStep {
                change: planned(c),
                purpose,
            })
            .collect(),
        closure: Closure::default(),
        slots: slots::Slots::default(),
        names: CatalogFacts::default(),
    };
    let rule = Rule {
        canonical: prepared.operations(),
        prepared,
        input: &input,
        dialect: &MinimalDialect,
    };
    let plan = || {
        order::propose(ChangeSet {
            changes: rule.prepared.logical.clone(),
        })
    };
    assert!(
        expand::expand(plan(), &rule, 0)
            .unwrap_err()
            .contains("rewrite budget of 0")
    );
    let all: Vec<_> = rule
        .prepared
        .parts
        .iter()
        .map(|p| p.change.clone())
        .collect();
    assert!(rule.select(&all).is_ok());
    let mut bad = all.clone();
    assert!(!bad[0].risks.is_empty());
    bad[0].risks.clear();
    assert!(rule.select(&bad).unwrap_err().contains("risk/mode"));
    bad = all.clone();
    bad[0].strategy.online = true;
    assert!(rule.select(&bad).unwrap_err().contains("risk/mode"));
    bad = all.clone();
    bad.push(all[0].clone());
    assert!(rule.select(&bad).unwrap_err().contains("duplicate"));
    assert!(
        rule.select(&rule.prepared.logical)
            .unwrap_err()
            .contains("unrelated")
    );
    assert!(
        rule.unmet(&all[1..])
            .unwrap_err()
            .contains("lost a logical split part")
    );
    bad = rule.prepared.logical.clone();
    bad[0].risks.clear();
    assert!(rule.unmet(&bad).unwrap_err().contains("root risk/mode"));
    struct Stuck<'a>(&'a Rule<'a>);
    impl Expansion for Stuck<'_> {
        type Obligation = Need;
        fn rule(&self) -> &'static str {
            "stuck-rebuild"
        }
        fn unmet(&self, _: &[PlannedChange]) -> Result<Vec<Need>, String> {
            Ok(vec![Need::Split])
        }
        fn expand(&self, plan: &[PlannedChange], need: &Need) -> Result<Rewrite, String> {
            self.0.expand(plan, need)
        }
        fn order(&self, plan: &[PlannedChange]) -> Result<Vec<Order>, String> {
            self.0.order(plan)
        }
    }
    assert!(
        expand::expand(plan(), &Stuck(&rule), expand::FACTOR)
            .unwrap_err()
            .contains("left it unmet")
    );
}

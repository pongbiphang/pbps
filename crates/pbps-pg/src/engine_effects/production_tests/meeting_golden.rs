//! The full reference changes, order and spelling oracle, on the canonical route.
use super::*;
use crate::engine_effects::scenes_test::{KINDS, Motion, build, uid};
use pbps_dialect::PlanPayload;
use pbps_diff::{
    Side,
    order::deployment::{
        Sources,
        schedule::{Declarations, Facts, schedule},
    },
};
use sha2::{Digest, Sha256};
#[test]
fn dependent_meetings_keep_the_complete_canonical_changes_order_and_spellings() {
    use pbps_diff::order::catalog::{Dependent, Holds, Part};
    use pbps_model::{Module, ModuleId, ModuleKind};
    let expected: std::collections::BTreeMap<_, _> = include_str!("../meeting_plans.golden")
        .lines()
        .flat_map(|line| {
            let cells: Vec<_> = line.split('\t').collect();
            [160000, 180000]
                .into_iter()
                .enumerate()
                .map(move |(at, version)| (format!("{}/PG{version}", cells[0]), cells[at + 1]))
        })
        .collect();
    let mut out = String::new();
    let id: ModuleId = "app.f(integer)".parse().unwrap();
    let module = |v: &str| Module {
        kind: ModuleKind::Function,
        definition: format!(
            "CREATE FUNCTION app.f(a integer) RETURNS integer AS 'SELECT a + {v}' LANGUAGE sql;"
        ),
        description: None,
        options: Default::default(),
    };
    for kind in KINDS {
        for motion in [Motion::Attach, Motion::Detach, Motion::Drop] {
            for cut_final in [false, true] {
                for version in [160000, 180000] {
                    let mut s = build(kind, motion, cut_final);
                    let mut found = Vec::new();
                    for (schema, is_before) in [(&mut s.before, true), (&mut s.after, false)] {
                        schema
                            .modules
                            .insert(id.clone(), module(if is_before { "0" } else { "1" }));
                        for (tname, t) in schema.tables.iter_mut() {
                            if let Some(k) = t.columns.get_mut("k") {
                                k.value_source = ValueSource::Default("app.f(1)".into());
                                if is_before {
                                    found.push(Dependent {
                                        described: format!("default of {}.k", tname.shown()),
                                        holds: Holds::TablePart {
                                            table: tname.clone(),
                                            part: Part::Default("k".into()),
                                        },
                                    });
                                }
                            }
                        }
                    }
                    {
                        let old: TableName = "app.u".parse().unwrap();
                        let new: TableName = "app.u2".parse().unwrap();
                        let mk = |default: bool| {
                            let mut t = Table::default();
                            let mut n = Column::new("integer".parse().unwrap());
                            if default {
                                n.value_source = ValueSource::Default("app.f(1)".into());
                            }
                            t.columns.insert("n".into(), n);
                            t
                        };
                        s.before.tables.insert(old.clone(), mk(true));
                        s.after.tables.insert(new.clone(), mk(true));
                        s.base_ids.tables.insert(uid("t_cccccc"), old.clone());
                        s.ids.tables.insert(uid("t_cccccc"), new.clone());
                        s.base_ids.columns.insert(uid("c_gggggg"), old.column("n"));
                        s.ids.columns.insert(uid("c_gggggg"), new.column("n"));
                        found.push(Dependent {
                            described: "default of app.u.n".into(),
                            holds: Holds::TablePart {
                                table: old,
                                part: Part::Default("n".into()),
                            },
                        });
                    }
                    let label = format!("{kind:?}/{motion:?}/final={cut_final}/PG{version}");
                    let before = Side {
                        schema: &s.before,
                        ids: &s.base_ids,
                    };
                    let after = Side {
                        schema: &s.after,
                        ids: &s.ids,
                    };
                    let mut cat = pbps_model::operation::MembershipCatalog::default();
                    for tname in s.before.tables.keys() {
                        cat.columns.insert(
                            tname.column(if tname.name == "u" { "n" } else { "k" }),
                            pbps_model::operation::StoredColumn {
                                default: Some("app.f(1)".into()),
                                routines: [id.clone()].into(),
                                ..Default::default()
                            },
                        );
                    }
                    let pg = Postgres::new().with_server_version(version);
                    let plan = match pbps_diff::diff(before, after, &pg, &Default::default()) {
                        Ok(p) => p,
                        Err(e) => {
                            out.push_str(&format!("{label}\tDIFF-ERR {e:?}\n"));
                            continue;
                        }
                    };
                    let policies = s
                        .base_ids
                        .tables
                        .keys()
                        .chain(s.base_ids.columns.keys())
                        .cloned()
                        .map(|u| (u, Vec::new()))
                        .collect();
                    let dependents = [(id.clone(), found)].into();
                    let done = schedule(
                        plan,
                        &Declarations {
                            schema: &s.after,
                            sources: Sources {
                                before,
                                after: &s.ids,
                            },
                            module_deps: &Default::default(),
                        },
                        &Facts::default()
                            .with_module_dependents(&dependents)
                            .with_policy_dependents(&policies)
                            .with_membership_catalog(&cat),
                        &pg,
                    );
                    match done {
                        Ok(done) => {
                            let ordered = done
                                .changes
                                .ordered(pbps_model::PlanMode::Transactional, &pg);
                            let steps: Vec<String> = ordered
                                .changes()
                                .changes
                                .iter()
                                .map(|p| format!("{:?}", p.change)
                                    // These empty fields were added on master after c97cd4ad.
                                    // Nonempty options/USING remain visible and fail the oracle.
                                    .replace("options: ModuleOptions { schemabinding: false, indexes: {}, clustered: None }, ", "")
                                    .replace(", using: None", ""))
                                .collect();
                            let spellings: Vec<String> = ordered
                                .operations()
                                .iter()
                                .map(|o| format!("{:?}", o.origin().spelling()))
                                .collect();
                            out.push_str(&format!(
                                "{label}\tOK {} || {}\n",
                                steps.join(" | "),
                                spellings.join(",")
                            ));
                        }
                        Err(e) => out.push_str(&format!("{label}\tERR {e}\n")),
                    }
                }
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for line in out.lines() {
        let (label, payload) = line.split_once('\t').unwrap();
        assert!(seen.insert(label));
        let status = payload.split_once(' ').unwrap().0;
        let found = format!("{status} {:x}", Sha256::digest(payload.as_bytes()));
        assert_eq!(found, expected[label], "{label}: {payload}");
    }
    assert_eq!(seen.len(), expected.len());
}

//! One copy of each SQL scene; expectations are observations, never inferred from its name.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::OnceLock};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub helpers: String,
    fixtures: Vec<serde_json::Value>,
}
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    pub name: String,
    pub kind: String,
    pub major: i64,
    pub motion: String,
    pub profile: String,
    pub slice: String,
    pub setup: String,
    pub change: String,
    pub move_before: String,
    pub move_after: String,
    pub parent: String,
    pub intents: Vec<Vec<String>>,
    pub expected: Expected,
    #[serde(default = "two_orders")]
    pub orders: Vec<bool>,
}
fn two_orders() -> Vec<bool> {
    vec![true, false]
}
#[derive(Deserialize, Serialize, Debug, Clone)]
pub(super) struct Expected {
    pub native: Native,
    // Change first, then move first. A refusal must occur at this exact stage.
    pub product: [Product; 2],
}
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Native {
    Different,
    Accepted,
}
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Product {
    Apply,
    Identity,
    GoalPull,
    GoalRecord,
    BaselinePull,
    BaselineValidate,
    BaselineRecord,
    Adopt,
    LocalValidate,
    Plan,
    IdentityPlan,
}

impl Profile {
    pub fn key(&self) -> (&str, i64, &str, &str) {
        (&self.name, self.major, &self.motion, &self.profile)
    }
    pub fn outcome(&self, change_first: bool) -> Product {
        self.expected.product[usize::from(!change_first)]
    }
}
pub(super) fn manifest() -> &'static Manifest {
    static DATA: OnceLock<Manifest> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../../fixtures/partition-moves-2296.json")).unwrap()
    })
}
impl Manifest {
    pub fn profiles(&self) -> Vec<Profile> {
        self.fixtures
            .iter()
            .flat_map(|fixture| {
                let mut common = fixture.as_object().unwrap().clone();
                let variants = common.remove("profiles").unwrap();
                variants
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|variant| {
                        let mut expanded = common.clone();
                        expanded.extend(variant.as_object().unwrap().clone());
                        serde_json::from_value(serde_json::Value::Object(expanded)).unwrap()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}
pub(super) fn profiles() -> Vec<Profile> {
    validate(manifest());
    manifest().profiles()
}

fn payload_digest(data: &Manifest) -> String {
    let mut p = data.profiles();
    p.sort_by(|a, b| a.key().cmp(&b.key()));
    let mut hash = Sha256::new();
    for p in p {
        for field in [
            &p.name,
            &p.kind,
            &p.major.to_string(),
            &p.motion,
            &p.profile,
            &p.slice,
            &p.setup,
            &p.change,
            &p.move_before,
            &p.move_after,
            &data.helpers,
            &p.parent,
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
    }
    format!("{:x}", hash.finalize())
}
fn outcome_digest(data: &Manifest) -> String {
    let mut profiles = data.profiles();
    profiles.sort_by(|a, b| a.key().cmp(&b.key()));
    let mut hash = Sha256::new();
    for p in profiles {
        hash.update(format!(
            "{}/{}/{}/{}:{}\n",
            p.name,
            p.major,
            p.motion,
            p.profile,
            serde_json::to_string(&p.expected).unwrap()
        ));
    }
    format!("{:x}", hash.finalize())
}
fn validate(data: &Manifest) {
    let p = data.profiles();
    let keys: BTreeSet<_> = p.iter().map(Profile::key).collect();
    assert_eq!(data.fixtures.len(), 38);
    assert_eq!(p.len(), 72);
    assert_eq!(keys.len(), 72);
    assert_eq!(p.iter().filter(|p| p.slice == "COLUMN").count(), 40);
    assert_eq!(p.iter().filter(|p| p.slice == "PART").count(), 32);
    let appendix = include_str!("../../../../../docs/measurements/partition-moves-2296.md");
    let mut documented = BTreeSet::new();
    for row in appendix
        .lines()
        .filter(|l| l.starts_with("| ") && l.contains(" (`"))
    {
        let cells: Vec<_> = row.split('|').map(str::trim).collect();
        let name = cells[1].split(' ').next().unwrap();
        for (at, (major, motion)) in [
            (16, "attach"),
            (16, "detach"),
            (16, "drop"),
            (18, "attach"),
            (18, "detach"),
            (18, "drop"),
        ]
        .into_iter()
        .enumerate()
        {
            for profile in cells[at + 2].split(';') {
                if let Some((profile, "D/D")) = profile.trim().split_once(": ") {
                    documented.insert((name, major, motion, profile));
                }
            }
        }
    }
    assert_eq!(keys, documented);
    for p in p {
        assert_eq!(p.orders, [true, false]);
        assert!(!p.kind.is_empty());
        assert!(p.major == 16 || p.major == 18);
        assert!(p.slice == "COLUMN" || p.slice == "PART");
        assert_eq!(p.expected.native, Native::Different);
    }
    // Fingerprints were characterized from 5d75a41d, independently of this loader.
    assert_eq!(
        payload_digest(data),
        "ff6439db30e7aaa7702cc47c7050c2bece6ac3ba2f68c6ba89a1bb4aab75e496"
    );
    assert_eq!(
        outcome_digest(data),
        "966c02c399670db58107714b6df03c7d989ca7005ab8dce77122f187cd3aed4f"
    );
}
#[test]
fn every_measured_d_profile_has_exactly_one_live_coverage_entry() {
    validate(manifest());
}
#[test]
fn dropped_profiles_duplicate_tuples_and_corrupted_expectations_are_rejected() {
    for edit in 0..3 {
        let mut data: Manifest =
            serde_json::from_str(include_str!("../../fixtures/partition-moves-2296.json")).unwrap();
        let variants = data.fixtures[0]["profiles"].as_array_mut().unwrap();
        match edit {
            0 => {
                variants.pop().unwrap();
            }
            1 => {
                variants[1] = variants[0].clone();
            }
            2 => {
                variants[0]["expected"]["product"][0] = serde_json::json!("plan");
            }
            _ => unreachable!(),
        }
        assert!(
            std::panic::catch_unwind(|| validate(&data)).is_err(),
            "mutation {edit}"
        );
    }
}

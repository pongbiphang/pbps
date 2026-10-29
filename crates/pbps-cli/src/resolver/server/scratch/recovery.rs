//! Recovering what a killed scratch run left behind (#2410, DEC-2410.1).
//!
//! The engine-neutral half: an [`Adapter`] lists the resources other runs
//! left, says whether their owners are provably gone, and removes them; this
//! decides, removes and reports.
//!
//! - A run's resources go only when the adapter proves the run gone. One it
//!   cannot prove anything about is kept and reported: removing a live run's
//!   resources is the failure this prevents.
//! - It never fails the run that called it. What it cannot remove is a warning
//!   naming the resource, why, and the statement that removes it. The layout
//!   may still refuse the run for what is left, as before.
//! - It removes in the adapter's order and, after the first failure, keeps the
//!   rest of that run's resources: the run's record is listed last and must
//!   outlive what it names.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;

/// Resources of one kind, removed together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resource {
    pub kind: &'static str,
    pub names: Vec<String>,
}

impl Resource {
    pub fn one(kind: &'static str, name: impl Into<String>) -> Self {
        Self {
            kind,
            names: vec![name.into()],
        }
    }

    /// `database pbps_scratch_…`, or `3 roles (a, b, c)`.
    fn shown(&self) -> String {
        const SHOWN: usize = 3;
        match self.names.as_slice() {
            [one] => format!("{} {one}", self.kind),
            names => {
                let head = names
                    .iter()
                    .take(SHOWN)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = match names.len().saturating_sub(SHOWN) {
                    0 => String::new(),
                    more => format!(" and {more} more"),
                };
                format!("{} {}s ({head}{more})", names.len(), self.kind)
            }
        }
    }
}

/// What the adapter could establish about a run's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    /// Nothing the adapter saw proves either; the reason is shown.
    Unprovable(String),
}

/// A resource left in place, why, and the statement that removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub resource: Resource,
    pub why: String,
    pub statement: String,
}

/// What became of one dead or unprovable run's resources.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Group {
    pub token: String,
    pub removed: Vec<Resource>,
    pub kept: Vec<Kept>,
}

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub groups: Vec<Group>,
    /// Tokens whose owners are alive; their resources were not touched.
    pub live: Vec<String>,
    /// Why the sweep could not look, when it could not.
    pub unread: Option<String>,
}

/// Why a removal did not finish; a batch of roles can drop all but one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub why: String,
    /// The names still there; `None` when all are.
    pub left: Option<Vec<String>>,
}

impl From<String> for Failed {
    fn from(why: String) -> Self {
        Self { why, left: None }
    }
}

/// An engine's side of recovery.
#[allow(
    async_fn_in_trait,
    reason = "every adapter is used by value in one function; no caller needs a Send bound"
)]
pub trait Adapter {
    /// Every other run's resources by token, in removal order, record last.
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String>;

    /// Whether each of `tokens` has an owner.
    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String>;

    /// Removes `resource` of the run `token`, or says why it could not and
    /// what is left of it. Some resources are the run's only by what they
    /// carry (a SQL Server companion database is marked with its run), so the
    /// run is named.
    async fn remove(&mut self, token: &str, resource: &Resource) -> Result<(), Failed>;

    /// What an operator runs to remove `resource` by hand.
    fn statement(&self, resource: &Resource) -> String;
}

/// Removes the resources of every run the adapter proves dead.
pub async fn sweep(adapter: &mut impl Adapter) -> Report {
    let unread = |why: String| Report {
        unread: Some(why),
        ..Report::default()
    };
    let found = match adapter.leftovers().await {
        Ok(found) => found,
        Err(why) => return unread(why),
    };
    if found.is_empty() {
        return Report::default();
    }
    let tokens: Vec<String> = found.keys().cloned().collect();
    let mut lives = match adapter.liveness(&tokens).await {
        Ok(lives) => lives,
        Err(why) => return unread(why),
    };
    let mut report = Report::default();
    for (token, resources) in found {
        let mut group = Group {
            token: token.clone(),
            ..Group::default()
        };
        match lives
            .remove(&token)
            .unwrap_or_else(|| Liveness::Unprovable("the sessions were not read".into()))
        {
            Liveness::Alive => {
                report.live.push(token);
                continue;
            }
            Liveness::Unprovable(why) => keep_all(adapter, &mut group, resources, &why),
            Liveness::Dead => {
                let mut rest = resources.into_iter();
                while let Some(resource) = rest.next() {
                    match adapter.remove(&token, &resource).await {
                        Ok(()) => group.removed.push(resource),
                        Err(Failed { why, left }) => {
                            let left = left.unwrap_or_else(|| resource.names.clone());
                            let gone: Vec<String> = resource
                                .names
                                .iter()
                                .filter(|name| !left.contains(name))
                                .cloned()
                                .collect();
                            if !gone.is_empty() {
                                group.removed.push(Resource {
                                    kind: resource.kind,
                                    names: gone,
                                });
                            }
                            let stuck = Resource {
                                kind: resource.kind,
                                names: left,
                            };
                            let after = format!(
                                "not removed because {} could not be: {why}",
                                stuck.shown()
                            );
                            keep_all(adapter, &mut group, vec![stuck], &why);
                            keep_all(adapter, &mut group, rest.collect(), &after);
                            break;
                        }
                    }
                }
            }
        }
        report.groups.push(group);
    }
    report
}

fn keep_all(adapter: &impl Adapter, group: &mut Group, resources: Vec<Resource>, why: &str) {
    for resource in resources {
        let statement = adapter.statement(&resource);
        group.kept.push(Kept {
            resource,
            why: why.to_owned(),
            statement,
        });
    }
}

impl Report {
    /// The warning a person reads, or `None` when there is nothing to say.
    pub fn render(&self) -> Option<String> {
        let mut lines = Vec::new();
        if let Some(why) = &self.unread {
            lines.push(format!(
                "warning: scratch left by earlier runs could not be looked for: {why}"
            ));
        }
        for group in &self.groups {
            let short = &group.token[..group.token.len().min(8)];
            if !group.removed.is_empty() {
                lines.push(format!(
                    "warning: scratch left by run {short}…: confirmed removal of {}",
                    group
                        .removed
                        .iter()
                        .map(Resource::shown)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            for kept in &group.kept {
                lines.push(format!(
                    "warning: run {short}… left {}, which was not removed: {}\n  remove it with: {}",
                    kept.resource.shown(),
                    kept.why,
                    kept.statement
                ));
            }
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// What this process's sweeps found, for the command's JSON.
#[derive(Debug, Default)]
pub struct Collected {
    pub groups: Vec<Group>,
    pub unread: Vec<String>,
}

static COLLECTED: Mutex<Collected> = Mutex::new(Collected {
    groups: Vec::new(),
    unread: Vec::new(),
});

/// Prints the warning to stderr and keeps what it found for [`take`].
pub fn emit(report: &Report) {
    if let Some(text) = report.render() {
        eprintln!("{text}");
    }
    if let Ok(mut collected) = COLLECTED.lock() {
        collected.groups.extend(report.groups.iter().cloned());
        collected.unread.extend(report.unread.clone());
    }
}

/// What [`emit`] collected, once.
pub fn take() -> Collected {
    COLLECTED
        .lock()
        .map(|mut collected| std::mem::take(&mut *collected))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovery_report_confirms_absence_without_attributing_another_removers_work() {
        let report = Report {
            groups: vec![Group {
                token: "run".into(),
                removed: vec![Resource::one("docker container", "owned")],
                kept: Vec::new(),
            }],
            ..Default::default()
        };
        let text = report.render().unwrap();
        assert!(text.contains("confirmed removal of docker container owned"));
        assert!(!text.contains("its session is gone"));
        assert_eq!(Report::default().render(), None);
    }

    use super::*;

    /// A cluster in memory: what is there, who is alive, and what refuses.
    #[derive(Default)]
    struct Fake {
        found: BTreeMap<String, Vec<Resource>>,
        lives: BTreeMap<String, Liveness>,
        refuses: Vec<&'static str>,
        /// Kinds a removal half-finishes, and the names it leaves.
        leaves: BTreeMap<&'static str, Vec<String>>,
        read_fails: bool,
        removed: Vec<String>,
    }

    impl Adapter for Fake {
        async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
            if self.read_fails {
                return Err("the catalog is unreadable".into());
            }
            Ok(self.found.clone())
        }
        async fn liveness(&mut self, _: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
            Ok(self.lives.clone())
        }
        async fn remove(&mut self, _: &str, resource: &Resource) -> Result<(), Failed> {
            if self.refuses.contains(&resource.kind) {
                return Err("permission denied".to_owned().into());
            }
            if let Some(left) = self.leaves.get(resource.kind) {
                return Err(Failed {
                    why: "has dependents".into(),
                    left: Some(left.clone()),
                });
            }
            self.removed.push(resource.shown());
            Ok(())
        }
        fn statement(&self, resource: &Resource) -> String {
            format!("DROP {};", resource.shown())
        }
    }

    fn run(token: &str) -> (String, Vec<Resource>) {
        (
            token.into(),
            vec![
                Resource::one("database", format!("db_{token}")),
                Resource::one("login", format!("login_{token}")),
            ],
        )
    }

    fn fake(lives: &[(&str, Liveness)]) -> Fake {
        Fake {
            found: ["dead", "live", "blind"].map(run).into_iter().collect(),
            lives: lives
                .iter()
                .map(|(t, l)| ((*t).into(), l.clone()))
                .collect(),
            ..Fake::default()
        }
    }

    #[tokio::test]
    async fn only_a_run_proven_dead_loses_its_resources() {
        let mut cluster = fake(&[
            ("dead", Liveness::Dead),
            ("live", Liveness::Alive),
            ("blind", Liveness::Unprovable("no session list".into())),
        ]);
        let report = sweep(&mut cluster).await;
        assert_eq!(cluster.removed, ["database db_dead", "login login_dead"]);
        assert_eq!(report.live, ["live"]);
        let blind = report.groups.iter().find(|g| g.token == "blind").unwrap();
        assert!(blind.removed.is_empty());
        assert_eq!(blind.kept.len(), 2, "kept, each with its statement");
        assert_eq!(blind.kept[0].statement, "DROP database db_blind;");
        assert!(report.groups.iter().all(|g| g.token != "live"));
    }

    #[tokio::test]
    async fn a_token_the_adapter_said_nothing_about_is_kept() {
        let mut cluster = fake(&[("dead", Liveness::Dead)]);
        let report = sweep(&mut cluster).await;
        assert_eq!(cluster.removed.len(), 2, "only the dead run's");
        assert!(report.live.is_empty());
        let live = report.groups.iter().find(|g| g.token == "live").unwrap();
        assert!(live.removed.is_empty() && !live.kept.is_empty());
    }

    #[tokio::test]
    async fn a_failed_removal_keeps_the_rest_of_the_run_with_its_record_last() {
        let mut cluster = fake(&[("dead", Liveness::Dead)]);
        cluster.refuses = vec!["database"];
        let report = sweep(&mut cluster).await;
        assert!(
            cluster.removed.is_empty(),
            "the login outlives the database"
        );
        let dead = report.groups.iter().find(|g| g.token == "dead").unwrap();
        assert!(dead.removed.is_empty());
        assert_eq!(dead.kept[0].why, "permission denied");
        assert!(
            dead.kept[1].why.contains("could not be"),
            "{:?}",
            dead.kept[1]
        );
        let text = report.render().unwrap();
        assert!(
            text.contains("remove it with: DROP database db_dead;"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_half_finished_removal_names_only_what_is_left() {
        let mut cluster = Fake::default();
        cluster.found.insert(
            "dead".into(),
            vec![
                Resource {
                    kind: "role",
                    names: vec!["a".into(), "b".into(), "c".into()],
                },
                Resource::one("login", "l"),
            ],
        );
        cluster.lives.insert("dead".into(), Liveness::Dead);
        cluster.leaves.insert("role", vec!["b".into()]);
        let report = sweep(&mut cluster).await;
        let group = &report.groups[0];
        assert_eq!(
            group.removed,
            [Resource {
                kind: "role",
                names: vec!["a".into(), "c".into()]
            }]
        );
        assert_eq!(group.kept[0].resource.names, ["b"]);
        assert_eq!(
            group.kept[1].resource.names,
            ["l"],
            "the record outlives it"
        );
        assert!(cluster.removed.is_empty(), "the login was not touched");
    }

    #[tokio::test]
    async fn an_unreadable_catalog_is_said_and_removes_nothing() {
        let mut cluster = fake(&[("dead", Liveness::Dead)]);
        cluster.read_fails = true;
        let report = sweep(&mut cluster).await;
        assert!(cluster.removed.is_empty());
        assert!(
            report
                .render()
                .unwrap()
                .contains("could not be looked for: the catalog is unreadable")
        );
    }

    #[tokio::test]
    async fn nothing_to_find_is_silent() {
        let report = sweep(&mut Fake::default()).await;
        assert_eq!(report, Report::default());
        assert_eq!(report.render(), None);
    }

    #[test]
    fn many_roles_are_counted_and_a_few_are_named() {
        let roles = |n: usize| Resource {
            kind: "role",
            names: (0..n).map(|i| format!("r{i}")).collect(),
        };
        assert_eq!(roles(1).shown(), "role r0");
        assert_eq!(roles(3).shown(), "3 roles (r0, r1, r2)");
        assert_eq!(roles(5).shown(), "5 roles (r0, r1, r2 and 2 more)");
    }
}

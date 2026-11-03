//! The admission predicate: may this routine run as the deploying role?
//! (#2563, slice 2, DEC-2563.2.)
//!
//! Every place pbps lets user-written code run asks the same question of each
//! routine it reaches, and this module is the only place that answers it: a
//! domain's CHECK and DEFAULT, a referenced type's input path, a recorded
//! `USING` conversion, a row requirement, a rebuild's copy conversion and the
//! reference-data trigger guard. A routine is admitted when
//!
//! 1. it is built in, or provably built-in-only (tier 1, DEC-2563.1);
//! 2. the project manages it, which is to say the deployer created it, so rule 3
//!    holds of it;
//! 3. its owner is trusted: a superuser, the deploying role, a role that can
//!    `SET ROLE` to one of them, or a `trusted_roles` member (DEC-1663.1, the
//!    walk in [`crate::catalog::untrusted_role_walk`]);
//! 4. otherwise, `pin_type_routines` accepts it: the plan shows its complete
//!    source and pins it, and approving the plan accepts that code (DEC-2181.2).
//!
//! Whatever 2 to 4 admit must also set a fixed `search_path`: a role that can
//! create objects in a schema searched earlier replaces an unqualified call
//! inside a trusted function (measured on 16 and 18, #2563), and a function-level
//! `SET` is immune. And it must not be `SECURITY DEFINER` with an untrusted
//! owner, which would run the owner's rights for every caller.
//!
//! The requirement is for code that resolves names when it runs. A tier 1
//! function has none (its body is a parsed tree of OIDs), and a `c` or `internal`
//! routine, which only a superuser can create, binds its symbols at load time
//! and carries no `proconfig` in any extension on 16 and 18 (measured).

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pbps_model::RoleName;

use crate::user_types::Oid;

/// What a command has declared about the code it accepts, shared by every site
/// it vets. The sites that rule 4 admits record their routine in it, so that the
/// plan can show and pin exactly those.
#[derive(Debug, Default)]
pub struct Admission {
    trusted: Vec<RoleName>,
    pin: bool,
    /// Whether this site can show a routine's source in the plan. A type the
    /// plan itself creates cannot be read at plan time, so it cannot.
    can_review: bool,
    /// The routines a saved plan showed and pinned. When set, a site refuses a
    /// reviewed routine outside it, before it evaluates anything.
    approved: Option<BTreeSet<Oid>>,
    /// Plan time: code that only the review admits is not evaluated before the
    /// human has read it, so a site that would evaluate defers to apply.
    defer: bool,
    /// How many vets so far have relied on the review.
    reliances: AtomicUsize,
    reviewed: Mutex<BTreeSet<Oid>>,
}

impl Admission {
    #[must_use]
    pub fn new(trusted: impl IntoIterator<Item = RoleName>, pin: bool) -> Self {
        Self {
            trusted: trusted.into_iter().collect(),
            pin,
            can_review: true,
            approved: None,
            defer: false,
            reliances: AtomicUsize::new(0),
            reviewed: Mutex::default(),
        }
    }

    /// Plan's admission: a site still vets (so the plan can show and pin the
    /// routine) but must not evaluate an expression whose admission relies on
    /// the review (rule 4). Apply evaluates it, after the approval.
    #[must_use]
    pub fn deferring(mut self) -> Self {
        self.defer = true;
        self
    }

    /// A mark for [`Self::relied_since`], taken before a site vets.
    #[must_use]
    pub fn mark(&self) -> usize {
        self.reliances.load(Ordering::SeqCst)
    }

    /// Whether this admission defers and a vet since `mark` relied on the
    /// review: the site must then skip its evaluation.
    #[must_use]
    pub fn relied_since(&self, mark: usize) -> bool {
        self.defer && self.mark() > mark
    }

    /// Apply's admission: review admits only the routines the plan holds. A
    /// name that binds to another routine now (a new overload, say) is refused
    /// where it is vetted, which is before any expression is evaluated.
    #[must_use]
    pub fn approving(mut self, shown: impl IntoIterator<Item = Oid>) -> Self {
        self.approved = Some(shown.into_iter().collect());
        self
    }

    /// The first of `reviewed` that the plan did not show, if it holds a set.
    #[must_use]
    pub(crate) fn unapproved(&self, reviewed: &BTreeSet<Oid>) -> Option<Oid> {
        let approved = self.approved.as_ref()?;
        reviewed.difference(approved).next().copied()
    }

    /// A site where rule 4 does not apply, whatever `pin_type_routines` says.
    #[must_use]
    pub fn without_review(trusted: impl IntoIterator<Item = RoleName>) -> Self {
        Self {
            can_review: false,
            ..Self::new(trusted, false)
        }
    }

    /// Whether a refusal here should offer `pin_type_routines` as a remedy.
    #[must_use]
    pub fn offers_review(&self) -> bool {
        self.can_review && !self.pin
    }

    /// The roles a human declared trusted, over and above the engine's rule.
    #[must_use]
    pub fn trusted(&self) -> &[RoleName] {
        &self.trusted
    }

    /// Whether `pin_type_routines` is on.
    #[must_use]
    pub fn pin(&self) -> bool {
        self.pin
    }

    pub(crate) fn review(&self, oids: impl IntoIterator<Item = Oid>) {
        let mut reviewed = self
            .reviewed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut any = false;
        reviewed.extend(oids.into_iter().inspect(|_| any = true));
        if any {
            self.reliances.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The routines rule 4 admitted so far, by `pg_proc.oid`.
    #[must_use]
    pub fn reviewed(&self) -> BTreeSet<Oid> {
        self.reviewed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What the catalog says of a routine a role wrote.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Routine<'a> {
    /// Admitted by the built-in-only proof (tier 1).
    pub tier1: bool,
    pub secdef: bool,
    /// `proconfig` holds a `search_path`.
    pub fixed_search_path: bool,
    /// `pg_language.lanname`.
    pub language: &'a str,
    /// A role that is not trusted can act as the owner.
    pub untrusted_owner: bool,
}

/// Why a routine is refused. Each has its own remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cause {
    UntrustedOwner,
    SecurityDefiner,
    NoSearchPath,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Judgement {
    Admit,
    /// Rule 4: admitted, and the plan must show and pin it.
    Review,
    Refuse(Vec<Cause>),
}

/// The predicate. Pure: the sites read the facts and apply this.
pub(crate) fn judge(routine: &Routine<'_>, pin: bool) -> Judgement {
    let mut causes = Vec::new();
    if routine.untrusted_owner {
        if routine.secdef {
            causes.push(Cause::SecurityDefiner);
        }
        if !pin {
            causes.push(Cause::UntrustedOwner);
        }
    }
    if names_resolve_at_run_time(routine) && !routine.fixed_search_path {
        causes.push(Cause::NoSearchPath);
    }
    if !causes.is_empty() {
        Judgement::Refuse(causes)
    } else if routine.untrusted_owner {
        Judgement::Review
    } else {
        Judgement::Admit
    }
}

fn names_resolve_at_run_time(routine: &Routine<'_>) -> bool {
    !routine.tier1 && !matches!(routine.language, "c" | "internal")
}

/// The statement that fixes a function's name resolution. `pg_temp` goes last,
/// where the engine would otherwise search it first.
#[must_use]
pub fn search_path_fix(function: &str) -> String {
    format!("ALTER FUNCTION {function} SET search_path = pg_catalog, pg_temp")
}

/// The ways out of a refusal, in the words every site uses. `pin` is whether
/// this site can review a routine: a type the plan itself creates cannot be
/// read at plan time.
#[must_use]
pub fn remedies(pin: bool) -> String {
    format!(
        "To admit it: bring the routine into the project; or list its owner in \
         `trusted_roles` in `pbps.yml`{}",
        if pin {
            "; or set `pin_type_routines: true` to review its source and pin it"
        } else {
            ""
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plpgsql() -> Routine<'static> {
        Routine {
            tier1: false,
            secdef: false,
            fixed_search_path: true,
            language: "plpgsql",
            untrusted_owner: false,
        }
    }

    #[test]
    fn a_trusted_owner_with_a_fixed_search_path_is_admitted_without_review() {
        assert_eq!(judge(&plpgsql(), false), Judgement::Admit);
        assert_eq!(judge(&plpgsql(), true), Judgement::Admit);
    }

    #[test]
    fn a_routine_that_resolves_names_at_run_time_needs_a_fixed_search_path() {
        let open = Routine {
            fixed_search_path: false,
            ..plpgsql()
        };
        for pin in [false, true] {
            assert_eq!(
                judge(&open, pin),
                Judgement::Refuse(vec![Cause::NoSearchPath])
            );
        }
    }

    #[test]
    fn tier_one_and_compiled_routines_need_no_search_path() {
        for routine in [
            Routine {
                tier1: true,
                language: "sql",
                fixed_search_path: false,
                ..plpgsql()
            },
            Routine {
                language: "c",
                fixed_search_path: false,
                ..plpgsql()
            },
            Routine {
                language: "internal",
                fixed_search_path: false,
                ..plpgsql()
            },
        ] {
            assert_eq!(judge(&routine, false), Judgement::Admit);
        }
        // Any other language is run-time name resolution, including `sql`.
        let sql = Routine {
            language: "sql",
            fixed_search_path: false,
            ..plpgsql()
        };
        assert_eq!(
            judge(&sql, false),
            Judgement::Refuse(vec![Cause::NoSearchPath])
        );
    }

    #[test]
    fn an_untrusted_owner_is_admitted_only_by_a_review() {
        let untrusted = Routine {
            untrusted_owner: true,
            ..plpgsql()
        };
        assert_eq!(
            judge(&untrusted, false),
            Judgement::Refuse(vec![Cause::UntrustedOwner])
        );
        assert_eq!(judge(&untrusted, true), Judgement::Review);
    }

    #[test]
    fn security_definer_with_an_untrusted_owner_is_never_admitted() {
        let definer = Routine {
            untrusted_owner: true,
            secdef: true,
            ..plpgsql()
        };
        assert_eq!(
            judge(&definer, true),
            Judgement::Refuse(vec![Cause::SecurityDefiner])
        );
        assert_eq!(
            judge(&definer, false),
            Judgement::Refuse(vec![Cause::SecurityDefiner, Cause::UntrustedOwner])
        );
        // A trusted owner's definer runs as a trusted role anyway.
        let trusted = Routine {
            untrusted_owner: false,
            ..definer
        };
        assert_eq!(judge(&trusted, false), Judgement::Admit);
    }

    #[test]
    fn a_refusal_names_every_remedy_its_site_has() {
        assert!(remedies(true).contains("pin_type_routines"));
        assert!(!remedies(false).contains("pin_type_routines"));
        assert!(remedies(false).contains("trusted_roles"));
        assert_eq!(
            search_path_fix("app.f(integer)"),
            "ALTER FUNCTION app.f(integer) SET search_path = pg_catalog, pg_temp"
        );
    }

    #[test]
    fn a_plan_holding_a_set_refuses_a_reviewed_routine_outside_it() {
        let open = Admission::new([], true);
        assert_eq!(open.unapproved(&BTreeSet::from([9])), None);
        let approving = Admission::new([], true).approving([1, 2]);
        assert_eq!(approving.unapproved(&BTreeSet::from([2])), None);
        assert_eq!(approving.unapproved(&BTreeSet::from([2, 9])), Some(9));
        assert_eq!(approving.unapproved(&BTreeSet::new()), None);
    }

    #[test]
    fn only_a_deferring_admission_skips_after_a_vet_that_relied_on_the_review() {
        for (admission, defers) in [
            (Admission::new([], true).deferring(), true),
            (Admission::new([], true), false),
        ] {
            let mark = admission.mark();
            admission.review([]);
            assert!(!admission.relied_since(mark));
            admission.review([7]);
            assert_eq!(admission.relied_since(mark), defers);
            // The same routine reviewed again at another site still counts.
            let mark = admission.mark();
            admission.review([7]);
            assert_eq!(admission.relied_since(mark), defers);
        }
    }

    #[test]
    fn reviewed_roots_accumulate_across_sites() {
        let admission = Admission::new([], true);
        admission.review([3, 1]);
        admission.review([1, 2]);
        assert_eq!(admission.reviewed(), BTreeSet::from([1, 2, 3]));
    }
}

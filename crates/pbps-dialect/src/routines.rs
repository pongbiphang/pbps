//! Shared routine admission and catalog evidence (#2798).

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pbps_model::RoleName;

/// The one admission rule of every site that runs code the plan holds
/// (DEC-2563.2), shared by the positions that vet.
///
/// What a command has declared about the code it accepts, shared by every site
/// it vets. The sites that rule 4 admits record their routine in it, so that the
/// plan can show and pin exactly those.
#[derive(Debug, Default)]
pub struct RoutineAdmission {
    trusted: Vec<RoleName>,
    pin: bool,
    /// Whether this site can show a routine's source in the plan. A type the
    /// plan itself creates cannot be read at plan time, so it cannot.
    can_review: bool,
    /// The routines a saved plan showed and pinned. When set, a site refuses a
    /// reviewed routine outside it, before it evaluates anything.
    approved: Option<BTreeSet<i64>>,
    /// Plan time: code that only the review admits is not evaluated before the
    /// human has read it, so a site that would evaluate defers to apply.
    defer: bool,
    /// How many vets so far have relied on the review.
    reliances: AtomicUsize,
    reviewed: Mutex<BTreeSet<i64>>,
}

impl RoutineAdmission {
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
    pub fn approving(mut self, shown: impl IntoIterator<Item = i64>) -> Self {
        self.approved = Some(shown.into_iter().collect());
        self
    }

    /// The first of `reviewed` that the plan did not show, if it holds a set.
    #[must_use]
    pub fn unapproved(&self, reviewed: &BTreeSet<i64>) -> Option<i64> {
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

    pub fn review(&self, oids: impl IntoIterator<Item = i64>) {
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
    pub fn reviewed(&self) -> BTreeSet<i64> {
        self.reviewed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// One routine in the pin set, as the catalog holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    /// `pg_proc.oid`, which keys the pin: an approved rename of a schema or of
    /// a type in the argument list changes the spelling of the signature but
    /// not the OID (DEC-2333.1).
    pub oid: i64,
    /// The schema's name when this was read, for messages only.
    pub schema: String,
    pub name: String,
    /// The argument types in the spelling the pull gives a routine's identity
    /// (`format_type` under the empty `search_path`), so that the caller can
    /// tell a managed routine from an unmanaged one.
    pub args: Vec<String>,
    /// The canonical JSON this routine's pin is made over.
    pub input: String,
}

/// What one read returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    /// `server_version_num`. A plan and an apply on different releases would
    /// render the same routine's row differently, so the caller compares this
    /// before any digest.
    pub server_version: String,
    pub routines: Vec<Routine>,
}

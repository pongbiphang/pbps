//! SQL Server's evidence reader and catalog vocabulary (DEC-2143.2).
use super::{authorization, capture, compatibility};
use pbps_model::resolver::{
    AuthorizationReproduction, Binding, EvidenceError, ObjectIdentity, ResolverEvidence,
    ResolverRuntime, Surface,
};
use std::collections::BTreeSet;

/// Pure reader support for the evidence this SQL Server adapter can repeat.
/// Placement keys are compared with the plan's declared dependencies by the
/// artifact's admission (`Dialect::validate_plan_analysis`), which has the plan.
pub fn validate_evidence(evidence: &ResolverEvidence) -> Result<(), EvidenceError> {
    let q = evidence.qualification();
    // The compared facts move with the rule's identifier (DEC-2012.1): another
    // identifier, another engine's included, is refused by name.
    if q.rule.as_str() != compatibility::REPORTED_RULE {
        return Err(EvidenceError::Rule(q.rule.clone()));
    }
    let ResolverRuntime::Scratch { same_instance, .. } = &q.runtime;
    // Whether the scratch shared the target's instance decides which checks
    // the run could make; a plan that does not say was not sealed by this
    // adapter's producer.
    if same_instance.is_not_applicable() {
        return Err(EvidenceError::Incomplete);
    }
    let measured = |manifest: &pbps_model::resolver::InputManifest| {
        manifest.adapter() == capture::RULE && MEASURED_MAJORS.contains(&manifest.engine_major())
    };
    if !measured(evidence.before())
        || !measured(evidence.after())
        || evidence.authorization().rule != authorization::RULE
        || q.authorization != AuthorizationReproduction::Reproduced
        || q.target_cluster.is_empty()
    {
        return Err(EvidenceError::Version);
    }
    Ok(())
}

/// The product majors the capture and the authorization reads were measured on.
const MEASURED_MAJORS: std::ops::RangeInclusive<u32> = 17..=17;

/// A computed expression is owned as a Default-compatible surface, but its
/// binding lives on sys.columns; a DEFAULT constraint lives on sys.objects.
pub fn binding_classes(surface: &Surface) -> Option<&'static [&'static str]> {
    match surface {
        Surface::Default(_) => Some(&[
            capture::class::OBJECT,
            capture::class::COLUMN,
            "pbps-unnamed-DEFAULT",
        ]),
        Surface::Check { .. } => Some(&[capture::class::OBJECT, "pbps-unnamed-CHECK"]),
        Surface::Module(_) => Some(&[capture::class::OBJECT]),
        // SQL Server has no expression-index binding surface. Its filtered
        // predicate binds only the table's columns and has no external callee.
        Surface::Index { .. }
        | Surface::Namespace(_)
        | Surface::Table(_)
        | Surface::Column(_)
        | Surface::Type(_) => None,
    }
}

/// SQL Server exposes the dependency directly in the captured object's
/// bindings; it has no PostgreSQL-style pg_depend identity to search for.
pub fn dependent_object(
    object: &ObjectIdentity,
    bindings: &[Binding],
    reached: &BTreeSet<ObjectIdentity>,
) -> Option<ObjectIdentity> {
    bindings
        .iter()
        .any(|binding| reached.contains(&binding.target))
        .then(|| object.clone())
}

#[cfg(test)]
mod tests;

//! The differences between two declarations of one type.

use super::*;

/// Lifecycle refusals for the attributes no executable change covers, shared
/// with connected prerequisite admission so a missing declared type is named
/// before its column's reference gate. `alterable` leaves out what a retained
/// domain changes in place: its NOT NULL, default and checks (DEC-1635.8).
pub fn type_differences(
    base: &Schema,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    alterable: bool,
) -> Vec<DiffError> {
    let mut errs = Vec::new();
    // Whole-object changes are handled by uid; retained definitions still
    // refuse the attributes no change covers.
    let names: BTreeSet<_> = base.types.keys().chain(declared.types.keys()).collect();
    for name in names {
        let reason = match (base.types.get(name), declared.types.get(name)) {
            (None, Some(_)) | (Some(_), None) => None,
            (Some(old), Some(new)) => {
                let normalize = |definition: &pbps_model::ManagedType| {
                    let mut definition = definition.clone();
                    if let pbps_model::ManagedType::Domain { base, .. } = &mut definition {
                        *base = dialect
                            .normalize_type(base)
                            .unwrap_or_else(|_| base.clone());
                    }
                    definition
                };
                let mut fields = normalize(old).differences(&normalize(new));
                if alterable {
                    fields.retain(|field| {
                        !matches!(
                            *field,
                            "domain NOT NULL" | "domain default" | "domain checks"
                        )
                    });
                }
                (!fields.is_empty()).then(|| {
                    let why = match new {
                        pbps_model::ManagedType::Enum { .. } => {
                            "an enum's labels change only through its recorded uid (#1635 slice 3, DEC-1635.6, DEC-1635.7)"
                        }
                        pbps_model::ManagedType::Domain { .. } => {
                            "a domain's base type and collation change by rebuilding it (#1635 slice 4b), and its NOT NULL, default and checks only through its recorded uid (DEC-1635.8)"
                        }
                    };
                    format!("{} changed; {why}", fields.join(", "))
                })
            }
            (None, None) => None,
        };
        if let Some(reason) = reason {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason,
            });
        }
    }

    errs
}

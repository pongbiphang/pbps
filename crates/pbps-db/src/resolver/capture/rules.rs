//! Coverage checks over engine-owned catalog rules (DEC-2162.1).
//!
//! This module neither reads nor translates properties. Each adapter decides
//! which fields may be compared as reported and which require an explicit rule.

/// Evidence that an engine has accounted for a catalog item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Compared, translated or handled by a separate typed read.
    Classified,
    /// An exception, such as a skip or a refusal, with its engine-owned reason.
    WithReason(&'static str),
}

/// Return unknown items and exceptions whose justification is empty. The same
/// check accepts live descriptors, frozen layouts and catalog disposition lists;
/// ordering, layout equality and read consistency remain the adapter's policy.
pub fn unclassified<T>(
    items: impl IntoIterator<Item = T>,
    mut classify: impl FnMut(&T) -> Option<Coverage>,
) -> impl Iterator<Item = T> {
    items.into_iter().filter(move |item| match classify(item) {
        Some(Coverage::Classified) => false,
        Some(Coverage::WithReason(reason)) => reason.trim().is_empty(),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_references_are_reported_but_unnamed_scalars_can_be_compared() {
        let columns = [
            ("objects", "future_id", "int"),
            ("objects", "label", "text"),
        ];
        let unknown: Vec<_> = unclassified(columns, |(_, name, _)| {
            (!name.ends_with("_id")).then_some(Coverage::Classified)
        })
        .collect();
        assert_eq!(unknown, vec![("objects", "future_id", "int")]);
    }

    #[test]
    fn a_catalog_without_a_disposition_is_reported() {
        let unknown: Vec<_> = unclassified(["objects", "future"], |name| {
            (*name == "objects").then_some(Coverage::Classified)
        })
        .collect();
        assert_eq!(unknown, vec!["future"]);
    }

    #[test]
    fn an_exception_requires_a_nonempty_reason() {
        let reasons = ["", " \t\n", "the row key is represented by its identity"];
        let invalid: Vec<_> =
            unclassified(reasons, |reason| Some(Coverage::WithReason(reason))).collect();
        assert_eq!(invalid, vec!["", " \t\n"]);
    }
}

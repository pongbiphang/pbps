//! What PostgreSQL's engine admits for an index key under a method's policy
//! (DEC-2448.1).
//!
//! The capability data is the model's ([`pbps_model::storage::MethodPolicy`]):
//! the loader and the differ read it without a dialect. What is PostgreSQL's
//! own is the reading of it against a key: which column types a method is held
//! over and which operator classes are the engine's own. `validate` (a
//! declaration) and `introspect` (a pull) both ask here, so a method added to
//! the table is admitted by both or by neither, and the two cannot drift.

use pbps_model::storage::MethodPolicy;
use pbps_model::{Column, OpClassRef};

/// Why a key's operator class is not one this model holds for the method.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClassProblem {
    /// The method is held under each type's default class only.
    DefaultOnly,
    /// The method's default class, spelled out: a declaration leaves it out
    /// so one index has one spelling.
    DefaultSpelledOut,
    /// A class of the engine's own that this model does not hold here.
    NotHeld,
    /// A class a schema owns. Only a provider can vouch for what it is, and
    /// none does yet (#2447 PR F), so it is refused rather than guessed.
    Outside,
}

/// `None` where `class` is a class this model holds for `policy`'s method.
pub(crate) fn class_problem(policy: &MethodPolicy, class: &OpClassRef) -> Option<ClassProblem> {
    let Some(name) = class.as_bare() else {
        return Some(ClassProblem::Outside);
    };
    if policy.classes.is_empty() && policy.default_class.is_none() {
        return Some(ClassProblem::DefaultOnly);
    }
    if policy.default_class == Some(name) {
        return Some(ClassProblem::DefaultSpelledOut);
    }
    (!policy.classes.contains(&name)).then_some(ClassProblem::NotHeld)
}

/// Whether `column` is of the one type the method is held over, where it has
/// one. Not an array of it, whose base is the element's (#1525 review).
pub(crate) fn holds_key_type(policy: &MethodPolicy, column: &Column) -> bool {
    policy.key_type.is_none_or(|base| {
        crate::types::normalize(&column.ty).is_ok_and(|t| t.base == base && !t.is_array())
    })
}

/// How a message names the one type a method is held over, e.g. "`jsonb`
/// columns".
pub(crate) fn key_scope(policy: &MethodPolicy) -> String {
    match policy.key_type {
        Some(base) => format!("`{base}` columns"),
        None => "columns".to_owned(),
    }
}

/// The classes a message offers: the default (by leaving the class out) and
/// the held ones, as the user writes them.
pub(crate) fn offered(policy: &MethodPolicy) -> String {
    let mut words: Vec<String> = policy
        .default_class
        .iter()
        .map(|d| format!("`{d}`"))
        .collect();
    words.extend(policy.classes.iter().map(|c| format!("`pg_catalog.{c}`")));
    words.join(" or ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::IndexMethod;

    fn class(text: &str) -> OpClassRef {
        text.parse().unwrap()
    }

    /// A B-tree key is held under its type's default class only: every class
    /// named for one is a problem, the engine's own or a schema's (DEC-1169.1).
    #[test]
    fn a_btree_key_holds_no_named_class() {
        let policy = IndexMethod::Btree.policy();
        for text in ["text_pattern_ops", "pg_catalog.text_pattern_ops"] {
            assert_eq!(
                class_problem(policy, &class(text)),
                Some(ClassProblem::DefaultOnly),
                "{text}"
            );
        }
        assert_eq!(
            class_problem(policy, &class("ext.int4_ops")),
            Some(ClassProblem::Outside)
        );
    }

    /// GIN over `jsonb` holds `jsonb_path_ops`, and its default only by being
    /// left out; a class of the same name in another schema is another class.
    #[test]
    fn a_gin_key_holds_its_path_class_and_not_its_default_or_a_namesake() {
        let policy = IndexMethod::Gin.policy();
        assert_eq!(class_problem(policy, &class("jsonb_path_ops")), None);
        assert_eq!(
            class_problem(policy, &class("pg_catalog.jsonb_path_ops")),
            None
        );
        assert_eq!(
            class_problem(policy, &class("jsonb_ops")),
            Some(ClassProblem::DefaultSpelledOut)
        );
        assert_eq!(
            class_problem(policy, &class("array_ops")),
            Some(ClassProblem::NotHeld)
        );
        assert_eq!(
            class_problem(policy, &class("app.jsonb_path_ops")),
            Some(ClassProblem::Outside)
        );
    }

    /// The key type is the method's, and an array of it is not it.
    #[test]
    fn the_key_type_is_the_policys_and_never_an_array_of_it() {
        let column = |ty: &str| Column::new(ty.parse().unwrap());
        let gin = IndexMethod::Gin.policy();
        assert!(holds_key_type(gin, &column("jsonb")));
        assert!(!holds_key_type(gin, &column("jsonb[]")));
        assert!(!holds_key_type(gin, &column("text")));
        assert!(holds_key_type(IndexMethod::Btree.policy(), &column("text")));
    }

    #[test]
    fn messages_name_what_the_policy_offers() {
        assert_eq!(
            offered(IndexMethod::Gin.policy()),
            "`jsonb_ops` or `pg_catalog.jsonb_path_ops`"
        );
        assert_eq!(key_scope(IndexMethod::Gin.policy()), "`jsonb` columns");
        assert_eq!(key_scope(IndexMethod::Btree.policy()), "columns");
    }
}

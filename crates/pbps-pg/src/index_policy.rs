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
    /// A class a schema owns, under a method that holds none a package
    /// provides (BRIN: no package does, DEC-2447.14).
    Outside,
}

/// `None` where `class` is a class this model holds for `policy`'s method.
pub(crate) fn class_problem(policy: &MethodPolicy, class: &OpClassRef) -> Option<ClassProblem> {
    let Some(name) = class.as_bare() else {
        // A qualified class is held when a managed package provides it; whether
        // one does is the plan's pin to prove (DEC-2447.14), not the policy's.
        return (!policy.provider_classes).then_some(ClassProblem::Outside);
    };
    if policy.classes.is_empty() && policy.default_class.is_none() {
        return Some(ClassProblem::DefaultOnly);
    }
    if policy.default_class == Some(name) {
        return Some(ClassProblem::DefaultSpelledOut);
    }
    (!policy.classes.contains(&name)).then_some(ClassProblem::NotHeld)
}

/// Whether a key names a class a managed package provides, given the schema of
/// the class (`None` for the engine's own bare class). The one rule behind both
/// the declaration and the pull (DEC-2447.14): the class, pinned against the
/// package, is what says the key is one the engine takes (`gin_trgm_ops` over
/// `text`, or over an expression), so the method's key-type and expression gates,
/// which are for the engine's own classes, do not apply to it.
pub(crate) fn provider_class(policy: &MethodPolicy, class_schema: Option<&str>) -> bool {
    policy.provider_classes && class_schema.is_some()
}

/// [`provider_class`] for a declared key.
pub(crate) fn provider_key(policy: &MethodPolicy, key: &pbps_model::IndexColumn) -> bool {
    provider_class(policy, key.opclass.as_ref().and_then(|c| c.schema()))
}

/// [`provider_class`] for a pulled key, whose class is the catalog's `schema.name`
/// text (empty for the default, `pg_catalog.` for the engine's own).
pub(crate) fn provider_class_text(policy: &MethodPolicy, class: &str) -> bool {
    provider_class(
        policy,
        (!class.is_empty() && !class.starts_with("pg_catalog.")).then_some(class),
    )
}

/// Whether an expression key is held: the method holds expression keys, or the key
/// names a class a package provides. Asked by the declaration and by the pull, so
/// a declaration this model accepts is one the pull reads back (a GIN expression
/// under `gin_trgm_ops` is, a GIN expression under no class is not).
pub(crate) fn holds_expression_key(policy: &MethodPolicy, provider: bool) -> bool {
    policy.expression_keys || provider
}

/// Whether `class` is one of the engine's own that a later part of the same
/// issue brings to `policy`'s method (BRIN's minmax-multi and Bloom, DEC-2449.1):
/// refused by name as arriving, not as unknown.
pub(crate) fn later_class(policy: &MethodPolicy, class: &OpClassRef) -> bool {
    policy.method == pbps_model::IndexMethod::Brin
        && class
            .as_bare()
            .is_some_and(|n| n.ends_with("_minmax_multi_ops") || n.ends_with("_bloom_ops"))
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
        // A provider's class is the pin's to prove under a method that holds one.
        assert_eq!(class_problem(policy, &class("ext.int4_ops")), None);
        assert_eq!(
            class_problem(IndexMethod::Brin.policy(), &class("ext.int4_ops")),
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
        // A namesake of an engine class in another schema is a provider's class
        // to be proved, never the engine's.
        assert_eq!(class_problem(policy, &class("app.jsonb_path_ops")), None);
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

    /// One rule for the declaration and the pull: a key naming a package's class is
    /// held where the method would not hold it, an expression key included; a key
    /// naming the engine's class, or none, is held as the method holds it. BRIN
    /// holds no package class, so nothing lifts its gates.
    #[test]
    fn a_package_class_lifts_the_expression_gate_for_the_declaration_and_the_pull_alike() {
        let gin = IndexMethod::Gin.policy();
        let brin = IndexMethod::Brin.policy();
        assert!(!holds_expression_key(gin, false));
        assert!(holds_expression_key(gin, true));
        assert!(holds_expression_key(IndexMethod::Btree.policy(), false));
        // The declared and the pulled spelling of the same class agree.
        let key = |class: Option<&str>| pbps_model::IndexColumn {
            key: pbps_model::IndexKey::Expression("lower(body)".into()),
            descending: false,
            opclass: class.map(|c| c.parse().unwrap()),
            collation: None,
        };
        for (declared, pulled) in [
            (Some("ext.gin_trgm_ops"), "ext.gin_trgm_ops"),
            (
                Some("pg_catalog.jsonb_path_ops"),
                "pg_catalog.jsonb_path_ops",
            ),
            (Some("jsonb_path_ops"), "pg_catalog.jsonb_path_ops"),
            (None, ""),
        ] {
            assert_eq!(
                provider_key(gin, &key(declared)),
                provider_class_text(gin, pulled),
                "{declared:?} / {pulled:?}"
            );
        }
        assert!(provider_class_text(gin, "ext.gin_trgm_ops"));
        // Negatives: the engine's class, the default, and a method with none.
        assert!(!provider_class_text(gin, "pg_catalog.jsonb_path_ops"));
        assert!(!provider_class_text(gin, ""));
        assert!(!provider_class_text(brin, "ext.int4_ops"));
        assert!(!provider_key(brin, &key(Some("ext.int4_ops"))));
        assert!(!holds_expression_key(
            gin,
            provider_class_text(gin, "pg_catalog.jsonb_path_ops")
        ));
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

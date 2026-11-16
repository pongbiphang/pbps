//! What PostgreSQL's engine admits for an index key under a method's policy
//! (DEC-2448.1).
//!
//! The capability data is the model's ([`pbps_model::storage::MethodPolicy`]):
//! the loader and the differ read it without a dialect. What is PostgreSQL's
//! own is the reading of it against a key: which column types a method is held
//! over and which operator classes are the engine's own. `validate` (a
//! declaration) and `introspect` (a pull) both ask here, so a method added to
//! the table is admitted by both or by neither, and the two cannot drift.

use pbps_model::storage::{KeyType, MethodPolicy};
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

/// `None` where `class` is a class this model holds for `policy`'s method over
/// a key of type `key` (the entry of the method's key types the column is, from
/// [`key_type_of`]). A method held by key type reads a class against that type:
/// `jsonb_path_ops` is not a class for a `tsvector`. A key whose type the method
/// does not hold has no class to judge: the type is what is refused, once
/// (DEC-1984.1).
pub(crate) fn class_problem(
    policy: &MethodPolicy,
    class: &OpClassRef,
    key: Option<&KeyType>,
) -> Option<ClassProblem> {
    let Some(name) = class.as_bare() else {
        // A qualified class is held when a managed package provides it; whether
        // one does is the plan's pin to prove (DEC-2447.14), not the policy's.
        return (!policy.provider_classes).then_some(ClassProblem::Outside);
    };
    let (default, classes) = match (policy.key_types.is_empty(), key) {
        (true, _) => (None, policy.classes),
        (false, Some(key)) => (Some(key.default_class), key.classes),
        (false, None) => return None,
    };
    if classes.is_empty() && default.is_none() {
        return Some(ClassProblem::DefaultOnly);
    }
    if default == Some(name) {
        return Some(ClassProblem::DefaultSpelledOut);
    }
    (!classes.contains(&name)).then_some(ClassProblem::NotHeld)
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

/// The key type of `policy` that `column` is: its normalised base type, and not
/// an array of it, whose base is the element's (#1525 review). `None` where the
/// method is not held by key type, or holds none of this column's.
pub(crate) fn key_type_of(policy: &MethodPolicy, column: &Column) -> Option<&'static KeyType> {
    let ty = crate::types::normalize(&column.ty).ok()?;
    (!ty.is_array())
        .then(|| policy.key_types.iter().find(|k| k.base == ty.base))
        .flatten()
}

/// Whether `column` is of a type the method is held over, where it is held by
/// type at all.
pub(crate) fn holds_key_type(policy: &MethodPolicy, column: &Column) -> bool {
    policy.key_types.is_empty() || key_type_of(policy, column).is_some()
}

/// How a message names the types a method is held over, e.g. "`jsonb` or
/// `tsvector` columns".
pub(crate) fn key_scope(policy: &MethodPolicy) -> String {
    if policy.key_types.is_empty() {
        return "columns".to_owned();
    }
    let types: Vec<String> = policy
        .key_types
        .iter()
        .map(|k| format!("`{}`", k.base))
        .collect();
    format!("{} columns", types.join(" or "))
}

/// The classes a message offers for a key of type `key` (every key type's, where
/// it is not known): the default (by leaving the class out) and the held ones,
/// as the user writes them.
pub(crate) fn offered(policy: &MethodPolicy, key: Option<&KeyType>) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut add = |default: Option<&str>, classes: &[&str]| {
        for word in default
            .map(|d| format!("`{d}`"))
            .into_iter()
            .chain(classes.iter().map(|c| format!("`pg_catalog.{c}`")))
        {
            if !words.contains(&word) {
                words.push(word);
            }
        }
    };
    match (policy.key_types.is_empty(), key) {
        (true, _) => add(None, policy.classes),
        (false, Some(key)) => add(Some(key.default_class), key.classes),
        (false, None) => policy
            .key_types
            .iter()
            .for_each(|k| add(Some(k.default_class), k.classes)),
    }
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
                class_problem(policy, &class(text), None),
                Some(ClassProblem::DefaultOnly),
                "{text}"
            );
        }
        // A provider's class is the pin's to prove under a method that holds one.
        assert_eq!(class_problem(policy, &class("ext.int4_ops"), None), None);
        assert_eq!(
            class_problem(IndexMethod::Brin.policy(), &class("ext.int4_ops"), None),
            Some(ClassProblem::Outside)
        );
    }

    fn column(ty: &str) -> Column {
        Column::new(ty.parse().unwrap())
    }

    /// GIN over `jsonb` holds `jsonb_path_ops`, and its default only by being
    /// left out; a class of the same name in another schema is another class.
    #[test]
    fn a_gin_key_holds_its_path_class_and_not_its_default_or_a_namesake() {
        let policy = IndexMethod::Gin.policy();
        let jsonb = key_type_of(policy, &column("jsonb"));
        assert!(jsonb.is_some());
        assert_eq!(class_problem(policy, &class("jsonb_path_ops"), jsonb), None);
        assert_eq!(
            class_problem(policy, &class("pg_catalog.jsonb_path_ops"), jsonb),
            None
        );
        assert_eq!(
            class_problem(policy, &class("jsonb_ops"), jsonb),
            Some(ClassProblem::DefaultSpelledOut)
        );
        assert_eq!(
            class_problem(policy, &class("array_ops"), jsonb),
            Some(ClassProblem::NotHeld)
        );
        // A namesake of an engine class in another schema is a provider's class
        // to be proved, never the engine's.
        assert_eq!(
            class_problem(policy, &class("app.jsonb_path_ops"), jsonb),
            None
        );
    }

    /// GIN over `tsvector` holds its one class, `tsvector_ops`, as the default
    /// only: naming it is refused as `jsonb_ops` is, and another type's class is
    /// not one of its classes (DEC-1984.1).
    #[test]
    fn a_gin_tsvector_key_holds_its_default_only_and_no_other_types_class() {
        let policy = IndexMethod::Gin.policy();
        let tsvector = key_type_of(policy, &column("tsvector"));
        assert_eq!(tsvector.map(|k| k.default_class), Some("tsvector_ops"));
        assert_eq!(
            class_problem(policy, &class("tsvector_ops"), tsvector),
            Some(ClassProblem::DefaultSpelledOut)
        );
        assert_eq!(
            class_problem(policy, &class("pg_catalog.tsvector_ops"), tsvector),
            Some(ClassProblem::DefaultSpelledOut)
        );
        // Negatives: `jsonb_path_ops` is jsonb's, `jsonb_ops` and `tsvector_ops`
        // each the other's default.
        for other in ["jsonb_path_ops", "jsonb_ops", "array_ops"] {
            assert_eq!(
                class_problem(policy, &class(other), tsvector),
                Some(ClassProblem::NotHeld),
                "{other}"
            );
        }
        let jsonb = key_type_of(policy, &column("jsonb"));
        assert_eq!(
            class_problem(policy, &class("tsvector_ops"), jsonb),
            Some(ClassProblem::NotHeld)
        );
        // A type the method does not hold has no class to judge: the type is
        // what is refused.
        assert_eq!(class_problem(policy, &class("tsvector_ops"), None), None);
        // A schema's class is the pin's to prove, as for jsonb.
        assert_eq!(
            class_problem(policy, &class("app.tsvector_ops"), tsvector),
            None
        );
    }

    /// The key types are the method's, and an array of one is not it.
    #[test]
    fn the_key_types_are_the_policys_and_never_an_array_of_one() {
        let gin = IndexMethod::Gin.policy();
        assert!(holds_key_type(gin, &column("jsonb")));
        assert!(holds_key_type(gin, &column("tsvector")));
        assert!(!holds_key_type(gin, &column("jsonb[]")));
        assert!(!holds_key_type(gin, &column("tsvector[]")));
        assert!(!holds_key_type(gin, &column("text")));
        assert!(!holds_key_type(gin, &column("tsquery")));
        assert!(!holds_key_type(gin, &column("inet")));
        assert!(holds_key_type(IndexMethod::Btree.policy(), &column("text")));
        assert!(key_type_of(IndexMethod::Btree.policy(), &column("text")).is_none());
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
        let gin = IndexMethod::Gin.policy();
        let of = |ty: &str| key_type_of(gin, &column(ty));
        assert_eq!(
            offered(gin, of("jsonb")),
            "`jsonb_ops` or `pg_catalog.jsonb_path_ops`"
        );
        assert_eq!(offered(gin, of("tsvector")), "`tsvector_ops`");
        assert_eq!(
            offered(gin, None),
            "`jsonb_ops` or `pg_catalog.jsonb_path_ops` or `tsvector_ops`"
        );
        assert_eq!(
            key_scope(IndexMethod::Gin.policy()),
            "`jsonb` or `tsvector` columns"
        );
        assert_eq!(key_scope(IndexMethod::Btree.policy()), "columns");
    }
}

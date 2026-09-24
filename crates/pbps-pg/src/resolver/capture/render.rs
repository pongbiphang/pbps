//! Which snapshot rows may invoke engine rendering functions. The selected
//! source/type closure is qualified before this is built; unrelated routines
//! and custom datum output are not executed just because their catalog rows
//! were available for name lookup.

use super::logical::{self, Row};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    object: u32,
    attribute: Option<i32>,
}

// Ephemeral same-snapshot locators only, never persisted identities/verifiers.
#[derive(Default)]
pub(super) struct Selection {
    rows: BTreeMap<&'static str, BTreeSet<Key>>,
}

impl Selection {
    pub fn include(&mut self, class: &'static str, row: &Row) -> Result<(), logical::Uncovered> {
        let Some(field) = primary_field(class) else {
            return Ok(());
        };
        let object = logical::number(row, field)?;
        if object == 0 {
            return Err(logical::Uncovered::MissingObject);
        }
        let attribute = if class == "pg_attribute" {
            Some(logical::signed(row, "attnum")?)
        } else {
            None
        };
        self.rows
            .entry(class)
            .or_default()
            .insert(Key { object, attribute });
        Ok(())
    }

    pub fn predicate(&self, class: &str) -> String {
        let Some(keys) = self.rows.get(class).filter(|keys| !keys.is_empty()) else {
            return "false".into();
        };
        let Some(field) = primary_field(class) else {
            return "false".into();
        };
        if class == "pg_attribute" {
            let keys = keys
                .iter()
                .map(|key| {
                    format!(
                        "({}, {})",
                        key.object,
                        key.attribute.expect("attribute key")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("(c.attrelid, c.attnum) IN ({keys})")
        } else {
            let keys = keys
                .iter()
                .map(|key| key.object.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("c.{field} IN ({keys})")
        }
    }
}

fn primary_field(class: &str) -> Option<&'static str> {
    match class {
        "pg_class" | "pg_type" | "pg_proc" | "pg_rewrite" | "pg_attrdef" | "pg_constraint"
        | "pg_collation" | "pg_database" | "pg_policy" | "pg_trigger" => Some("oid"),
        "pg_index" => Some("indexrelid"),
        "pg_partitioned_table" => Some("partrelid"),
        "pg_attribute" => Some("attrelid"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_explicit_snapshot_locators_can_run_a_renderer() {
        let mut selected = Selection::default();
        assert_eq!(selected.predicate("pg_proc"), "false");
        selected
            .include("pg_proc", json!({"oid":42}).as_object().unwrap())
            .unwrap();
        assert_eq!(selected.predicate("pg_proc"), "c.oid IN (42)");
        assert_eq!(selected.predicate("pg_class"), "false");
        selected
            .include(
                "pg_attribute",
                json!({"attrelid":7,"attnum":2}).as_object().unwrap(),
            )
            .unwrap();
        assert_eq!(
            selected.predicate("pg_attribute"),
            "(c.attrelid, c.attnum) IN ((7, 2))"
        );
        assert!(
            selected
                .include("pg_proc", json!({"oid":"42 OR true"}).as_object().unwrap())
                .is_err()
        );
        assert!(
            selected
                .include("pg_proc", json!({"oid":0}).as_object().unwrap())
                .is_err()
        );
    }
}

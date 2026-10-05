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
        if let Some(key) = key(class, row)? {
            self.rows.entry(class).or_default().insert(key);
        }
        Ok(())
    }

    /// Replace each selected row of the first pass with its rendering. The
    /// rendering pass reads the selected rows alone, from the same snapshot,
    /// so every rendered row must replace exactly one selected row and every
    /// selected row must be rendered: a missing or extra one is an
    /// incomplete read, never a row left unrendered (#1538).
    pub fn overlay(
        &self,
        rows: &mut BTreeMap<String, Vec<Row>>,
        rendered: BTreeMap<String, Vec<Row>>,
    ) -> Result<(), logical::Uncovered> {
        for (class, members) in rendered {
            let mut by_key = BTreeMap::new();
            for row in members {
                let key = key(&class, &row)?.ok_or(logical::Uncovered::UnsupportedClass)?;
                if by_key.insert(key, row).is_some() {
                    return Err(logical::Uncovered::DuplicateObject);
                }
            }
            let selected = self.rows.get(class.as_str()).map_or(0, BTreeSet::len);
            if by_key.len() != selected {
                return Err(logical::Uncovered::MissingObject);
            }
            if by_key.is_empty() {
                continue;
            }
            let first = rows
                .get_mut(&class)
                .ok_or(logical::Uncovered::MissingClass)?;
            for row in first.iter_mut() {
                if let Some(rendered) = key(&class, row)?.and_then(|key| by_key.remove(&key)) {
                    *row = rendered;
                }
            }
            if !by_key.is_empty() {
                return Err(logical::Uncovered::MissingObject);
            }
        }
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

fn key(class: &str, row: &Row) -> Result<Option<Key>, logical::Uncovered> {
    let Some(field) = primary_field(class) else {
        return Ok(None);
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
    Ok(Some(Key { object, attribute }))
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

    fn rows(class: &str, members: &[serde_json::Value]) -> BTreeMap<String, Vec<Row>> {
        BTreeMap::from([(
            class.to_owned(),
            members
                .iter()
                .map(|row| row.as_object().unwrap().clone())
                .collect(),
        )])
    }

    #[test]
    fn a_rendering_replaces_exactly_its_selected_row_and_leaves_the_rest() {
        let mut selected = Selection::default();
        selected
            .include("pg_proc", json!({"oid":42}).as_object().unwrap())
            .unwrap();
        let first = rows(
            "pg_proc",
            &[json!({"oid":41,"def":null}), json!({"oid":42,"def":null})],
        );
        let mut merged = first.clone();
        selected
            .overlay(
                &mut merged,
                rows("pg_proc", &[json!({"oid":42,"def":"rendered"})]),
            )
            .unwrap();
        assert_eq!(
            merged["pg_proc"][0], first["pg_proc"][0],
            "unselected row kept"
        );
        assert_eq!(merged["pg_proc"][1]["def"], json!("rendered"));

        // A rendering pass is read from the same snapshot as the first; any
        // other count of rows is an incomplete read, not a partial rendering.
        let missing = selected.overlay(&mut first.clone(), rows("pg_proc", &[]));
        assert_eq!(missing, Err(logical::Uncovered::MissingObject));
        let unselected = selected.overlay(
            &mut first.clone(),
            rows("pg_proc", &[json!({"oid":42}), json!({"oid":41})]),
        );
        assert_eq!(unselected, Err(logical::Uncovered::MissingObject));
        let repeated = selected.overlay(
            &mut first.clone(),
            rows("pg_proc", &[json!({"oid":42}), json!({"oid":42})]),
        );
        assert_eq!(repeated, Err(logical::Uncovered::DuplicateObject));
        let absent = selected.overlay(
            &mut rows("pg_proc", &[json!({"oid":41})]),
            rows("pg_proc", &[json!({"oid":42})]),
        );
        assert_eq!(absent, Err(logical::Uncovered::MissingObject));
        let keyless = Selection::default().overlay(
            &mut rows("pg_roles", &[json!({"rolname":"r"})]),
            rows("pg_roles", &[json!({"rolname":"r"})]),
        );
        assert_eq!(keyless, Err(logical::Uncovered::UnsupportedClass));
    }
}

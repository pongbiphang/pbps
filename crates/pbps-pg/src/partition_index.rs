//! Name allocation only; explicit rebuild recipes still refuse SQL lowering.
use pbps_diff::order::catalog::graph::Fact;
use pbps_model::{ColumnName, IndexPart, ObjectLocalName, TableName};
use std::collections::BTreeSet;

/// `attributes` are PostgreSQL's index attribute names, including expression
/// labels and INCLUDE columns, bound to the index definition by the caller.
/// `unavailable` comes from the shared slot description at the build boundary.
pub fn clone_name(
    leaf: &TableName,
    part: &IndexPart,
    attributes: &Fact<Vec<ColumnName>>,
    unavailable: &BTreeSet<String>,
) -> Result<ObjectLocalName, String> {
    let label = match part {
        IndexPart::PrimaryKey => "pkey",
        IndexPart::Unique(_) => "key",
        IndexPart::Index(_) => "idx",
    };
    let mut addition = String::new();
    if !matches!(part, IndexPart::PrimaryKey) {
        let attributes = attributes.require(&format!("{}: index attribute names", leaf.shown()))?;
        if attributes.is_empty()
            || attributes
                .iter()
                .any(|n| n.raw().is_empty() || n.raw().len() > 63)
        {
            return Err(format!("{}: invalid index attribute names", leaf.shown()));
        }
        for name in attributes {
            if !addition.is_empty() {
                addition.push('_');
            }
            addition.push_str(name.raw());
            if addition.len() >= 64 {
                break;
            }
        }
    }
    // PostgreSQL ChooseRelationName/makeObjectName preserve the label (and
    // retry digits), sharing the remaining byte budget between the two names.
    for ordinal in 0..=unavailable.len() {
        let label = if ordinal == 0 {
            label.to_owned()
        } else {
            format!("{label}{ordinal}")
        };
        let mut a = leaf.name.len();
        let mut b = addition.len();
        let budget = 63 - label.len() - 1 - usize::from(!addition.is_empty());
        while a + b > budget {
            if a > b {
                a -= 1;
            } else {
                b -= 1;
            }
        }
        let second = if addition.is_empty() {
            String::new()
        } else {
            format!("_{}", prefix(&addition, b))
        };
        let candidate = format!("{}{second}_{label}", prefix(&leaf.name, a));
        if !unavailable.contains(&candidate) {
            return Ok(candidate.into());
        }
    }
    unreachable!("one more distinct candidate than occupied names")
}

fn prefix(s: &str, mut bytes: usize) -> &str {
    while !s.is_char_boundary(bytes) {
        bytes -= 1;
    }
    &s[..bytes]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_leaf_names_keep_expression_labels_suffixes_and_utf8_boundaries() {
        let leaf = TableName::new("app", "p1");
        let names = Fact::Present(vec!["expr".into(), "lower".into(), "coalesce".into()]);
        let part = IndexPart::Index("parent_idx".into());
        let occupied = [
            "p1_expr_lower_coalesce_idx".into(),
            "p1_expr_lower_coalesce_idx1".into(),
        ]
        .into();
        assert_eq!(
            clone_name(&leaf, &part, &names, &occupied).unwrap().raw(),
            "p1_expr_lower_coalesce_idx2"
        );
        let long = TableName::new("app", "ä".repeat(31));
        let n = clone_name(
            &long,
            &part,
            &Fact::Present(vec!["ä".repeat(31).into()]),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(n.raw().len() <= 63 && n.raw().ends_with("_idx"));
        for facts in [
            Fact::Absent,
            Fact::Unreadable("42501".into()),
            Fact::Present(vec![]),
        ] {
            assert!(
                clone_name(&leaf, &part, &facts, &BTreeSet::new())
                    .unwrap_err()
                    .contains("app.p1")
            );
        }
    }
}

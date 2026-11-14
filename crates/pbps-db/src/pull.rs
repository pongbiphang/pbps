//! Small helpers every engine's pull needs and none of them should spell
//! again: message phrasing, the limitation recorder and grouping.
//!
//! They live beside [`crate::catalog::Pulled`] because they only phrase or
//! fill what it holds. Nothing here knows an engine's catalog, its SQL or its
//! codes; the moment a helper has to, it belongs to that engine instead.

use std::collections::BTreeMap;
use std::fmt::Display;

use crate::catalog::{Limitation, LimitationTarget};

/// `1 routine` / `4 routines`.
///
/// Generic over the count's integer type because the callers hold their counts
/// as `usize`, `u32` and `i64`; converting at each call site would be three
/// more places to get a sign or a width wrong. `From<u8>` is what gives `1`
/// without a cast, and only integers have it.
pub fn plural<N>(n: N, what: &str) -> String
where
    N: Copy + PartialEq + From<u8> + Display,
{
    if n == N::from(1) {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The names, capped, with the rest counted rather than printed.
///
/// A count alone sends the reader to write the query themselves; the whole
/// list of every routine in a large database is a wall nobody reads. Ten and a
/// remainder is the shape that answers "which ones" for the cases that have an
/// answer and stays one paragraph for the ones that do not.
#[must_use]
pub fn listed(names: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut out = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        out.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    out
}

/// Records a fact the model cannot hold: the warning to show, and, where there
/// is an object to scope it to, the limitation that lets a caller tell
/// somebody else's object from a managed one.
///
/// Takes the two vectors rather than a [`crate::catalog::Pulled`] because the
/// SQL Server assembler fills them as locals before it has one. A `None`
/// target is a warning that no object can be blamed for (SQL Server meets a
/// table id its own read did not return): it must still be shown, and it must
/// not become a limitation scoped to nothing, which a filter by target would
/// either drop or misattribute.
pub fn record_limitation(
    warnings: &mut Vec<String>,
    limitations: &mut Vec<Limitation>,
    target: Option<LimitationTarget>,
    detail: String,
) {
    warnings.push(detail.clone());
    if let Some(target) = target {
        limitations.push(Limitation { target, detail });
    }
}

/// `items` by a key, in key order, each group keeping the order of `items`.
///
/// Borrowed values and a `Copy` key are the whole shape: it is for the join an
/// assembler does between a flat catalog read and the object each row belongs
/// to. A map that owns its values or keys on several fields is a different
/// job, and bending it to fit here would hide the difference.
pub fn group<T, K: Ord + Copy>(items: &[T], key: impl Fn(&T) -> K) -> BTreeMap<K, Vec<&T>> {
    let mut out: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for item in items {
        out.entry(key(item)).or_default().push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::TableName;

    fn names(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("n{i}")).collect()
    }

    #[test]
    fn plural_is_singular_only_for_exactly_one() {
        assert_eq!(plural(1_usize, "routine"), "1 routine");
        assert_eq!(plural(0_usize, "routine"), "0 routines");
        assert_eq!(plural(2_usize, "routine"), "2 routines");
    }

    #[test]
    fn plural_takes_the_counts_integer_type_unchanged() {
        assert_eq!(plural(1_u32, "partition"), "1 partition");
        assert_eq!(plural(3_u32, "partition"), "3 partitions");
        assert_eq!(plural(1_i64, "object"), "1 object");
        // A count is never negative, but the type allows it and must not
        // read as one.
        assert_eq!(plural(-1_i64, "object"), "-1 objects");
    }

    #[test]
    fn listed_prints_every_name_up_to_ten_and_counts_the_rest() {
        assert_eq!(listed(&[]), "");
        assert_eq!(listed(&names(1)), "n1");
        assert_eq!(listed(&names(2)), "n1, n2");
        assert_eq!(listed(&names(10)), names(10).join(", "));
        assert_eq!(
            listed(&names(11)),
            format!("{} and 1 more", names(10).join(", "))
        );
        assert_eq!(
            listed(&names(25)),
            format!("{} and 15 more", names(10).join(", "))
        );
    }

    #[test]
    fn a_limitation_with_a_target_is_both_warned_and_recorded() {
        let (mut warnings, mut limitations) = (Vec::new(), Vec::new());
        let table = TableName::new("dbo", "t");
        record_limitation(
            &mut warnings,
            &mut limitations,
            Some(LimitationTarget::Relation(table.clone())),
            "cannot hold it".to_owned(),
        );
        assert_eq!(warnings, ["cannot hold it"]);
        assert_eq!(
            limitations,
            [Limitation {
                target: LimitationTarget::Relation(table),
                detail: "cannot hold it".to_owned(),
            }]
        );
    }

    #[test]
    fn a_limitation_with_no_target_is_only_warned() {
        let (mut warnings, mut limitations) = (Vec::new(), Vec::new());
        record_limitation(&mut warnings, &mut limitations, None, "unknown".to_owned());
        assert_eq!(warnings, ["unknown"]);
        assert!(limitations.is_empty());
    }

    #[test]
    fn group_orders_by_key_and_keeps_item_order_inside_a_group() {
        let items = [(2, 'a'), (1, 'b'), (2, 'c'), (1, 'd')];
        let grouped = group(&items, |&(k, _)| k);
        let shown: Vec<(i32, Vec<char>)> = grouped
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(|&(_, c)| c).collect()))
            .collect();
        assert_eq!(shown, [(1, vec!['b', 'd']), (2, vec!['a', 'c'])]);
    }

    #[test]
    fn group_of_nothing_has_no_keys() {
        assert!(group(&[] as &[u8], |&b| b).is_empty());
    }
}

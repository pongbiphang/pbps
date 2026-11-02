use super::*;

fn rows(columns: &[&str], data: &[(&[Option<&str>], u64)]) -> Rows {
    Rows {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        rows: data
            .iter()
            .map(|(row, n)| (row.iter().map(|c| c.map(str::to_owned)).collect(), *n))
            .collect(),
    }
}

fn snapshot(table: Rows, allocators: &[(&str, &str)]) -> Snapshot {
    Snapshot {
        tables: BTreeMap::from([("app.t".to_owned(), table)]),
        allocators: allocators
            .iter()
            .map(|(name, next)| {
                (
                    (*name).to_owned(),
                    Allocator {
                        used_by: Some("app.t.id".into()),
                        next: (*next).to_owned(),
                    },
                )
            })
            .collect(),
    }
}

fn equal(a: &Snapshot, b: &Snapshot) -> bool {
    let (tables, allocators) = compare(a, b);
    tables.iter().all(|t| t.equal) && allocators.iter().all(|x| x.equal)
}

#[test]
fn identical_rows_and_allocators_are_equal() {
    let a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    let b = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    assert!(equal(&a, &b));
}

#[test]
fn equal_rows_with_a_different_next_value_are_not_equal() {
    // The measured counterexample: (1,kept),(2,discarded) then DELETE 2, against
    // (1,kept) alone. Their rows are equal; the next ids are 3 and 2.
    let a = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "3")]);
    let b = snapshot(rows(&["id"], &[(&[Some("1")], 1)]), &[("app.s", "2")]);
    let (tables, allocators) = compare(&a, &b);
    assert!(tables.iter().all(|t| t.equal));
    assert!(!allocators[0].equal);
    assert_eq!(allocators[0].original_next.as_deref(), Some("3"));
    assert_eq!(allocators[0].proposed_next.as_deref(), Some("2"));
}

#[test]
fn null_empty_duplicate_case_and_space_are_all_differences() {
    let base = || rows(&["v"], &[(&[Some("a")], 1)]);
    let none = snapshot(base(), &[]);
    for other in [
        rows(&["v"], &[(&[None], 1)]),
        rows(&["v"], &[(&[Some("")], 1)]),
        rows(&["v"], &[(&[Some("a")], 2)]),
        rows(&["v"], &[(&[Some("A")], 1)]),
        rows(&["v"], &[(&[Some("a ")], 1)]),
        rows(&["v"], &[]),
        rows(&["v", "w"], &[(&[Some("a"), None], 1)]),
    ] {
        assert!(!equal(&none, &snapshot(other, &[])));
    }
}

#[test]
fn a_table_or_sequence_only_one_side_has_is_a_difference() {
    let a = snapshot(rows(&["id"], &[]), &[("app.s", "1")]);
    let mut b = snapshot(rows(&["id"], &[]), &[]);
    assert!(!equal(&a, &b));
    b.tables.insert("app.u".into(), rows(&["id"], &[]));
    b.allocators.insert(
        "app.s".into(),
        Allocator {
            used_by: None,
            next: "1".into(),
        },
    );
    assert!(!equal(&a, &b));
}

#[test]
fn shown_differences_are_bounded_but_the_equality_test_is_not() {
    let mut big = Rows {
        columns: vec!["n".into()],
        rows: BTreeMap::new(),
    };
    for i in 0..(SHOWN + 5) {
        big.rows.insert(vec![Some(i.to_string())], 1);
    }
    let a = snapshot(big, &[]);
    let b = snapshot(rows(&["n"], &[]), &[]);
    let (tables, _) = compare(&a, &b);
    assert!(!tables[0].equal);
    assert_eq!(tables[0].differences.len(), SHOWN);
    assert!(tables[0].truncated);
    assert_eq!(tables[0].original_rows, (SHOWN + 5) as u64);
}

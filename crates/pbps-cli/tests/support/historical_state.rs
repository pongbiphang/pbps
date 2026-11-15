//! A state row as an older build wrote it, for the tests that pin old bytes.
//!
//! A snapshot always writes `object_deps`, empty or not, and a row older than
//! `OBJECT_DEPS_VERSION` may not hold it (DEC-2469.7). A test that forces an old
//! version on today's struct would otherwise serialize a row no build wrote and
//! the reader would refuse it. An old row carried its edges as `module_deps`
//! only when there were some, so removing an empty `object_deps` gives its bytes
//! exactly, and a non-empty one is refused here rather than silently dropped.
#![allow(dead_code)]

pub fn historical_state(state: &pbps_model::StateSnapshot) -> String {
    let mut row = serde_json::to_value(state).unwrap();
    let edges = row.as_object_mut().unwrap().shift_remove("object_deps");
    assert_eq!(
        edges,
        Some(serde_json::json!([])),
        "an old row has no `object_deps` to carry edges in"
    );
    serde_json::to_string(&row).unwrap()
}

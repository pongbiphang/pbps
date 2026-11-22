//! Standalone sequences.

use super::*;

/// Standalone sequences, by name (DEC-2721.1). **Every difference is refused here, by
/// name**: this part reads and holds them and plans nothing for them. Creating one arrives
/// in the next part, and altering, renaming or dropping one after it, so a plan that would
/// leave a declaration unmet is refused rather than made without the change that says so.
/// A sequence the target has and the declarations do not is not this project's, and is left
/// alone. The counter is not compared: it is in no field of either side.
pub(super) fn diff_sequences(
    base: &pbps_model::Schema,
    declared: &pbps_model::Schema,
    dialect: &(impl Dialect + ?Sized),
    errs: &mut Vec<DiffError>,
) {
    // Spellings of one type (`int`, `integer`) are one type to the engine.
    let normal = |sequence: &pbps_model::Sequence| {
        let mut sequence = sequence.clone();
        if let Ok(ty) = dialect.normalize_type(&sequence.ty) {
            sequence.ty = ty;
        }
        sequence
    };
    for (name, now) in &declared.sequences {
        let reason = match base.sequences.get(name) {
            None => {
                "the target has none; creating one arrives with the next part of #2721".to_owned()
            }
            Some(was) => {
                let (was, now) = (normal(was), normal(now));
                let differing = sequence_differences(&was, &now, ("declared", "target has"));
                if differing.is_empty() {
                    continue;
                }
                format!(
                    "it differs from the target's ({}); changing one arrives in a later part of \
                     #2721, and for now it stands as the target holds it",
                    differing.join("; ")
                )
            }
        };
        errs.push(DiffError::SequenceRefused {
            name: name.clone(),
            reason,
        });
    }
}

/// What differs between the held sequence and the new one, attribute by attribute, each as
/// `name: <words.0> new, <words.1> held` ("declared", "target has" in a plan; "now",
/// "recorded" in a drift report). A bound or a cache that is `None` is
/// the engine's default, and is said so.
fn sequence_differences(
    was: &pbps_model::Sequence,
    now: &pbps_model::Sequence,
    words: (&str, &str),
) -> Vec<String> {
    fn bound(value: Option<i64>) -> String {
        value.map_or_else(|| "the engine's default".to_owned(), |v| v.to_string())
    }
    let mut out = Vec::new();
    let mut note = |what: &str, declared: String, held: String| {
        if declared != held {
            out.push(format!(
                "{what}: {} {declared}, {} {held}",
                words.0, words.1
            ));
        }
    };
    note(
        "type",
        now.ty.shown().to_string(),
        was.ty.shown().to_string(),
    );
    note("start", now.start.to_string(), was.start.to_string());
    note(
        "increment",
        now.increment.to_string(),
        was.increment.to_string(),
    );
    note("min", bound(now.options.min), bound(was.options.min));
    note("max", bound(now.options.max), bound(was.options.max));
    note(
        "cycle",
        now.options.cycle.to_string(),
        was.options.cycle.to_string(),
    );
    note("cache", bound(now.options.cache), bound(was.options.cache));
    out
}

//! The interactive prompt of SPEC §6.3 — the third channel for identity intent.
//!
//! # What it is, and what it deliberately is not
//!
//! It is a convenience wrapper: what it produces is exactly what
//! `pbps rename` / `pbps drop` produce — one entry in the identity file, in git,
//! reviewed in the merge request. That is what still exists when production
//! deploys the rename five versions later, and it is why this channel adds no
//! new artifact of its own.
//!
//! It is **not** a way to answer faster. Similarity orders the candidates, one
//! pair at a time, and nothing here decides. SPEC §14.3 refuses any
//! `--assume-renames`: a confirmation that can be written once into a CI file or
//! a shell alias has stopped being a confirmation, and this module is the reason
//! that refusal costs the user nothing — the interactive path is where the
//! convenience belongs.
//!
//! # Why it will not prompt in CI
//!
//! [`interactive`] requires both stdin and stderr to be terminals. A pipeline
//! with a captured stdin would otherwise hang forever on a question nobody can
//! see, and a run that prompted on one machine and failed on another would make
//! the ids file depend on where `plan` happened to run.

use std::io::{IsTerminal as _, Write as _};

use pbps_diff::Blocker;
use pbps_model::{ColumnRef, Intent, TableName};

/// Whether a person is there to answer.
///
/// Both streams, not just one. stdin alone is not enough — output redirected to
/// a file means nobody sees the question; stderr alone is not enough — a
/// captured stdin means nobody can answer it.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// One answer the user can pick.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// What the user reads.
    pub label: String,
    pub kind: ChoiceKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChoiceKind {
    /// Complete on its own.
    Rename(Intent),
    /// A drop, once a reason has been typed. The reason is never defaulted and
    /// never suggested: it is the field an audit reads, and a default would be
    /// the answer everybody accepts.
    DropColumn(ColumnRef),
    DropTable(TableName),
}

/// The candidates for one blocker, best first.
///
/// Pure, so the ordering can be tested without a terminal — which is the half of
/// this module that can be got wrong quietly.
pub fn choices(b: &Blocker) -> Vec<Choice> {
    let mut out = Vec::new();
    match b {
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => {
            let mut pairs: Vec<(&String, &String)> = disappeared
                .iter()
                .flat_map(|from| appeared.iter().map(move |to| (from, to)))
                .collect();
            // Sorted by similarity, with the names themselves as the tiebreak so
            // two equally similar candidates come out in the same order every
            // run. An unstable order here would make the prompt's numbering
            // depend on hash iteration, and "press 1" would mean different
            // things on two machines.
            pairs.sort_by(|a, b| {
                similarity(b.0, b.1)
                    .total_cmp(&similarity(a.0, a.1))
                    .then_with(|| (a.0, a.1).cmp(&(b.0, b.1)))
            });
            for (from, to) in pairs {
                out.push(Choice {
                    label: format!("{from} was renamed to {to}"),
                    kind: ChoiceKind::Rename(Intent::RenameColumn {
                        table: table.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    }),
                });
            }
            for from in disappeared {
                out.push(Choice {
                    label: format!("{from} was dropped (you will be asked why)"),
                    kind: ChoiceKind::DropColumn(ColumnRef {
                        table: table.clone(),
                        name: from.clone(),
                    }),
                });
            }
        }
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => {
            let mut pairs: Vec<(&TableName, &TableName)> = disappeared
                .iter()
                .flat_map(|from| appeared.iter().map(move |to| (from, to)))
                .collect();
            pairs.sort_by(|a, b| {
                similarity(&a.0.to_string(), &a.1.to_string())
                    .total_cmp(&similarity(&b.0.to_string(), &b.1.to_string()))
                    .reverse()
                    .then_with(|| (a.0, a.1).cmp(&(b.0, b.1)))
            });
            for (from, to) in pairs {
                out.push(Choice {
                    label: format!("table {from} was renamed to {to}"),
                    kind: ChoiceKind::Rename(Intent::RenameTable {
                        from: from.clone(),
                        to: to.clone(),
                    }),
                });
            }
            for from in disappeared {
                out.push(Choice {
                    label: format!("table {from} was dropped (you will be asked why)"),
                    kind: ChoiceKind::DropTable(from.clone()),
                });
            }
        }
        Blocker::DropColumnNeedsReason { column } => out.push(Choice {
            label: format!("{column} was dropped (you will be asked why)"),
            kind: ChoiceKind::DropColumn(column.clone()),
        }),
        Blocker::DropTableNeedsReason { table } => out.push(Choice {
            label: format!("table {table} was dropped (you will be asked why)"),
            kind: ChoiceKind::DropTable(table.clone()),
        }),
        // A stale or misspelled annotation. There is nothing to choose: the
        // remedy is to edit the file, and offering an option here would invite
        // the user to confirm a typo into the identity file.
        Blocker::UnusedIntent { .. } => {}
    }
    out
}

/// How alike two identifiers are, in `0.0..=1.0`.
///
/// Levenshtein, normalized by the longer string. Written out rather than pulled
/// in: it is the only thing this tool needs from a string-distance crate, and
/// the tool gets audited, where a shorter dependency tree is worth more than a
/// hundred lines saved (the same trade as `civil_from_days`).
///
/// Case-insensitive, because `customerName` → `customer_name` is a rename
/// somebody actually makes, and it would otherwise rank below a coincidence.
fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.to_ascii_lowercase().chars().collect();
    let b: Vec<char> = b.to_ascii_lowercase().chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let longest = a.len().max(b.len());

    // One row at a time: the full matrix is never needed and a wide table would
    // allocate for nothing.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - (prev[b.len()] as f64 / longest as f64)
}

/// Asks about every blocker, one at a time, and returns the intents chosen.
///
/// Returns `None` the moment the user declines — by choosing nothing, by
/// answering something that is not an option, or by closing stdin. A partial
/// answer is not written: the caller falls back to printing the copy-pastable
/// commands of §6.4, which is the behaviour with no TTY at all, so a user who
/// changes their mind is never left with half an identity file.
pub fn ask(blockers: &[Blocker]) -> Option<Vec<Intent>> {
    let stdin = std::io::stdin();
    ask_from(blockers, stdin.lock())
}

/// [`ask`] with the answers coming from anywhere.
///
/// Split out so the conversation can be tested without a pseudo-terminal. The
/// alternative — driving the real binary under `script` — would test the same
/// logic through a tool that behaves differently on each platform CI runs, which
/// is a way of not testing it on the platform where it breaks.
pub fn ask_from<R: std::io::BufRead>(blockers: &[Blocker], reader: R) -> Option<Vec<Intent>> {
    let mut lines = reader.lines();
    let mut intents = Vec::new();

    for b in blockers {
        let choices = choices(b);
        if choices.is_empty() {
            continue;
        }
        // To stderr throughout: stdout may be a plan somebody is piping, and a
        // question in the middle of it would corrupt the file.
        eprintln!();
        eprintln!("  {}", question(b));
        for (i, c) in choices.iter().enumerate() {
            eprintln!("    {}) {}", i + 1, c.label);
        }
        eprint!("  Which is it? [1-{}, or blank to stop] ", choices.len());
        let _ = std::io::stderr().flush();

        let answer = lines.next()?.ok()?;
        let picked = answer.trim().parse::<usize>().ok()?;
        let choice = choices.get(picked.checked_sub(1)?)?;

        intents.push(match &choice.kind {
            ChoiceKind::Rename(i) => i.clone(),
            ChoiceKind::DropColumn(column) => Intent::DropColumn {
                column: column.clone(),
                reason: reason(&mut lines, &column.to_string())?,
            },
            ChoiceKind::DropTable(table) => Intent::DropTable {
                table: table.clone(),
                reason: reason(&mut lines, &table.to_string())?,
            },
        });
    }
    (!intents.is_empty()).then_some(intents)
}

/// Asks why something is being dropped, and refuses to accept nothing.
///
/// The reason is what the tombstone shows an audit (SPEC §6.1). An empty one
/// would be the answer everyone gives, so a blank line ends the prompt rather
/// than recording a drop nobody explained.
fn reason<R: std::io::BufRead>(lines: &mut std::io::Lines<R>, what: &str) -> Option<String> {
    eprint!("  Why is {what} being dropped? (an audit reads this) ");
    let _ = std::io::stderr().flush();
    let answer = lines.next()?.ok()?;
    let answer = answer.trim();
    (!answer.is_empty()).then(|| answer.to_owned())
}

fn question(b: &Blocker) -> String {
    match b {
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => format!(
            "{table}: {} disappeared, {} is new",
            disappeared.join(", "),
            appeared.join(", ")
        ),
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => format!(
            "table {} disappeared, {} is new",
            list(disappeared),
            list(appeared)
        ),
        Blocker::DropColumnNeedsReason { column } => {
            format!("{column} disappeared from the declarations")
        }
        Blocker::DropTableNeedsReason { table } => {
            format!("table {table} disappeared from the declarations")
        }
        Blocker::UnusedIntent { intent } => format!("{intent:?}"),
    }
}

fn list<T: std::fmt::Display>(v: &[T]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(script: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(script.as_bytes().to_vec())
    }

    fn table() -> TableName {
        "dbo.customer".parse().unwrap()
    }

    #[test]
    fn identical_strings_are_completely_similar() {
        assert_eq!(similarity("a", "a"), 1.0);
        assert!(similarity("customer_name", "customer_nam") > 0.9);
        assert!(similarity("abc", "xyz") < 0.1);
    }

    /// A rename that only changes the casing convention is one people actually
    /// make, and it must not rank below a coincidence.
    #[test]
    fn casing_is_not_treated_as_a_difference() {
        assert_eq!(similarity("FullName", "fullname"), 1.0);
        assert!(similarity("customerName", "customer_name") > similarity("customerName", "id"));
    }

    /// The whole permitted use of similarity: putting the likely pair first
    /// (SPEC §14.3). It orders; it never decides.
    #[test]
    fn the_likeliest_rename_is_offered_first() {
        let b = Blocker::AmbiguousColumns {
            table: table(),
            disappeared: vec!["customer_name".into(), "zip".into()],
            appeared: vec!["postcode".into(), "full_name".into()],
        };
        let c = choices(&b);
        assert!(
            c[0].label
                .starts_with("customer_name was renamed to full_name"),
            "{:?}",
            c[0].label
        );
        // Every pairing is still offered, and every drop after them: ordering is
        // not filtering, and a candidate that was hidden would be one the user
        // cannot choose.
        assert_eq!(c.len(), 4 + 2);
        assert!(c[4].label.contains("dropped"), "{:?}", c[4].label);
    }

    /// Two candidates the metric cannot separate must still come out in one
    /// order, or "press 2" means different things on two machines.
    #[test]
    fn equally_similar_candidates_are_ordered_deterministically() {
        let b = Blocker::AmbiguousColumns {
            table: table(),
            disappeared: vec!["aa".into()],
            appeared: vec!["bb".into(), "cc".into()],
        };
        let first = choices(&b);
        let second = choices(&b);
        assert_eq!(first, second);
        assert!(first[0].label.contains("bb"), "{:?}", first[0].label);
    }

    /// The negative case: a stale annotation has no answer a prompt can take.
    /// Offering one would invite the user to confirm their own typo into the
    /// identity file, where nothing would ever question it again.
    #[test]
    fn a_stale_intent_offers_nothing_to_choose() {
        let b = Blocker::UnusedIntent {
            intent: Intent::RenameTable {
                from: table(),
                to: "dbo.nope".parse().unwrap(),
            },
        };
        assert!(choices(&b).is_empty());
    }

    /// A drop reached through the prompt still needs a reason, exactly as
    /// `pbps drop --reason` does: the channel changed, the artifact did not.
    #[test]
    fn a_drop_choice_is_not_complete_without_a_reason() {
        let b = Blocker::DropColumnNeedsReason {
            column: "dbo.customer.pii".parse().unwrap(),
        };
        let c = choices(&b);
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].kind, ChoiceKind::DropColumn(_)));
    }

    /// The happy path: the offered rename becomes exactly the intent
    /// `pbps rename` would have produced. The channel changed; the artifact did
    /// not (SPEC §6.3).
    #[test]
    fn choosing_a_rename_produces_the_same_intent_as_the_command() {
        let b = Blocker::AmbiguousColumns {
            table: table(),
            disappeared: vec!["customer_name".into()],
            appeared: vec!["full_name".into()],
        };
        let got = ask_from(std::slice::from_ref(&b), answers("1\n")).unwrap();
        assert_eq!(
            got,
            vec![Intent::RenameColumn {
                table: table(),
                from: "customer_name".into(),
                to: "full_name".into(),
            }]
        );
    }

    /// A drop asks a second question, and the answer goes into the tombstone
    /// verbatim. Nothing here may default it.
    #[test]
    fn choosing_a_drop_asks_why_and_keeps_the_answer() {
        let b = Blocker::DropColumnNeedsReason {
            column: "dbo.customer.pii".parse().unwrap(),
        };
        let got = ask_from(std::slice::from_ref(&b), answers("1\nREG-2026-042\n")).unwrap();
        assert_eq!(
            got,
            vec![Intent::DropColumn {
                column: "dbo.customer.pii".parse().unwrap(),
                reason: "REG-2026-042".into(),
            }]
        );
    }

    /// The negative cases, and the important ones: every way of not answering
    /// has to record nothing at all. A partial answer written to the identity
    /// file would be a decision the user never made.
    #[test]
    fn no_answer_of_any_kind_records_anything() {
        let ambiguous = Blocker::AmbiguousColumns {
            table: table(),
            disappeared: vec!["a".into()],
            appeared: vec!["b".into()],
        };
        let drop = Blocker::DropColumnNeedsReason {
            column: "dbo.customer.pii".parse().unwrap(),
        };

        // A blank line: "I would rather think about it".
        assert!(ask_from(std::slice::from_ref(&ambiguous), answers("\n")).is_none());
        // Closed stdin, with no newline at all.
        assert!(ask_from(std::slice::from_ref(&ambiguous), answers("")).is_none());
        // Something that is not an option. Never charitably interpreted: a
        // mistyped number that resolved to "the nearest choice" would record a
        // rename nobody chose.
        assert!(ask_from(std::slice::from_ref(&ambiguous), answers("yes\n")).is_none());
        assert!(ask_from(std::slice::from_ref(&ambiguous), answers("0\n")).is_none());
        assert!(ask_from(std::slice::from_ref(&ambiguous), answers("99\n")).is_none());
        // A drop with no reason. An empty reason is the answer everyone gives,
        // so it ends the prompt instead of writing a tombstone that explains
        // nothing.
        assert!(ask_from(std::slice::from_ref(&drop), answers("1\n\n")).is_none());
        assert!(ask_from(std::slice::from_ref(&drop), answers("1\n   \n")).is_none());
    }

    /// Stopping half way through several questions abandons the answers already
    /// given. Writing them would leave the user with an identity file that is
    /// neither what they had nor what they were deciding on.
    #[test]
    fn stopping_part_way_abandons_the_earlier_answers() {
        let blockers = vec![
            Blocker::AmbiguousColumns {
                table: table(),
                disappeared: vec!["a".into()],
                appeared: vec!["b".into()],
            },
            Blocker::DropColumnNeedsReason {
                column: "dbo.customer.pii".parse().unwrap(),
            },
        ];
        assert!(ask_from(&blockers, answers("1\n\n")).is_none());
    }
}

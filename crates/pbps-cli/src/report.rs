//! Turning results into text a person can read and act on.
//!
//! The principle here is that **every blocked situation comes with a command you
//! can copy and paste**. Non-interactive environments are never prompted
//! (constraint 6 in CLAUDE.md), so the error message itself has to be the
//! instructions — otherwise a user reading a CI log sees "this is ambiguous" with
//! no idea what to type.

use pbps_diff::{Blocker, RenameSide};
use pbps_model::change::DeleteCause;
use pbps_model::escape::Escaped;
use pbps_model::plan_names::{At, Holder, PlanNames};
use pbps_model::{Change, ChangeSet, DriftReport, Intent, RiskClass, TableName};

/// One intent in the user's own vocabulary.
///
/// An intent retains its meaning, not where it was supplied (SPEC §6). Describe
/// the operation without inventing an annotation for a command or prompt answer.
pub fn intent(i: &Intent) -> String {
    match i {
        Intent::RenameType { from, to } => {
            format!("rename type {} -> {}", from.shown(), to.shown())
        }
        Intent::DropType { name, reason } => {
            format!("drop type {} (reason: {reason})", name.shown())
        }
        Intent::RenameTable { from, to } => format!(
            "rename table {from} -> {to}",
            from = from.shown(),
            to = to.shown()
        ),
        Intent::RenameColumn { table, from, to } => {
            format!(
                "rename column {table}.{from} -> {table}.{to}",
                table = table.shown(),
                from = from.shown(),
                to = to.shown(),
            )
        }
        Intent::DropTable { table, reason } => format!(
            "drop table {table} (reason: {reason})",
            table = table.shown()
        ),
        Intent::DropColumn { column, reason } => format!(
            "drop column {column} (reason: {reason})",
            column = column.shown()
        ),
        Intent::RenameRole { from, to } => format!(
            "rename role {from} -> {to}",
            from = from.shown(),
            to = to.shown(),
        ),
        Intent::DropRole { role, reason } => {
            format!("drop role {role} (reason: {reason})", role = role.shown(),)
        }
        Intent::MoveRows {
            table,
            max_rows,
            reason,
        } => format!(
            "move at most {max_rows} DEFAULT-partition rows into {table} (reason: {reason})",
            table = table.shown()
        ),
    }
}

pub fn blockers(list: &[Blocker]) -> String {
    blockers_with_intents(list, &[])
}

pub fn blockers_with_intents(list: &[Blocker], supplied: &[Intent]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} change(s) could not be decided automatically\n",
        list.len()
    ));
    for b in list {
        out.push('\n');
        out.push_str(&one_blocker(b));
    }
    if let Some(batch) = batch_remedy(list, supplied) {
        out.push('\n');
        out.push_str(&batch);
        out.push('\n');
    }
    out
}

/// Keep the complete remedy in one finding rather than copying a growing batch
/// into every blocker. The individual finding IDs retain their meaning.
pub fn blocker_findings(list: &[Blocker]) -> Vec<crate::output::Finding> {
    let mut findings: Vec<_> = list.iter().map(blocker_finding).collect();
    if let Some(batch) = batch_remedy(list, &[]) {
        findings.push(
            crate::output::Finding::note(
                "identity.batch-required",
                "Record all pending identity decisions together.",
            )
            .remedy(batch),
        );
    }
    findings
}

fn decision_count(blocker: &Blocker) -> Option<usize> {
    match blocker {
        Blocker::AmbiguousTypes { disappeared, .. } => Some(disappeared.len()),
        Blocker::DropTypeNeedsReason { .. } => Some(1),
        Blocker::AmbiguousColumns { disappeared, .. } => Some(disappeared.len()),
        Blocker::AmbiguousRoles { disappeared, .. } => Some(disappeared.len()),
        Blocker::AmbiguousTables { disappeared, .. } => Some(disappeared.len()),
        Blocker::DropColumnNeedsReason { .. }
        | Blocker::DropTableNeedsReason { .. }
        | Blocker::DropRoleNeedsReason { .. } => Some(1),
        Blocker::ProvisionalTableIdentity { blocker, .. } => decision_count(blocker),
        Blocker::UnusedIntent { .. }
        | Blocker::ConflictingRenameIntents { .. }
        | Blocker::RenameTargetExists { .. }
        | Blocker::UnrepresentableName { .. }
        | Blocker::MissingType { .. }
        | Blocker::MoveNotBesideDefault { .. } => None,
    }
}

fn pair_flag(flag: &str, first: &str, second: &str, second_name: &str) -> String {
    format!(
        "{flag} {} {}",
        shell_arg(first).unwrap_or_else(|| placeholder("object name")),
        shell_arg(second).unwrap_or_else(|| {
            // Placeholder prose must never become a recorded audit reason.
            if second_name == "why" {
                "\"\"".into()
            } else {
                placeholder(second_name)
            }
        }),
    )
}

fn supplied_flag(intent: &Intent) -> Option<String> {
    Some(match intent {
        Intent::RenameType { from, to } => pair_flag(
            "--rename-type",
            &from.raw().to_string(),
            &to.raw().to_string(),
            "new",
        ),
        Intent::DropType { name, reason } => pair_flag(
            "--drop-type",
            &name.raw().to_string(),
            reason.as_ref(),
            "why",
        ),
        Intent::RenameColumn { table, from, to } => pair_flag(
            "--rename",
            &format!(
                "{table}.{from}",
                table = table.raw(),
                // Preserve raw command arguments for shell_arg.
                from = from.raw(),
            ),
            to.raw(),
            "new column",
        ),
        Intent::RenameTable { from, to } => pair_flag(
            "--rename-table",
            &from.raw().to_string(),
            &to.raw().to_string(),
            "new table",
        ),
        Intent::RenameRole { from, to } => {
            pair_flag("--rename-role", from.raw(), to.raw(), "new role")
        }
        Intent::DropColumn { column, reason } => {
            pair_flag("--drop", &column.raw().to_string(), reason.as_ref(), "why")
        }
        Intent::DropTable { table, reason } => pair_flag(
            "--drop-table",
            &table.raw().to_string(),
            reason.as_ref(),
            "why",
        ),
        Intent::DropRole { role, reason } => {
            pair_flag("--drop-role", role.raw(), reason.as_ref(), "why")
        }
        // MoveRows remains a separate command (SPEC §6.5); do not advertise an
        // option the identity batch does not accept.
        Intent::MoveRows { .. } => return None,
    })
}

fn batch_remedy(list: &[Blocker], supplied: &[Intent]) -> Option<String> {
    // A conflict or unused intent must be corrected before it can be carried
    // forward as a valid answer. Resolution already classifies those cases.
    let pending: usize = list
        .iter()
        .map(decision_count)
        .collect::<Option<Vec<_>>>()?
        .iter()
        .sum();
    if pending + supplied.len() < 2 {
        return None;
    }
    let mut flags = supplied
        .iter()
        .map(supplied_flag)
        .collect::<Option<Vec<_>>>()?;
    let mut choices = Vec::new();
    for blocker in list {
        batch_flags(blocker, &mut flags, &mut choices);
    }
    let mut out = String::from(
        "Identity decisions are recorded together; separate single-intent commands cannot record a partial answer.\n",
    );
    if !supplied.is_empty() {
        out.push_str("The supplied intents were valid but not recorded. Include them again with the remaining decisions.\n");
    }
    out.push_str("Replace the quoted placeholders with your own values, quoted for your shell.\n");
    out.push_str(&format!("    pbps intent {}\n", flags.join(" ")));
    if !choices.is_empty() {
        out.push_str("Append one chosen disposition for each disappeared object to that same command; no rename is chosen for you:\n");
        out.push_str(&choices.join("\n"));
        out.push('\n');
    }
    Some(out)
}

fn batch_flags(blocker: &Blocker, fixed: &mut Vec<String>, choices: &mut Vec<String>) {
    let mut alternatives = |rename: &str, drop: &str, from: &str, targets: Vec<String>| {
        for to in targets {
            choices.push(format!(
                "    if renamed: {}",
                pair_flag(rename, from, &to, "new name")
            ));
        }
        choices.push(format!(
            "    if dropped: {}",
            pair_flag(drop, from, "", "why")
        ));
    };
    match blocker {
        Blocker::AmbiguousTypes {
            disappeared,
            appeared,
        } => {
            for from in disappeared {
                alternatives(
                    "--rename-type",
                    "--drop-type",
                    &from.raw().to_string(),
                    appeared.iter().map(|n| n.raw().to_string()).collect(),
                );
            }
        }
        Blocker::DropTypeNeedsReason { name } => {
            fixed.push(pair_flag("--drop-type", &name.raw().to_string(), "", "why"))
        }
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => {
            for from in disappeared {
                alternatives(
                    "--rename",
                    "--drop",
                    &format!(
                        "{table}.{from}",
                        table = table.raw(),
                        // Preserve raw command arguments for shell_arg.
                        from = from.raw(),
                    ),
                    appeared.iter().map(|c| c.raw().to_owned()).collect(),
                );
            }
        }
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => {
            for from in disappeared {
                alternatives(
                    "--rename-table",
                    "--drop-table",
                    &from.raw().to_string(),
                    appeared.iter().map(|name| name.raw().to_string()).collect(),
                );
            }
        }
        Blocker::AmbiguousRoles {
            disappeared,
            appeared,
        } => {
            for from in disappeared {
                alternatives(
                    "--rename-role",
                    "--drop-role",
                    from.raw(),
                    appeared.iter().map(|name| name.raw().to_owned()).collect(),
                );
            }
        }
        Blocker::DropColumnNeedsReason { column } => {
            fixed.push(pair_flag("--drop", &column.raw().to_string(), "", "why"))
        }
        Blocker::DropTableNeedsReason { table } => fixed.push(pair_flag(
            "--drop-table",
            &table.raw().to_string(),
            "",
            "why",
        )),
        Blocker::DropRoleNeedsReason { role } => {
            fixed.push(pair_flag("--drop-role", role.raw(), "", "why"))
        }
        Blocker::ProvisionalTableIdentity { from, to, blocker } => {
            // A derived column choice is conditional on its table identity.
            // Putting its flag on the fixed command would silently decide it.
            choices.push(format!(
                "    only if {from} -> {to} is the table rename you choose:",
                from = from.shown(),
                to = to.shown()
            ));
            let mut conditional = Vec::new();
            batch_flags(blocker, &mut conditional, choices);
            choices.extend(conditional.into_iter().map(|flag| format!("      {flag}")));
        }
        Blocker::UnusedIntent { .. }
        | Blocker::ConflictingRenameIntents { .. }
        | Blocker::RenameTargetExists { .. }
        | Blocker::UnrepresentableName { .. }
        | Blocker::MissingType { .. }
        | Blocker::MoveNotBesideDefault { .. } => {}
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn module_counts_distinguish_raw_names_from_their_display_spelling() {
        let id: pbps_model::ModuleId = "app.x\n\\\u{200b}(integer)".parse().unwrap();
        let escaped: pbps_model::ModuleId = id.shown().to_string().parse().unwrap();
        let changes: Vec<_> = [id.clone(), id, escaped]
            .into_iter()
            .map(|id| {
                pbps_model::PlannedChange::new(Change::DropModule {
                    id,
                    kind: pbps_model::ModuleKind::Function,
                })
            })
            .collect();
        let ids = pbps_model::IdsFile::default();
        let names = PlanNames::of(&changes, &ids).unwrap();
        assert_eq!(touched(&names), (0, 2));
        // Neither rendering choice is allowed to collapse distinct identities.
        let changes = &changes[..2];
        assert_eq!(touched(&PlanNames::of(changes, &ids).unwrap()), (0, 1));
    }

    #[cfg(unix)]
    #[test]
    fn standalone_remedy_commands_round_trip_raw_table_and_column_names() {
        for part in [r"one\backslash", "名前_ünï"] {
            let table = TableName::new("app", part);
            for (blocker, expected) in [
                (
                    Blocker::DropTableNeedsReason {
                        table: table.clone(),
                    },
                    vec![
                        "drop-table".into(),
                        format!("app.{part}"),
                        "--reason".into(),
                        "".into(),
                    ],
                ),
                (
                    Blocker::DropColumnNeedsReason {
                        column: table.column(part),
                    },
                    vec![
                        "drop".into(),
                        format!("app.{part}.{part}"),
                        "--reason".into(),
                        "".into(),
                    ],
                ),
                (
                    Blocker::AmbiguousTables {
                        disappeared: vec![table.clone()],
                        appeared: vec![TableName::new("app", "new")],
                    },
                    vec![
                        "rename-table".into(),
                        format!("app.{part}"),
                        "app.new".into(),
                    ],
                ),
                (
                    Blocker::AmbiguousColumns {
                        table: table.clone(),
                        disappeared: vec!["old".into()],
                        appeared: vec![part.into()],
                    },
                    vec!["rename".into(), format!("app.{part}.old"), part.into()],
                ),
            ] {
                let text = one_blocker(&blocker);
                let command = text
                    .lines()
                    .find_map(|line| line.split_once("pbps ").map(|(_, command)| command))
                    .unwrap();
                let script = format!("set -- {command}; printf '%s\\0' \"$@\"");
                let output = std::process::Command::new("sh")
                    .args(["-c", &script])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                let args: Vec<&[u8]> = output
                    .stdout
                    .strip_suffix(&[0])
                    .expect("printf terminates the final argument")
                    .split(|&b| b == 0)
                    .collect();
                assert_eq!(
                    args,
                    expected.iter().map(String::as_bytes).collect::<Vec<_>>(),
                    "{text:?}"
                );
            }
        }
        for part in ["a\u{1b}[31mb", "a\rb", "a\u{9b}b"] {
            let text = one_blocker(&Blocker::DropTableNeedsReason {
                table: TableName::new("app", part),
            });
            assert!(
                text.contains("pbps drop-table \"<table>\" --reason \"\""),
                "{text:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pasteable_flags_round_trip_raw_names_and_refuse_terminal_controls() {
        // Parse the actual generated flags with a shell; comparing our own
        // quote spelling would miss an argument silently rewritten on paste.
        for part in [r"one\backslash", "名前_ünï"] {
            let table = TableName::new("app", part);
            let intent = Intent::RenameTable {
                from: table.clone(),
                to: TableName::new("app", "new"),
            };
            let flags = supplied_flag(&intent).unwrap();
            let script = format!("set -- {flags}; printf '%s\\0' \"$@\"");
            let output = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let args: Vec<&[u8]> = output
                .stdout
                .strip_suffix(&[0])
                .expect("printf terminates the final argument")
                .split(|&b| b == 0)
                .collect();
            assert_eq!(
                args,
                [
                    b"--rename-table".as_slice(),
                    format!("app.{part}").as_bytes(),
                    b"app.new".as_slice()
                ]
            );
            let column = Intent::RenameColumn {
                table,
                from: part.into(),
                to: "next".into(),
            };
            let flags = supplied_flag(&column).unwrap();
            let script = format!("set -- {flags}; printf '%s\\0' \"$@\"");
            let output = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            let args: Vec<&[u8]> = output
                .stdout
                .strip_suffix(&[0])
                .expect("printf terminates the final argument")
                .split(|&b| b == 0)
                .collect();
            assert_eq!(args[1], format!("app.{part}.{part}").as_bytes());
        }
        for part in ["a\u{1b}[31mb", "a\rb", "a\u{9b}b", "a\u{200b}b"] {
            let table = TableName::new("app", part);
            let intent = Intent::DropTable {
                table,
                reason: "retired".parse().unwrap(),
            };
            assert_eq!(
                supplied_flag(&intent).unwrap(),
                "--drop-table \"<object name>\" retired"
            );
        }
        assert_eq!(shell_arg("名前_ünï"), Some("\"名前_ünï\"".into()));
        // The existing cross-shell refusals continue to apply.
        for part in ["-flag", "trailing\\", r"two\\slashes", "$expand", "a!b"] {
            assert_eq!(shell_arg(part), None, "{part:?}");
        }
    }

    #[test]
    fn human_and_json_share_one_complete_batch_without_changing_blocker_ids() {
        let blockers = vec![
            Blocker::DropColumnNeedsReason {
                column: "app.t.old".parse().unwrap(),
            },
            Blocker::DropRoleNeedsReason {
                role: "old reader".into(),
            },
        ];
        let human = super::blockers(&blockers);
        let findings = blocker_findings(&blockers);
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].id, "identity.drop-column-needs-reason");
        assert_eq!(findings[1].id, "identity.drop-role-needs-reason");
        let remedy = findings[2].remedy.as_ref().unwrap();
        assert!(human.contains(remedy));
        assert!(
            remedy.contains("pbps intent --drop app.t.old \"\" --drop-role \"old reader\" \"\"")
        );
        assert_eq!(blocker_findings(&blockers[..1]).len(), 1);
    }

    #[test]
    fn every_missing_identity_reason_is_an_empty_double_quoted_argument() {
        for blocker in [
            Blocker::DropColumnNeedsReason {
                column: "app.t.a".parse().unwrap(),
            },
            Blocker::DropTableNeedsReason {
                table: "app.t".parse().unwrap(),
            },
            Blocker::DropRoleNeedsReason {
                role: "reader".into(),
            },
            Blocker::DropTypeNeedsReason {
                name: r#"app."type space""#.parse().unwrap(),
            },
            Blocker::AmbiguousTypes {
                disappeared: vec!["app.old".parse().unwrap()],
                appeared: vec!["app.new".parse().unwrap()],
            },
        ] {
            let human = one_blocker(&blocker);
            assert!(human.contains("\"\""), "{human}");
            assert!(!human.contains("<why>") && !human.contains("<reason>"));
            if matches!(&blocker, Blocker::DropTypeNeedsReason { .. }) {
                assert!(!has_bare_placeholder(&human));
            }
            let json = blocker_finding(&blocker);
            assert!(
                json.remedy
                    .as_deref()
                    .unwrap_or(&json.message)
                    .contains("\"\"")
            );
        }
    }

    #[test]
    fn an_ambiguous_group_offers_flags_without_choosing_a_rename() {
        let blockers = [Blocker::AmbiguousColumns {
            table: "app.t".parse().unwrap(),
            disappeared: vec!["a".into(), "b".into()],
            appeared: vec!["x".into(), "y".into()],
        }];
        let remedy = batch_remedy(&blockers, &[]).unwrap();
        let command = remedy
            .lines()
            .find(|line| line.contains("pbps intent"))
            .unwrap();
        assert!(!command.contains("--rename"));
        for from in ["a", "b"] {
            for to in ["x", "y"] {
                assert!(remedy.contains(&format!("--rename app.t.{from} {to}")));
            }
            assert!(remedy.contains(&format!("--drop app.t.{from} \"\"")));
        }
    }

    #[test]
    fn incomplete_answers_are_retained_but_invalid_answers_are_not_endorsed() {
        let supplied = Intent::DropColumn {
            column: "app.t.a".parse().unwrap(),
            reason: "first reason".parse().unwrap(),
        };
        let pending = Blocker::DropColumnNeedsReason {
            column: "app.t.b".parse().unwrap(),
        };
        let remedy = batch_remedy(
            std::slice::from_ref(&pending),
            std::slice::from_ref(&supplied),
        )
        .unwrap();
        assert!(remedy.contains("valid but not recorded"));
        assert!(remedy.contains("pbps intent --drop app.t.a \"first reason\" --drop app.t.b \"\""));
        let invalid = Blocker::UnusedIntent {
            intent: supplied.clone(),
        };
        assert!(batch_remedy(&[pending, invalid], &[supplied]).is_none());
    }

    #[test]
    fn provisional_decisions_and_shell_sensitive_values_are_never_fixed_arguments() {
        let blocker = Blocker::ProvisionalTableIdentity {
            from: "app.old".parse().unwrap(),
            to: "app.new".parse().unwrap(),
            blocker: Box::new(Blocker::DropColumnNeedsReason {
                column: "app.new.old".parse().unwrap(),
            }),
        };
        let supplied = Intent::DropRole {
            role: "reader".into(),
            reason: "$REASON".parse().unwrap(),
        };
        let remedy = batch_remedy(&[blocker], &[supplied]).unwrap();
        let command = remedy
            .lines()
            .find(|line| line.contains("pbps intent"))
            .unwrap();
        assert_eq!(command.trim(), "pbps intent --drop-role reader \"\"");
        assert!(!remedy.contains("$REASON"));
        assert!(remedy.contains("only if app.old -> app.new"));
        assert!(remedy.contains("--drop app.new.old"));
    }
}

/// One blocker as a typed finding: a stable id, what happened, and the commands
/// that resolve it.
///
/// The remedy is the same text the human view prints — there is one description
/// of how to resolve an ambiguity, and a second one written for JSON would drift
/// from it the first time a command gained a flag.
pub fn blocker_finding(b: &Blocker) -> crate::output::Finding {
    fn id(b: &Blocker) -> &'static str {
        match b {
            Blocker::AmbiguousTypes { .. } => "identity.ambiguous-types",
            Blocker::DropTypeNeedsReason { .. } => "identity.drop-type-needs-reason",
            Blocker::ProvisionalTableIdentity { blocker, .. } => id(blocker),
            Blocker::AmbiguousColumns { .. } => "identity.ambiguous-columns",
            Blocker::AmbiguousTables { .. } => "identity.ambiguous-tables",
            Blocker::DropColumnNeedsReason { .. } => "identity.drop-column-needs-reason",
            Blocker::DropTableNeedsReason { .. } => "identity.drop-table-needs-reason",
            Blocker::AmbiguousRoles { .. } => "identity.ambiguous-roles",
            Blocker::DropRoleNeedsReason { .. } => "identity.drop-role-needs-reason",
            Blocker::UnusedIntent { .. } => "identity.unused-intent",
            Blocker::ConflictingRenameIntents { .. } => "identity.conflicting-rename-intents",
            Blocker::RenameTargetExists { .. } => "identity.rename-target-exists",
            Blocker::UnrepresentableName { .. } => "identity.unrepresentable-name",
            Blocker::MoveNotBesideDefault { .. } => "identity.move-not-beside-default",
            Blocker::MissingType { .. } => "identity.missing-type",
        }
    }
    let text = one_blocker(b);
    // The first line says what happened; the rest are the commands.
    let (message, remedy) = match text.split_once("\n\n") {
        Some((head, tail)) => (head.trim().to_owned(), tail.trim().to_owned()),
        None => (text.trim().to_owned(), String::new()),
    };
    let f = crate::output::Finding::error(id(b), message);
    if remedy.is_empty() {
        f
    } else {
        f.remedy(remedy)
    }
}

fn one_blocker(b: &Blocker) -> String {
    let why = "\"\"";
    match b {
        Blocker::AmbiguousTypes {
            disappeared,
            appeared,
        } => format!(
            "  types {} disappeared; {} are new. Record a rename with renamed_from or pbps intent --rename-type, or supply pbps intent --drop-type {type_arg} {why}.\n",
            disappeared
                .iter()
                .map(|n| n.shown().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            appeared
                .iter()
                .map(|n| n.shown().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            type_arg = placeholder("type"),
        ),
        Blocker::DropTypeNeedsReason { name } => {
            let argument =
                shell_arg(&name.raw().to_string()).unwrap_or_else(|| placeholder("type"));
            format!(
                "  type {} disappeared; record its reason with pbps intent --drop-type {argument} {why}\n",
                name.shown(),
            )
        }
        Blocker::MissingType {
            name,
            unrepresentable,
        } => {
            let why = match unrepresentable {
                Some(detail) => format!("is unrepresentable: {detail}"),
                None => "is absent from the pulled schema".into(),
            };
            format!(
                "  type `{name}` {why}; its identity remains recorded; \
                 pull cannot record a drop without explicit human intent\n",
                name = name.shown()
            )
        }
        Blocker::ProvisionalTableIdentity { from, to, blocker } => format!(
            "  if {from} -> {to} is what you meant (provisional table identity):\n{}",
            one_blocker(blocker),
            from = from.shown(),
            to = to.shown()
        ),
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  {table}: {} disappeared, {} is new\n\n",
                disappeared
                    .iter()
                    .map(|c| c.shown().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                appeared
                    .iter()
                    .map(|c| c.shown().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                table = table.shown()
            );
            for from in disappeared {
                let source = format!(
                    "{table}.{from}",
                    table = table.raw(),
                    // Preserve raw command arguments for shell_arg.
                    from = from.raw(),
                );
                let source_arg = shell_arg(&source).unwrap_or_else(|| placeholder("object name"));
                for to in appeared {
                    let command = pair_flag("pbps rename", &source, to.raw(), "new column");
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  {command}\n",
                        from = from.shown(),
                        to = to.shown(),
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop {source_arg} --reason {why}\n",
                    from = from.shown(),
                ));
            }
            s
        }
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  table {} disappeared, {} is new\n\n",
                join(disappeared.iter().map(TableName::shown)),
                join(appeared.iter().map(TableName::shown))
            );
            for from in disappeared {
                let source_arg =
                    shell_arg(&from.raw().to_string()).unwrap_or_else(|| placeholder("table"));
                for to in appeared {
                    let command = pair_flag(
                        "pbps rename-table",
                        &from.raw().to_string(),
                        &to.raw().to_string(),
                        "new table",
                    );
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  {command}\n",
                        from = from.shown(),
                        to = to.shown()
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop-table {source_arg} --reason {why}\n",
                    from = from.shown()
                ));
            }
            s
        }
        Blocker::DropColumnNeedsReason { column } => format!(
            "  {column} disappeared from the declarations, but a drop must record why (an audit asks for it)\n\n    pbps drop {argument} --reason {why}\n",
            column = column.shown(),
            argument =
                shell_arg(&column.raw().to_string()).unwrap_or_else(|| placeholder("column"))
        ),
        Blocker::DropTableNeedsReason { table } => format!(
            "  table {table} disappeared from the declarations, but a drop must record why\n\n    pbps drop-table {argument} --reason {why}\n",
            table = table.shown(),
            argument = shell_arg(&table.raw().to_string()).unwrap_or_else(|| placeholder("table"))
        ),
        Blocker::AmbiguousRoles {
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  role {} disappeared, {} is new\n\n",
                join(disappeared.iter().map(pbps_model::RoleName::shown)),
                join(appeared.iter().map(pbps_model::RoleName::shown))
            );
            for from in disappeared {
                let source_arg = shell_arg(from.raw()).unwrap_or_else(|| placeholder("role"));
                for to in appeared {
                    let command = pair_flag("pbps rename-role", from.raw(), to.raw(), "new role");
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  {command}\n",
                        from = from.shown(),
                        to = to.shown(),
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop-role {source_arg} --reason {why}\n",
                    from = from.shown(),
                ));
            }
            s
        }
        Blocker::DropRoleNeedsReason { role } => format!(
            "  role {role} disappeared from the declarations, but a drop must record why — \
             its members lose whatever it granted\n\n    pbps drop-role {argument} --reason {why}\n",
            argument = shell_arg(role.raw()).unwrap_or_else(|| placeholder("role")),
            role = role.shown(),
        ),
        Blocker::UnusedIntent { intent: i } => {
            format!(
                "  this intent matches nothing in either the declarations or the identity file, likely a typo:\n    {}\n\n    correct the names where this intent was supplied: edit its declaration annotation, fix and rerun the command, or retry with a corrected prompt answer\n",
                intent(i)
            )
        }
        // The opposite complaint to the one above: these match too much. Every
        // one of them is well formed, and nothing says which the
        // author meant — so they are listed and none is performed.
        Blocker::ConflictingRenameIntents {
            side,
            name,
            intents,
        } => {
            let claim = match side {
                RenameSide::Source => format!("all rename {name}"),
                RenameSide::Target => format!("all rename something to {name}"),
            };
            let mut s = format!(
                "  {} intents {claim}, and nothing says which one you meant:\n",
                intents.len()
            );
            for i in intents {
                s.push_str(&format!("    {}\n", intent(i)));
            }
            // Where it was written, not where it was read. An intent reaches
            // `resolve` from a `renamed_from:` line, from the `pbps rename`
            // being run right now (`cmd_intent` appends it to the loaded ones)
            // or from an answer at the prompt, and a remedy naming only the
            // declarations sends the user to delete the annotation that is
            // right — after which the column it explained is an unexplained
            // addition and the old one an unexplained drop.
            // "the rest", not "the other": three columns can each carry
            // `renamed_from: old`, and an instruction to remove one of them
            // leaves two and the same blocker on the next run.
            s.push_str(
                "\n    keep one and drop the rest, each where it was written: a \
                 `renamed_from:` line in the declarations, or the `pbps rename` you \
                 have just run, or a prompt answer; correct the input and retry\n",
            );
            s
        }
        // Keeping the occupant can leave the source without a disposition, and
        // taking the annotation away cannot supply one: the disappearance
        // needs its own recorded intent (SPEC 7.1, #554). Conditional, since
        // another rename or a companion drop may already account for the
        // source, and then dropping the erroneous statement is all it takes. The drop command is named
        // by the source's own finding, which carries its kind.
        Blocker::RenameTargetExists { target } => format!(
            "  rename target {target} already exists in the declarations\n\n    \
             to keep the existing declaration, remove or correct the `renamed_from:` annotation or rename intent, \
             and unless another rename or a recorded drop already accounts for the source, decide what becomes of it: \
             declare it again, record its drop with a reason, or rename it to a name nothing holds; \
             to reuse that name for the source, first free the occupied target in an earlier revision\n"
        ),
        // No command fixes this one: unlike an ambiguous rename or a drop
        // missing a reason, the object itself has a name this tool's own
        // `.`-joined identity format cannot carry, so the remedy is renaming
        // the object, not resolving an ambiguity pbps can see both sides of.
        // `what` already carries the right noun for every case (`"schema"`,
        // `"table"`, `"column"`, `"view"`, `"routine"`, `"trigger"`), so one
        // message covers them all; `table` only adds a `"dbo.t: "` prefix
        // when there is a containing table worth naming (a column, or a
        // trigger's own table).
        Blocker::UnrepresentableName { what, part, table } => {
            let prefix = table
                .as_ref()
                .map(|t| format!("{t}: ", t = t.shown()))
                .unwrap_or_default();
            format!(
                "  {prefix}{what} `{part}` contains a `.`, which pbps uses to separate schema, \
                 table and column; it cannot mint an identity for this {what}.\n\n    rename \
                 the {what} so it no longer contains a `.`, then declare it (or pull) again\n"
            )
        }
        Blocker::MoveNotBesideDefault { table } => format!(
            "  {table} is not a range or list partition whose parent declares a DEFAULT \
             partition, so no rows can move into it\n\n    declare {table} as a range or list \
             partition beside the DEFAULT, or drop the `move-rows` request\n",
            table = table.shown()
        ),
    }
}

fn join(v: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    v.into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The headline: how much, where, and how risky.
///
/// Printed above the change list so that the first thing a reader sees is the
/// shape of the plan rather than its first line. A plan touching sixty tables
/// scrolls past; a plan touching one and dropping a column does not, and the two
/// must not look the same for the first screenful (SPEC §14.1).
///
/// Stable by construction: the counts come from the typed ChangeSet and the risk
/// order is [`RiskClass::ALL`], so two runs over the same plan produce the same
/// text and a diff of two summaries means the plans really differ.
/// How many distinct tables and how many modules a change set touches.
///
/// `Change::table()` returns the *object* name, which for a view, procedure,
/// function or trigger is the module's own name — so counting its distinct
/// values called a one-view plan "1 table". `module_name()` is what separates
/// them, and it exists precisely because they are different kinds of object
/// (ADR-0002).
pub fn touched(names: &PlanNames<'_>) -> (usize, usize) {
    let mut tables = std::collections::BTreeSet::new();
    let mut modules = std::collections::BTreeSet::new();
    for (i, p) in names.changes().iter().enumerate() {
        match (p.change.module_id(), p.change.table()) {
            (Some(m), _) => {
                modules.insert(m.raw().to_string());
            }
            (None, Some(table)) => {
                let holder = match names.holder(table, At::Before(i)) {
                    Holder::Vacant => names.holder(table, At::After(i)),
                    other @ (Holder::Tracked(_) | Holder::Unmanaged) => other,
                };
                let named = match holder {
                    Holder::Tracked(uid) => names.table_at(uid, At::End).unwrap_or(table),
                    Holder::Unmanaged | Holder::Vacant => table,
                };
                tables.insert(named.clone());
            }
            // A role is neither; it is counted in its own summary line.
            (None, None) => {}
        }
    }
    (tables.len(), modules.len())
}

/// The role a change acts on, under the name the change itself carries — the
/// *old* name for a rename, resolved at its own position in the name index.
///
/// `Change::roles` answers with both ends of a rename, which is the right
/// answer for "did this plan touch that role" and the wrong one for counting:
/// collected as they come, one renamed role is two.
// Exhaustive rather than a wildcard, for the reason `Change::roles` gives: a
// change added later that names a role has to be classified here, or the
// summary stops counting it.
fn role_of(change: &Change) -> Option<&pbps_model::RoleName> {
    match change {
Change::RebuildPartitionedTable(_) | Change::BuildPartitionedIndex(_) | Change::CopyTableRows(_) | Change::AnalyzeTableTree(_) => None,
        Change::AddEnumValue { .. } | Change::AlterDomain { .. } | Change::RenameEnumValue { .. } | Change::EnumRebuild { .. } | Change::CreateType { .. } | Change::DropType { .. } | Change::RenameType { .. } => None,
        Change::CreateRole { name, .. }
        | Change::DropRole { name, .. }
        | Change::Grant { role: name, .. }
        | Change::Revoke { role: name, .. } => Some(name),
        Change::RenameRole { from, .. } => Some(from),
        Change::CreateTable { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::AlterColumnNumbering { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::SetStorageParameters { .. }
        | Change::DropPolicy { .. }
        | Change::CreatePolicy { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetReplicaIdentity { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::AddComputedColumn { .. }
        | Change::DropComputedColumn { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        // `PUBLIC` is not a role the summary can count: no declaration, ids
        // file or pull ever names it (ADR-0010 §5).
        | Change::PublicExecution { .. } => None,
    }
}

/// "3 table(s)", "2 module(s)", or both — never a count of one naming the
/// other.
pub fn objects(tables: usize, modules: usize, roles: usize) -> String {
    let mut parts = Vec::new();
    if tables > 0 || (modules == 0 && roles == 0) {
        parts.push(format!("{tables} table(s)"));
    }
    if modules > 0 {
        parts.push(format!("{modules} module(s)"));
    }
    if roles > 0 {
        parts.push(format!("{roles} role(s)"));
    }
    parts.join(" and ")
}

/// How many distinct roles a change set touches (ADR-0005).
pub fn touched_roles(names: &PlanNames<'_>) -> usize {
    names
        .changes()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let name = role_of(&p.change)?;
            let holder = match names.role_holder(name, At::Before(i)) {
                Holder::Vacant => names.role_holder(name, At::After(i)),
                other @ (Holder::Tracked(_) | Holder::Unmanaged) => other,
            };
            Some(match holder {
                Holder::Tracked(uid) => names.role_at(uid, At::End).unwrap_or(name),
                Holder::Unmanaged | Holder::Vacant => name,
            })
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

pub fn summary(cs: &ChangeSet, names: &PlanNames<'_>) -> String {
    if cs.is_empty() {
        return String::new();
    }
    let (tables, modules) = touched(names);
    let mut out = format!(
        "\n  {} change(s) across {}.\n",
        cs.changes.len(),
        objects(tables, modules, touched_roles(names))
    );

    let risks = cs.risks();
    if risks.is_empty() {
        out.push_str("  No risk class applies; this plan needs no --allow.\n");
        return out;
    }
    for class in RiskClass::ALL {
        if !risks.contains(&class) {
            continue;
        }
        let n = cs
            .changes
            .iter()
            .filter(|p| p.risks.contains(&class))
            .count();
        out.push_str(&format!(
            "    {:<12} {n:>3} change(s) — {}\n",
            class.as_str(),
            class.why()
        ));
    }
    out
}

pub fn plan(cs: &ChangeSet, names: &PlanNames<'_>) -> String {
    if cs.is_empty() {
        return "No changes.\n".to_owned();
    }

    let mut out = summary(cs, names);
    out.push_str(&changes(cs));
    // The advice names the *gated* classes only. A labelled-but-ungated one
    // (`grant-widen`, ADR-0005) is on every line it applies to above, and in
    // the summary; putting it in the `--allow` would advise a flag the gate
    // never asks for.
    let risks = cs.gated_risks();
    if !risks.is_empty() {
        out.push_str(&format!(
            "\n  This plan contains risky changes and needs approval to apply: --allow {}\n",
            risks
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
        if risks.contains(&RiskClass::Destructive) {
            out.push_str(&format!(
                "  Some of them are destructive: {}.\n",
                RiskClass::Destructive.why()
            ));
        }
    }
    if cs.risks().contains(&RiskClass::GrantWiden) {
        out.push_str(
            "\n  This plan widens access (grant-widen). No flag gates it; the merge request is \
             where the grants are reviewed.\n",
        );
    }
    out
}

/// The change list alone, grouped by table.
///
/// Separate from [`plan`] because the `--allow` advice below it belongs to a
/// plan and to nothing else: a drift report describes what already happened, and
/// telling the reader which flag would approve it invites them to approve their
/// way past a schema someone changed by hand.
///
/// Every name, description and finding in it is text out of the plan, shown
/// escaped (DEC-2032.1, #2035): this is what a reviewer approves from, and a
/// hand-made plan could otherwise put a line of its own in it. The line breaks
/// here are the literals; none comes from a value.
pub fn changes(cs: &ChangeSet) -> String {
    changes_with_remedy(cs, None)
}

pub fn required_rows(guard: &pbps_model::DropGuard) -> Vec<String> {
    guard.requirements.values().map(|r| format!(
        "{} requires `{}` immediately before removal; the scan holds an exclusive table lock through the drop, so writers wait",
        guard.table.shown(), Escaped(&r.predicate))).collect()
}

fn changes_with_remedy(cs: &ChangeSet, remedy: Option<&str>) -> String {
    let mut out = String::new();
    let mut current = None;
    for p in &cs.changes {
        let table = p.change.subject();
        if current.as_deref() != Some(table.as_str()) {
            out.push_str(&format!("\n  {}\n", Escaped(&table)));
            if let Some(remedy) = remedy {
                out.push_str(&format!("    {remedy}\n"));
            }
            current = Some(table);
        }
        let risks = if p.risks.is_empty() {
            String::new()
        } else {
            format!(
                "  [{}]",
                p.risks
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!("    {}{}\n", describe(&p.change), risks));
        if let Some(guard) = &p.drop_guard {
            for line in required_rows(guard) {
                out.push_str(&format!("      {line}\n"));
            }
        }
        // A step a planning rule added says which, and for what (#1776,
        // DEC-1958.3). A plan read back from its file has none to say.
        if let Some(o) = &p.provenance {
            out.push_str(&format!(
                "      added by {}: {} for {} — {}\n",
                o.rule,
                o.role.as_str(),
                Escaped(&o.source),
                o.reason
            ));
        }
        // The analyzers' findings, under the change they are about, at the
        // severity the project chose (ADR-0008).
        for f in &p.findings {
            let finding = crate::policy_finding(f);
            out.push_str(&format!(
                "      {}: {} — {}\n",
                finding.severity_word(),
                Escaped(&finding.id),
                finding.message
            ));
        }
    }
    out
}

/// A step a planning rule added, for `plan --format json` (DEC-1958.3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct StepOrigin {
    /// The step's position in the plan, from 0.
    pub step: usize,
    /// The rule that added it.
    pub rule: String,
    /// `split part`, `teardown` or `restoration`.
    pub role: String,
    /// The subject of the logical change it serves.
    pub source: String,
    pub reason: String,
}

/// The origin of each step a planning rule added, in plan order.
pub fn origins(cs: &ChangeSet) -> Vec<StepOrigin> {
    cs.changes
        .iter()
        .enumerate()
        .filter_map(|(step, p)| {
            let o = p.provenance.as_ref()?;
            Some(StepOrigin {
                step,
                rule: o.rule.to_owned(),
                role: o.role.as_str().to_owned(),
                source: o.source.clone(),
                reason: o.reason.clone(),
            })
        })
        .collect()
}

/// Shared by human drift subjects and the existing JSON finding remedies.
pub const DRIFT_REMEDY: &str = "Three ways out (SPEC 8.3): accept the change with `pbps pull` \
    and merge by hand; restore the database to the declarations, then run a fresh \
    `pbps plan --db` and `pbps apply --plan`; or rebaseline with \
    `pbps baseline --reason …`.";

/// A drift report as prose.
///
/// The changes are phrased as "what the database has grown", because that is
/// what happened. Turning them round into "the plan would drop it" would smuggle
/// a remedy into a report whose job is to describe — and the remedy is a
/// judgement call with three legitimate answers (SPEC §8.3).
pub fn drift(r: &DriftReport) -> String {
    let mut out = format!(
        "Environment: {}\nBaseline:    entry #{} recorded {}\n",
        r.environment, r.baseline.entry_id, r.baseline.applied_at
    );

    if !r.unmanaged.is_empty() {
        out.push_str(&format!(
            "Unmanaged:   {} table(s) left alone: {}\n",
            r.unmanaged.len(),
            join(r.unmanaged.iter().map(TableName::shown))
        ));
    }

    if !r.has_drift() {
        out.push_str("\nNo drift: the database matches its recorded state.\n");
        return out;
    }

    out.push_str("\nDRIFT: the database no longer matches its recorded state.\n");
    if !r.unexpressible.is_empty() {
        // First, because these are the ones no workflow can resolve: `pull`
        // cannot express them either, so the reader has to act by hand.
        out.push_str(&format!(
            "\n  {} difference(s) that cannot even be expressed as changes:\n",
            r.unexpressible.len()
        ));
        for e in &r.unexpressible {
            out.push_str(&format!("    {e}\n"));
            out.push_str(&format!("      {DRIFT_REMEDY}\n"));
        }
    }
    if !r.changes.is_empty() {
        out.push_str("\n  Differences found (recorded state -> database as it is now):\n");
        // The plan's own vocabulary, indented, minus its `--allow` advice: a
        // reader who has read one plan can read this without learning a second
        // vocabulary, but nothing here is waiting to be approved.
        for line in changes_with_remedy(&r.changes, Some(DRIFT_REMEDY)).lines() {
            if line.trim().is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("  {line}\n"));
            }
        }
    } else {
        // The checksums differ but the differ produced nothing: something the
        // model does not carry has changed. Saying so is far better than an
        // empty list that reads like "nothing, really".
        out.push_str(
            "\n  The state fingerprints differ, but no difference could be expressed as a\n  \
             change. Something outside what pbps models has moved; compare the recorded\n  \
             state_json by hand.\n",
        );
    }
    if r.changes.is_empty() && r.unexpressible.is_empty() {
        out.push_str(&format!("\n  {DRIFT_REMEDY}\n"));
    }
    out
}

/// What a dev-database rehearsal found (SPEC §9.3).
pub fn rehearsal(r: &crate::rehearsal::Findings) -> String {
    let mut out = format!(
        "\nDev rehearsal: built the baseline in {} statement(s), applied {} more.\n",
        r.built, r.applied
    );
    out.push_str("  Separation from production is operator-vouched.\n");
    if r.externals_staged {
        out.push_str(
            "  Externals were staged before the baseline and not compared with any target.\n",
        );
    }
    if !r.structural.is_empty() {
        out.push_str(
            "\n  The plan does NOT converge: after applying it, the database still differs \
             from the declarations.\n",
        );
        for d in &r.structural {
            out.push_str(&format!("    {d}\n"));
        }
    }
    if !r.spelling.is_empty() {
        // The one thing no offline normalization can produce: what the engine
        // actually stored. Rewriting the declaration in that form is what makes
        // the difference stop being reported after every apply.
        out.push_str(
            "\n  These differ only in how the engine spells them. Each costs one rebuilt \
             constraint per apply until the declaration is written in the stored form:\n",
        );
        for d in &r.spelling {
            out.push_str(&format!("    {d}\n"));
        }
    }
    if r.structural.is_empty() && r.spelling.is_empty() {
        out.push_str("  The declarations compile and the plan converges on them.\n");
    }
    // A container runs Developer edition — the Enterprise feature set — while
    // production may be Standard. Saying so keeps a green rehearsal from
    // reading as a promise it cannot make (ADR-0003 decision 3).
    out.push_str(
        "  This is still a preview: it proves syntax and convergence, not edition \
         capabilities, and only `pbps plan --db` produces an applyable plan.\n",
    );
    out
}

pub fn describe(c: &Change) -> String {
    match c {
        Change::RebuildPartitionedTable(_)
        | Change::BuildPartitionedIndex(_)
        | Change::CopyTableRows(_)
        | Change::AnalyzeTableTree(_) => {
            let (operation, _) = c.pending_rebuild().expect("rebuild variant");
            format!(
                "pending blocking rewrite: {operation} for {} (unavailable)",
                Escaped(&c.subject())
            )
        }
        Change::AddEnumValue {
            name,
            value,
            placement,
            ..
        } => {
            let position = match placement {
                Some(pbps_model::EnumPlacement::Before(label)) => {
                    format!(" before {}", Escaped(label))
                }
                Some(pbps_model::EnumPlacement::After(label)) => {
                    format!(" after {}", Escaped(label))
                }
                None => String::new(),
            };
            format!(
                "+ add enum value {} to {}{position}",
                Escaped(value),
                name.shown()
            )
        }
        Change::AlterDomain {
            name,
            from,
            to,
            half,
            ..
        } => {
            let mut parts = Vec::new();
            if let Some(delta) = pbps_model::DomainDelta::of(from, to) {
                match half {
                    pbps_model::DomainHalf::Loosen => {
                        parts.extend(
                            delta
                                .drop_checks
                                .iter()
                                .map(|check| format!("drop check {}", check.shown())),
                        );
                        if delta.drop_not_null {
                            parts.push("drop NOT NULL".to_owned());
                        }
                        match delta.default {
                            Some(Some(_)) => parts.push("set the default".to_owned()),
                            Some(None) => parts.push("drop the default".to_owned()),
                            None => {}
                        }
                    }
                    pbps_model::DomainHalf::Tighten => {
                        if delta.set_not_null {
                            parts.push("set NOT NULL".to_owned());
                        }
                        parts.extend(delta.add_checks.iter().map(|(check, _)| {
                            format!(
                                "add check {}, validated over every existing row",
                                check.shown()
                            )
                        }));
                    }
                }
            }
            format!(
                "~ {} domain {}: {}",
                match half {
                    pbps_model::DomainHalf::Loosen => "loosen",
                    pbps_model::DomainHalf::Tighten => "tighten",
                },
                name.shown(),
                parts.join(", ")
            )
        }
        Change::RenameEnumValue { name, from, to, .. } => format!(
            "~ rename enum value {} -> {} in {}",
            Escaped(from),
            Escaped(to),
            name.shown()
        ),
        Change::EnumRebuild {
            name, tmp, step, ..
        } => match step {
            pbps_model::EnumRebuildStep::Create {
                definition,
                removed,
                why,
                ..
            } => {
                let labels = match definition.as_ref() {
                    pbps_model::ManagedType::Enum { values } => format!(
                        " with labels {}",
                        values
                            .iter()
                            .map(|v| Escaped(v).to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    pbps_model::ManagedType::Domain { .. } => String::new(),
                };
                let why = why
                    .as_ref()
                    .map(|why| format!(" ({})", Escaped(why)))
                    .unwrap_or_default();
                let removed = if removed.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; removes {}",
                        removed
                            .iter()
                            .map(|v| Escaped(v).to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                format!(
                    "~ rebuild {} {} as {}{labels}{removed}{why}",
                    definition.kind(),
                    name.shown(),
                    tmp.shown()
                )
            }
            pbps_model::EnumRebuildStep::Retype { column, .. } => format!(
                "~ move {} to the rebuilt {} (rewrites {}: ACCESS EXCLUSIVE lock and a full table rewrite)",
                column.shown(),
                name.shown(),
                column.table.shown()
            ),
            pbps_model::EnumRebuildStep::Swap { comment, .. } => format!(
                "~ drop the old {0} and rename {1} to {0}{2}",
                name.shown(),
                tmp.shown(),
                if comment.is_some() {
                    ", keeping its comment"
                } else {
                    ""
                }
            ),
        },
        Change::CreateType {
            name, definition, ..
        } => format!("+ create {} {}", definition.kind(), name.shown()),
        Change::DropType {
            name, definition, ..
        } => format!("- drop {} {}", definition.kind(), name.shown()),
        Change::RenameType { from, to, .. } => {
            format!("~ rename type {} -> {}", from.shown(), to.shown())
        }
        // A partition has no columns of its own to count; what it is, is its
        // parent and the rows it takes (#1170).
        Change::CreateTable {
            name,
            table,
            beside_default,
            moves_at_most,
            ..
        } => match &table.partition_of {
            Some(of) => match &of.bound {
                pbps_model::PartitionBound::Default => {
                    format!(
                        "+ create table {name}, the default partition of {}",
                        of.parent.shown(),
                        name = name.shown()
                    )
                }
                bound @ (pbps_model::PartitionBound::Range { .. }
                | pbps_model::PartitionBound::List { .. }
                | pbps_model::PartitionBound::Hash { .. }) => {
                    let list = |data: &[pbps_model::BoundDatum]| {
                        data.iter()
                            .map(|d| match d {
                                pbps_model::BoundDatum::Value(v) => format!("{v:?}"),
                                end @ (pbps_model::BoundDatum::MinValue
                                | pbps_model::BoundDatum::MaxValue) => end.to_string(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let rows = match bound {
                        pbps_model::PartitionBound::Range { from, to } => {
                            format!("from ({}) to ({})", list(from), list(to))
                        }
                        // LIST and HASH (#1632).
                        pbps_model::PartitionBound::List { values } => format!(
                            "in ({})",
                            values
                                .iter()
                                .map(|v| match v {
                                    pbps_model::ListDatum::Value(v) => format!("{v:?}"),
                                    pbps_model::ListDatum::Null => "NULL".to_owned(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        pbps_model::PartitionBound::Hash { modulus, remainder } => {
                            format!("with remainder {remainder} of modulus {modulus}")
                        }
                        pbps_model::PartitionBound::Default => String::new(),
                    };
                    let made = format!(
                        "+ create table {name}, a partition of {} for rows {rows}",
                        of.parent.shown(),
                        name = name.shown()
                    );
                    // The rows it takes from the DEFAULT are the change's to
                    // say, since they leave that partition (#1547), and so
                    // is how many a human allowed it to take (#1788).
                    match (beside_default, moves_at_most) {
                        (Some(default), Some(most)) => format!(
                            "{made}, moving those rows out of the default partition {default}, \
                             at most {most} as allowed",
                            default = default.shown()
                        ),
                        (Some(default), None) => format!(
                            "{made}, moving those rows out of the default partition {default}, \
                             none allowed: the apply refuses any it holds",
                            default = default.shown()
                        ),
                        (None, _) => made,
                    }
                }
            },
            None => format!(
                "+ create table {name} ({} columns)",
                table.columns.len(),
                name = name.shown()
            ),
        },
        Change::DropTable {
            name,
            detach_from: None,
            ..
        } => format!("- drop table {name}", name = name.shown()),
        Change::DetachPartition {
            table,
            parent,
            names,
            ..
        } => format!(
            "~ detach table {table} from {parent}, keeping its rows, with {} name(s) of its own",
            names
                .iter()
                .filter(|n| n.name.as_ref() != Some(&n.parent))
                .count(),
            table = table.shown(),
            parent = parent.shown()
        ),
        Change::AttachPartition {
            table,
            parent,
            bound,
            ..
        } => {
            let partition = if *bound == pbps_model::PartitionBound::Default {
                "its default partition"
            } else {
                "a partition"
            };
            format!(
                "~ attach table {table} to {parent} as {partition}, keeping its rows",
                table = table.shown(),
                parent = parent.shown()
            )
        }
        // Said, because the detach is a statement of its own and locks the
        // parent (#1171).
        Change::DropTable {
            name,
            detach_from: Some(parent),
            ..
        } => format!(
            "- drop table {name}, a partition of {parent}, detached from it first",
            name = name.shown(),
            parent = parent.shown()
        ),
        Change::RenameTable { from, to, .. } => format!(
            "~ rename table {from} -> {to}",
            from = from.shown(),
            to = to.shown()
        ),
        Change::AddColumn { name, column, .. } => {
            format!(
                "+ add column {name} {}",
                column.ty.shown(),
                name = name.shown(),
            )
        }
        Change::DropColumn { column, .. } => format!("- drop column {}", column.name.shown()),
        Change::RenameColumn { from, to, .. } => format!(
            "~ rename column {from} -> {to}",
            from = from.shown(),
            to = to.shown(),
        ),
        // The whole expression, never elided: a reviewer approving a plan is
        // approving what the data becomes, and it is the one text in the
        // plan that nobody but its author has read (DEC-2331.1).
        Change::AlterColumnType {
            column,
            from,
            to,
            using,
            ..
        } => {
            format!(
                "~ {} type {from} -> {to}{}",
                column.name.shown(),
                using
                    .as_ref()
                    .map(|expression| format!(" USING {}", Escaped(expression)))
                    .unwrap_or_default(),
                from = from.shown(),
                to = to.shown(),
            )
        }
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => format!(
            "~ {} becomes {}",
            column.name.shown(),
            if *to_nullable { "nullable" } else { "NOT NULL" }
        ),
        Change::AlterColumnDefault { column, to, .. } => match to {
            Some(v) => format!(
                "~ {} default -> {v}",
                column.name.shown(),
                v = pbps_model::escape::Escaped(v),
            ),
            None => format!("~ {} default removed", column.name.shown()),
        },
        Change::AlterColumnExpression { column, to, .. } => {
            format!(
                "~ {} generated as {to}",
                column.name.shown(),
                to = pbps_model::escape::Escaped(&to.to_string()),
            )
        }
        Change::AlterColumnNumbering {
            column, from, to, ..
        } => format!(
            "~ {} numbered by {} -> {}",
            column.name.shown(),
            numbering_source(from),
            numbering_source(to)
        ),
        Change::AddComputedColumn { name, computed, .. } => format!(
            "+ add computed column {name} as ({}){}{}",
            Escaped(&computed.expression),
            if computed.persisted { " persisted" } else { "" },
            if computed.not_null { " not null" } else { "" },
            name = name.shown(),
        ),
        Change::DropComputedColumn { name, .. } => {
            format!("- drop computed column {name}", name = name.shown(),)
        }
        Change::SetColumnDeprecated { column, reason, .. } => match reason {
            Some(r) => format!("~ {} marked deprecated: {}", column.name.shown(), r.shown()),
            None => format!("~ {} no longer deprecated", column.name.shown()),
        },
        Change::SetPrimaryKey { to, .. } => match to {
            Some(pk) => format!(
                "~ primary key -> ({})",
                pk.columns
                    .iter()
                    .map(|n| n.shown().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "- drop primary key".to_owned(),
        },
        Change::SetTablePersistence { unlogged, .. } => {
            if *unlogged { "~ unlogged" } else { "~ logged" }.to_owned()
        }
        // Named as a column's, under the partition it is listed under
        // (#1581). Without its own, the partition takes its parent's again.
        Change::SetPartitionDefault {
            column,
            to,
            fallback,
            ..
        } => match (to, fallback) {
            (Some(v), _) => format!(
                "~ {column} default -> {v}",
                column = column.shown(),
                v = pbps_model::escape::Escaped(v),
            ),
            (None, Some(_)) => format!(
                "~ {column} default -> its parent's",
                column = column.shown(),
            ),
            (None, None) => format!("~ {column} default removed", column = column.shown(),),
        },
        Change::SetPartitionNotNull {
            column, not_null, ..
        } => format!(
            "~ {column} becomes {}",
            if *not_null { "NOT NULL" } else { "nullable" },
            column = column.shown(),
        ),
        Change::SetIndexStorageParameters {
            target, set, reset, ..
        } => {
            let mut parts: Vec<String> = set
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{k} = {v}",
                        k = Escaped(k),
                        v = pbps_model::escape::Escaped(v),
                    )
                })
                .collect();
            parts.extend(reset.iter().map(|k| format!("{} reset", Escaped(k))));
            let of = match target {
                pbps_model::IndexPart::PrimaryKey => "primary key".to_owned(),
                pbps_model::IndexPart::Unique(n) => {
                    format!("unique constraint {n}", n = n.shown(),)
                }
                pbps_model::IndexPart::Index(n) => format!("index {n}", n = n.shown(),),
            };
            format!("~ storage parameters of {of}: {}", parts.join(", "))
        }
        Change::SetStorageParameters { set, reset, .. } => {
            let mut parts: Vec<String> = set
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{k} = {v}",
                        k = Escaped(k),
                        v = pbps_model::escape::Escaped(v),
                    )
                })
                .collect();
            parts.extend(reset.iter().map(|k| format!("{} reset", Escaped(k))));
            format!("~ storage parameters: {}", parts.join(", "))
        }
        Change::DropPolicy { name, .. } => format!("- drop row policy {}", Escaped(name)),
        Change::CreatePolicy { name, policy, .. } => format!(
            "+ create row policy {} FOR {} AS {}",
            Escaped(name),
            policy.command.as_str().to_ascii_uppercase(),
            if policy.restrictive {
                "RESTRICTIVE"
            } else {
                "PERMISSIVE"
            }
        ),
        Change::SetRowSecurity { to, .. } => format!(
            "~ row security: enabled={}, forced={}",
            to.enabled, to.forced
        ),
        Change::SetReplicaIdentity { to, .. } => match to {
            Some(identity) => format!(
                "~ replica identity -> {identity}",
                identity = identity.shown(),
            ),
            None => "~ replica identity -> default".to_owned(),
        },
        Change::AddUnique { name, .. } => {
            format!("+ unique constraint {name}", name = name.shown(),)
        }
        Change::DropUnique { name, .. } => {
            format!("- unique constraint {name}", name = name.shown(),)
        }
        Change::AddForeignKey { name, .. } => format!("+ foreign key {name}", name = name.shown(),),
        Change::DropForeignKey { name, .. } => {
            format!("- foreign key {name}", name = name.shown(),)
        }
        Change::AddCheck { name, .. } => format!("+ check constraint {name}", name = name.shown(),),
        Change::DropCheck { name, .. } => {
            format!("- check constraint {name}", name = name.shown(),)
        }
        Change::AddIndex { name, .. } => format!("+ index {name}", name = name.shown(),),
        Change::DropIndex { name, .. } => format!("- index {name}", name = name.shown(),),
        Change::CreateModule { module, .. } => format!("+ create {}", module.kind),
        // "restate", not "alter": the whole definition is sent, which is what
        // `CREATE OR ALTER` does and what the reviewer is approving.
        Change::AlterModule { module, .. } => format!("~ restate {}", module.kind),
        Change::DropModule { kind, .. } => format!("- drop {kind}"),
        // Reference data (ADR-0004). The key identifies the row to a reviewer
        // who has no connection, so it leads every line.
        Change::InsertRow { key, row, .. } => {
            format!(
                "+ row {key} ({} values)",
                row.0.len() + 1,
                key = pbps_model::escape::Escaped(&key.to_string()),
            )
        }
        Change::UpdateRow { key, columns, .. } => {
            // Both ends, because the reviewer at the gate cannot look the old
            // value up — and "sets label" is not something anyone can approve.
            let cells: Vec<String> = columns
                .iter()
                .map(|(c, (from, to))| {
                    format!(
                        "{c} {from} -> {to}",
                        c = c.shown(),
                        to = pbps_model::escape::Escaped(&to.to_string()),
                        from = pbps_model::escape::Escaped(&from.to_string()),
                    )
                })
                .collect();
            format!(
                "~ row {key}: {}",
                cells.join(", "),
                key = pbps_model::escape::Escaped(&key.to_string()),
            )
        }
        Change::DeleteRow { key, cause, .. } => match cause {
            DeleteCause::Undeclared => format!(
                "- row {key} (not declared)",
                key = pbps_model::escape::Escaped(&key.to_string()),
            ),
            DeleteCause::KeyChanged => format!(
                "- row {key} (key changed)",
                key = pbps_model::escape::Escaped(&key.to_string()),
            ),
        },
        Change::SetDataMode { to, .. } => match to {
            Some(m) => format!("~ reference data is now `{m}`"),
            None => "~ reference data is no longer declared".to_owned(),
        },
        Change::SetEnsureKeys { from, to, .. } => format!(
            "~ ensure keys: manage [{}]; stop managing [{}]",
            to.difference(from)
                .map(|key| Escaped(&key.to_string()).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            from.difference(to)
                .map(|key| Escaped(&key.to_string()).to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        // Roles (ADR-0005). The permissions are spelled out: "grant on
        // dbo.customer" tells a reviewer nothing about how much wider access
        // just got.
        Change::CreateRole { name, .. } => format!("+ create role {name}", name = name.shown(),),
        Change::DropRole { name, members, .. } if members.is_empty() => {
            format!("- drop role {name}", name = name.shown(),)
        }
        Change::DropRole { name, members, .. } => format!(
            "- drop role {name}, removing {} member(s) first: {}",
            members.len(),
            members
                .iter()
                .map(|n| n.shown().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            name = name.shown(),
        ),
        Change::RenameRole { from, to, .. } => format!(
            "~ rename role {from} -> {to}",
            from = from.shown(),
            to = to.shown(),
        ),
        Change::Grant {
            target,
            permissions,
            ..
        } => format!(
            "+ grant {} on {target}",
            permissions_list(permissions),
            target = target.shown(),
        ),
        Change::Revoke {
            target,
            permissions,
            ..
        } => format!(
            "- revoke {} on {target}",
            permissions_list(permissions),
            target = target.shown(),
        ),
        Change::PublicExecution {
            routine,
            access: pbps_model::PublicAccess::Revoked,
            ..
        } => format!(
            "- revoke execute on {routine} from PUBLIC",
            routine = routine.shown()
        ),
        // A statement of its own, because the engine's default is not
        // something to take on trust (DECISIONS 517) — and worth a reader's
        // eye either way: it says this routine stays runnable by every
        // principal in the cluster.
        Change::PublicExecution {
            routine,
            access: pbps_model::PublicAccess::Kept,
            ..
        } => format!(
            "+ grant execute on {routine} to PUBLIC (declared)",
            routine = routine.shown()
        ),
    }
}

fn permissions_list(p: &std::collections::BTreeSet<pbps_model::Permission>) -> String {
    p.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
}

/// One argument, quoted so that pasting it passes the value through unchanged —
/// or `None` when no spelling can promise that.
///
/// # Why there is a `None`
///
/// This line is read in POSIX shells, PowerShell and `cmd`, and their quoting
/// rules do not overlap enough to cover everything:
///
/// - POSIX single quotes are literal, but **`cmd` does not treat `'` as quoting
///   at all**, so `&`, `|`, `<` and `>` stay live inside them. An earlier
///   version of this function used single quotes for exactly those characters
///   and claimed it failed safe in `cmd`; it does not — `cmd` would split the
///   command at the `&` and run the remainder.
/// - Double quotes are understood by all three for *splitting*, but POSIX
///   shells and PowerShell still expand `$` and a backtick inside them, and a
///   POSIX shell still reads `\\` as an escape inside them.
///
/// So there is no single string that is safe everywhere for a value containing
/// both families. Rather than pick a form that is wrong on one platform, this
/// returns `None` and the caller prints the path on a line of its own, where
/// nothing can execute it. A command that cannot be pasted blindly is a much
/// smaller problem than one that redirects or runs something when it is.
pub fn shell_arg(value: &str) -> Option<String> {
    // Preserve a single backslash inside shell quotes, but refuse every other
    // character Escaped rewrites: an inert display spelling is a different
    // argument, and emitting the original control character is not safe.
    let without_backslashes = value.replace('\\', "");
    if Escaped(&without_backslashes).to_string() != without_backslashes {
        return None;
    }
    // `~` is safe away from the front: it means home-directory expansion as the
    // first character of a word and nothing at all elsewhere, and every Windows
    // short path is full of it (`C:\Users\RUNNER~1\...`).
    //
    // `\` is deliberately *not* in this set, even though the same Windows paths
    // are full of it too. Bare is the one form a POSIX shell reads the
    // backslashes in: `C:\Users\RUNNER~1\plan.json` pasted unquoted arrives as
    // `C:UsersRUNNER~1plan.json`. It was added here to stop Windows paths being
    // quoted, which had the direction backwards — those are exactly the values
    // that need the quotes.
    //
    // `@` is the same shape as `~`, one shell further out: in PowerShell a token
    // *beginning* `@` in argument position is splatting, so `@args` would be
    // replaced by the current argument array rather than passed as the string
    // it is. Away from the front it means nothing, and `deploy@prod` is a
    // perfectly ordinary environment name — so the front is where it is
    // refused, and quoting it below makes PowerShell read it literally.
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:@+=~".contains(c);
    if !value.is_empty() && !value.starts_with(['~', '@', '-']) && value.chars().all(bare) {
        return Some(value.to_owned());
    }

    // A leading `-` is refused outright rather than quoted, because quoting
    // does not help: the shell strips the quotes and clap still receives an
    // argument beginning `-` and reads it as a flag — measured, not assumed
    // (`pbps explain --plan -plan.json` answers "unexpected argument '-p'").
    //
    // `--plan=-plan.json` *does* work, and emitting every option in that form
    // would carry these values. Not taken: it changes the shape of every
    // command this module advertises, and every test that reads one, to buy an
    // environment name or plan path beginning with a hyphen. The placeholder is
    // the established answer for a value that cannot be pasted, and this is one.
    if value.starts_with('-') {
        return None;
    }

    // Double quotes hold for a value a shell would only *split* — a space, most
    // often. They do not neutralize expansion (`$`, a backtick), a quote of the
    // same kind, a newline, or a trailing backslash, which would escape the
    // closing quote itself — and a Windows directory path ends with one more
    // often than not.
    //
    // A doubled backslash is refused for the same reason: inside POSIX double
    // quotes it collapses to a single one, which silently rewrites the leading
    // pair of a UNC path into a value that is still a valid path, just a
    // different one.
    let expands =
        value.contains(['$', '`', '"', '\n']) || value.ends_with('\\') || value.contains("\\\\");
    // Live in `cmd` whatever they are wrapped in, since `cmd` has no literal
    // quote character to wrap them in.
    //
    // `!` is in the list for a narrower reason: it is inert in a default `cmd`,
    // but under `setlocal enabledelayedexpansion` it expands *inside* double
    // quotes, so a path or environment name containing `!NAME!` would silently
    // become a different value on the one shell where this is hardest to
    // notice. Whether delayed expansion is on is not knowable from here, so the
    // safe reading is that it might be.
    let cmd_metacharacters = value.contains(['&', '|', '<', '>', '^', '%', '!']);
    if expands || cmd_metacharacters {
        return None;
    }
    Some(format!("\"{value}\""))
}

/// A placeholder the reader replaces before pasting: `"<plan path>"`.
///
/// Bare, `<plan path>` is a redirection in every shell this tool's remedies
/// are pasted into, and it lands: `pbps apply --plan <plan.json> --checksum
/// <approved-checksum>` creates a file called `--checksum` the moment a
/// `plan.json` exists beside it (measured with bash 5). Double quotes rather
/// than single: `cmd` has no literal-quote character, so `<` stays live
/// inside `'...'` there (PITFALLS, "`shell_arg` has been wrong about shells
/// five times", row 1), while `"..."` is one argument in POSIX shells,
/// PowerShell and `cmd` alike — and a placeholder carries none of the
/// characters double quotes fail on (`$`, a backtick, `\`, `%`, `!`).
/// One spelling, here, for every remedy: quoting each site by hand is how
/// the bare form spread.
pub fn placeholder(name: &str) -> String {
    format!("\"<{name}>\"")
}

/// `true` when `text` carries a placeholder a shell would read as a
/// redirection: a `<` that is not the first character inside double quotes.
/// A test helper, so every command's remedies can be checked the same way.
#[cfg(test)]
pub(crate) fn has_bare_placeholder(text: &str) -> bool {
    let mut previous = None;
    for c in text.chars() {
        if c == '<' && previous != Some('"') {
            return true;
        }
        previous = Some(c);
    }
    false
}

/// A `--env` argument for a copy-pastable remedy.
///
/// Environment names are YAML map keys, so `US West` is a perfectly valid one —
/// and interpolated verbatim it becomes two arguments. Where no cross-shell
/// spelling exists the caller gets `None` and should fall back to a placeholder;
/// a remedy that changes meaning when pasted is worse than one that has to be
/// completed by hand.
pub fn env_arg(name: &str) -> String {
    shell_arg(name).unwrap_or_else(|| placeholder("environment"))
}

/// A value source as a numbering change's line names it (#1889), with the
/// options it sets beside the engine's (#2015, DEC-1921.1): an options-only
/// retune, such as a maximum lowered, is what its `numbering` approval is
/// asked for, and both sides read alike without them.
fn numbering_source(source: &pbps_model::ValueSource) -> String {
    use pbps_model::ValueSource;
    match source {
        ValueSource::None => "nothing".to_owned(),
        ValueSource::Default(text) => format!(
            "the default {text}",
            text = pbps_model::escape::Escaped(text),
        ),
        ValueSource::Identity(id) => format!(
            "an identity (start {}, increment {}{})",
            id.seed,
            id.increment,
            sequence_options(&id.options)
        ),
        ValueSource::Sequence(sequence) => format!(
            "{} (start {}, increment {}{})",
            sequence.name.as_ref().map_or_else(
                || "its own sequence".to_owned(),
                |n| format!("its sequence {n}", n = pbps_model::escape::Escaped(n),)
            ),
            sequence.start,
            sequence.increment,
            sequence_options(&sequence.options)
        ),
        ValueSource::Generated(generated) => {
            format!("the expression {}", Escaped(&generated.expression))
        }
    }
}

/// The options a sequence sets beside the engine's, each after a comma;
/// nothing for the engine's own.
fn sequence_options(options: &pbps_model::SequenceOptions) -> String {
    let mut out = String::new();
    if let Some(min) = options.min {
        out.push_str(&format!(", min {min}"));
    }
    if let Some(max) = options.max {
        out.push_str(&format!(", max {max}"));
    }
    if options.cycle {
        out.push_str(", cycle");
    }
    if let Some(cache) = options.cache {
        out.push_str(&format!(", cache {cache}"));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn each_drift_subject_has_the_same_three_ways_out_and_clean_reports_have_none() {
        let mut report = pbps_model::DriftReport {
            version: pbps_model::drift::CURRENT_VERSION,
            environment: "prod".into(),
            checked_at: "2026-10-24".into(),
            baseline: pbps_model::DriftBaseline {
                entry_id: 1,
                applied_at: "2026-10-24".into(),
                checksum: "before".into(),
            },
            live_checksum: "after".into(),
            changes: set(vec![
                Change::DropTable {
                    uid: uid(UidKind::Table, "a"),
                    name: "app.a".parse().unwrap(),
                    detach_from: None,
                },
                Change::DropTable {
                    uid: uid(UidKind::Table, "b"),
                    name: "app.b".parse().unwrap(),
                    detach_from: None,
                },
            ]),
            unmanaged: Vec::new(),
            unexpressible: vec!["app.c: unsupported difference".into()],
        };
        let text = drift(&report);
        assert!(report.has_drift());
        for command in [
            "`pbps pull`",
            "`pbps plan --db`",
            "`pbps apply --plan`",
            "`pbps baseline --reason …`",
        ] {
            assert!(text.contains(command), "{text}");
        }
        assert_eq!(text.matches(DRIFT_REMEDY).count(), 3, "{text}");
        for subject in ["app.a", "app.b", "app.c: unsupported difference"] {
            let after = text.split_once(subject).unwrap().1;
            assert!(
                after.lines().nth(1).unwrap().contains(DRIFT_REMEDY),
                "{text}"
            );
        }
        assert!(!changes(&report.changes).contains(DRIFT_REMEDY));
        report.changes = ChangeSet::default();
        report.unexpressible.clear();
        report.live_checksum = report.baseline.checksum.clone();
        assert!(!report.has_drift());
        let clean = drift(&report);
        assert!(clean.contains("No drift"));
        assert!(!clean.contains("Three ways out"));
    }

    #[test]
    fn enum_label_lines_show_placement_and_escaped_rename_without_inventing_a_neighbour() {
        use pbps_model::{Change, EnumPlacement, ManagedType};
        let name: pbps_model::TypeRef = "app.e".parse().unwrap();
        let uid: pbps_model::Uid = "y_000001".parse().unwrap();
        let definition = Box::new(ManagedType::Enum {
            values: vec!["new\nline".into()],
        });
        let add = |placement| Change::AddEnumValue {
            uid: uid.clone(),
            name: name.clone(),
            value: "new\nline".into(),
            placement,
            ordinal: 0,
            definition: definition.clone(),
        };
        assert_eq!(
            super::describe(&add(Some(EnumPlacement::Before("old".into())))),
            "+ add enum value new\\nline to app.e before old"
        );
        assert!(!super::describe(&add(None)).contains("before"));
        assert_eq!(
            super::describe(&Change::RenameEnumValue {
                uid,
                name,
                from: "old".into(),
                to: "new\nline".into(),
                ordinal: 0,
                definition
            }),
            "~ rename enum value old -> new\\nline in app.e"
        );
    }

    /// DEC-1635.8: each half of a domain alteration names what it does, and
    /// a check's name is escaped like any other name in the line.
    #[test]
    fn a_domain_alterations_halves_say_what_they_drop_add_and_set() {
        use pbps_model::DomainHalf::{Loosen, Tighten};
        let domain = |not_null: bool, default: Option<&str>, checks: &[(&str, &str)]| {
            pbps_model::ManagedType::Domain {
                base: "text".parse().unwrap(),
                not_null,
                default: default.map(str::to_owned),
                checks: checks
                    .iter()
                    .map(|(n, t)| ((*n).into(), (*t).to_owned()))
                    .collect(),
                collation: None,
            }
        };
        let alter = |from, to, half| Change::AlterDomain {
            uid: "y_000001".parse().unwrap(),
            name: "app.code".parse().unwrap(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        let from = domain(true, Some("'a'"), &[("old\nname", "1"), ("kept", "2")]);
        let to = domain(false, None, &[("kept", "2"), ("fresh", "3")]);
        assert_eq!(
            super::describe(&alter(from.clone(), to.clone(), Loosen)),
            "~ loosen domain app.code: drop check old\\nname, drop NOT NULL, drop the default"
        );
        assert_eq!(
            super::describe(&alter(to, from, Tighten)),
            "~ tighten domain app.code: set NOT NULL, add check old\\nname, validated over every existing row"
        );
    }

    /// A rebuild's lines name the replacement, the labels that go, and for
    /// each retype the table it rewrites and the lock it takes (DEC-1635.7).
    #[test]
    fn enum_rebuild_lines_name_the_rewritten_table_the_lock_and_the_removed_labels() {
        use pbps_model::{Change, ColumnRef, EnumRebuildStep, ManagedType};
        let step = |step| Change::EnumRebuild {
            uid: "y_000001".parse().unwrap(),
            name: "app.e".parse().unwrap(),
            tmp: "app.tmp".parse().unwrap(),
            step,
        };
        let definition = Box::new(ManagedType::Enum {
            values: vec!["ok".into()],
        });
        let create = super::describe(&step(EnumRebuildStep::Create {
            definition: definition.clone(),
            removed: vec!["sad\nline".into()],
            ordinal: 0,
            over: None,
            why: None,
        }));
        assert!(
            create.contains("rebuild enum app.e as app.tmp")
                && create.contains("removes sad\\nline"),
            "{create}"
        );
        // A pure reorder removes nothing and says nothing about removal.
        assert!(
            !super::describe(&step(EnumRebuildStep::Create {
                definition: definition.clone(),
                removed: vec![],
                ordinal: 0,
                over: None,
                why: None,
            }))
            .contains("removes")
        );
        let retype = super::describe(&step(EnumRebuildStep::Retype {
            column_uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "m", 0),
            column: ColumnRef {
                table: "app.t".parse().unwrap(),
                name: "m".into(),
            },
            to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
            array: false,
            values: Box::new(pbps_model::ValueChange::Relabel { removed: vec![] }),
        }));
        assert!(
            retype.contains("app.t.m")
                && retype.contains("rewrites app.t")
                && retype.contains("ACCESS EXCLUSIVE")
                && retype.contains("full table rewrite"),
            "{retype}"
        );
        let swap = |comment: Option<&str>| {
            super::describe(&step(EnumRebuildStep::Swap {
                definition: definition.clone(),
                comment: comment.map(str::to_owned),
                depth: 0,
                over: None,
            }))
        };
        assert!(swap(Some("c")).contains("keeping its comment"));
        assert!(!swap(None).contains("comment"));
    }

    /// A domain's rebuild names the kind and the reason it is rebuilt; an
    /// enum's, which has none, reads as before (DEC-1635.9).
    #[test]
    fn domain_rebuild_lines_say_why_the_domain_is_rebuilt() {
        use pbps_model::{Change, EnumRebuildStep, ManagedType};
        let create = |why: Option<&str>| {
            super::describe(&Change::EnumRebuild {
                uid: "y_000002".parse().unwrap(),
                name: "app.d".parse().unwrap(),
                tmp: "app.tmp".parse().unwrap(),
                step: EnumRebuildStep::Create {
                    definition: Box::new(ManagedType::Domain {
                        base: "text".parse().unwrap(),
                        not_null: false,
                        default: None,
                        checks: Default::default(),
                        collation: None,
                    }),
                    removed: vec![],
                    ordinal: 0,
                    over: None,
                    why: why.map(str::to_owned),
                },
            })
        };
        let line = create(Some("its base type changed"));
        assert!(
            line.contains("rebuild domain app.d as app.tmp")
                && line.contains("its base type changed"),
            "{line}"
        );
        assert!(!create(None).contains("its base type"));
        assert!(!create(None).contains("enum"));
    }

    /// A numbering change's line shows the options each side sets, so an
    /// options-only retune reads as the change it is (#2015). The negative
    /// case: a retune of the start alone shows no options.
    #[test]
    fn a_numbering_line_shows_the_options_each_side_sets() {
        use pbps_model::{Change, ColumnRef, Identity, SequenceOptions, ValueSource};
        let identity = |seed, options| {
            ValueSource::Identity(Identity {
                seed,
                increment: 1,
                options,
            })
        };
        let line = |from, to| {
            super::describe(&Change::AlterColumnNumbering {
                uid: "c_aaaaaa".parse().unwrap(),
                column: ColumnRef {
                    table: "app.t".parse().unwrap(),
                    name: "id".into(),
                },
                ty: "integer".parse().unwrap(),
                from,
                to,
            })
        };
        let bounded = |max| SequenceOptions {
            max: Some(max),
            ..Default::default()
        };
        assert_eq!(
            line(identity(1, bounded(1000)), identity(1, bounded(100))),
            "~ id numbered by an identity (start 1, increment 1, max 1000) -> an identity \
             (start 1, increment 1, max 100)"
        );
        let every = SequenceOptions {
            min: Some(-5),
            max: Some(9),
            cycle: true,
            cache: Some(20),
        };
        assert!(
            line(identity(1, Default::default()), identity(1, every))
                .ends_with("(start 1, increment 1, min -5, max 9, cycle, cache 20)")
        );
        assert_eq!(
            line(
                identity(1, Default::default()),
                identity(5, Default::default())
            ),
            "~ id numbered by an identity (start 1, increment 1) -> an identity (start 5, \
             increment 1)"
        );
    }

    /// A step a rule added names its origin under it, escaped like every
    /// other text out of the plan; a step without one, as every step of a
    /// plan read back from its file, shows none (DEC-1958.3).
    #[test]
    fn an_added_step_names_its_origin_and_a_plain_one_names_none() {
        use pbps_model::{AddedRole, Change, ChangeSet, PlannedChange, Provenance};
        let drop = || {
            PlannedChange::new(Change::DropCheck {
                table: "app.t".parse().unwrap(),
                name: "ck".into(),
            })
        };
        let mut added = drop();
        added.provenance = Some(Provenance {
            rule: "weave",
            role: AddedRole::Teardown,
            source: "app.f(integer)".into(),
            reason: format!("ck depends on {}", Escaped("it\n    + forged line")),
        });
        let cs = ChangeSet {
            changes: vec![drop(), added],
        };
        let text = super::changes(&cs);
        assert_eq!(text.matches("added by").count(), 1, "{text}");
        assert!(
            text.contains("      added by weave: teardown for app.f(integer) — ck depends on it"),
            "{text}"
        );
        assert!(!text.contains("\n    + forged line"), "{text}");
        let origins = super::origins(&cs);
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].step, 1);
        assert_eq!(origins[0].role, "teardown");
        // Not part of what the plan is: equal with or without it.
        assert_eq!(cs.changes[0], cs.changes[1]);
    }

    /// A partition is described by its parent and the rows it takes, not by
    /// the columns it has none of (#1170); an ordinary table by its columns.
    #[test]
    fn a_created_partition_is_described_by_its_parent_and_bound() {
        use pbps_model::{BoundDatum as D, Change, PartitionBound as B};
        let create = |bound: Option<B>| Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.p1".parse().unwrap(),
            table: Box::new(pbps_model::Table {
                partition_of: bound.map(|bound| pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound,
                    columns: Default::default(),
                }),
                ..Default::default()
            }),
            beside_default: None,
            moves_at_most: None,
        };
        assert_eq!(
            super::describe(&create(Some(B::Range {
                from: vec![D::MinValue, D::Value("2025-01-01".into())],
                to: vec![D::Value("it's".into()), D::MaxValue],
            }))),
            "+ create table app.p1, a partition of app.ev for rows from (MINVALUE, \"2025-01-01\") \
             to (\"it's\", MAXVALUE)"
        );
        assert_eq!(
            super::describe(&create(Some(B::Default))),
            "+ create table app.p1, the default partition of app.ev"
        );
        // Beside a standing DEFAULT, it says which rows it takes from where
        // (#1547).
        let Change::CreateTable {
            uid, name, table, ..
        } = create(Some(B::Range {
            from: vec![D::Value("1".into())],
            to: vec![D::Value("2".into())],
        }))
        else {
            unreachable!("a create");
        };
        assert_eq!(
            super::describe(&Change::CreateTable {
                uid,
                name,
                table,
                beside_default: Some("app.ev_rest".parse().unwrap()),
                moves_at_most: None,
            }),
            "+ create table app.p1, a partition of app.ev for rows from (\"1\") to (\"2\"), \
             moving those rows out of the default partition app.ev_rest, none allowed: the \
             apply refuses any it holds"
        );
        // And how many a human allowed it to take (#1788).
        let Change::CreateTable {
            uid, name, table, ..
        } = create(Some(B::Range {
            from: vec![D::Value("1".into())],
            to: vec![D::Value("2".into())],
        }))
        else {
            unreachable!("a create");
        };
        assert_eq!(
            super::describe(&Change::CreateTable {
                uid,
                name,
                table,
                beside_default: Some("app.ev_rest".parse().unwrap()),
                moves_at_most: Some(500),
            }),
            "+ create table app.p1, a partition of app.ev for rows from (\"1\") to (\"2\"), \
             moving those rows out of the default partition app.ev_rest, at most 500 as allowed"
        );
        assert_eq!(
            super::describe(&create(None)),
            "+ create table app.p1 (0 columns)"
        );
        let drop = |detach_from: Option<&str>| Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.p1".parse().unwrap(),
            detach_from: detach_from.map(|p| p.parse().unwrap()),
        };
        assert_eq!(
            super::describe(&drop(Some("app.ev"))),
            "- drop table app.p1, a partition of app.ev, detached from it first"
        );
        assert_eq!(super::describe(&drop(None)), "- drop table app.p1");
        let detach = Change::DetachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: "app.p1".parse().unwrap(),
            parent: "app.ev".parse().unwrap(),
            names: vec![
                pbps_model::DetachedName {
                    kind: pbps_model::DetachedKind::Index,
                    parent: "ev_n".into(),
                    name: Some("arch_n".into()),
                },
                pbps_model::DetachedName {
                    kind: pbps_model::DetachedKind::Check,
                    parent: "ev_ck".into(),
                    name: Some("ev_ck".into()),
                },
            ],
            shape: Box::default(),
        };
        assert_eq!(
            super::describe(&detach),
            "~ detach table app.p1 from app.ev, keeping its rows, with 1 name(s) of its own"
        );
        let attach = Change::AttachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            parent: "app.ev".parse().unwrap(),
            bound: pbps_model::PartitionBound::Default,
            key: Vec::new(),
            shape: Box::default(),
        };
        assert_eq!(
            super::describe(&attach),
            "~ attach table app.t to app.ev as its default partition, keeping its rows"
        );
        let ranged = Change::AttachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            parent: "app.ev".parse().unwrap(),
            bound: pbps_model::PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::Value("1".into())],
                to: vec![pbps_model::BoundDatum::Value("2".into())],
            },
            key: Vec::new(),
            shape: Box::default(),
        };
        assert_eq!(
            super::describe(&ranged),
            "~ attach table app.t to app.ev as a partition, keeping its rows"
        );
    }

    #[test]
    fn intent_reports_describe_the_operation_without_inventing_its_source() {
        use pbps_model::Intent;
        let cases = [
            (
                Intent::RenameTable {
                    from: "dbo.old".parse().unwrap(),
                    to: "dbo.new".parse().unwrap(),
                },
                "rename table dbo.old -> dbo.new",
            ),
            (
                Intent::RenameColumn {
                    table: "dbo.t".parse().unwrap(),
                    from: "old".into(),
                    to: "new".into(),
                },
                "rename column dbo.t.old -> dbo.t.new",
            ),
            (
                Intent::RenameRole {
                    from: "old".into(),
                    to: "new".into(),
                },
                "rename role old -> new",
            ),
            (
                Intent::DropTable {
                    table: "dbo.t".parse().unwrap(),
                    reason: "retired".parse().unwrap(),
                },
                "drop table dbo.t (reason: retired)",
            ),
            (
                Intent::DropColumn {
                    column: "dbo.t.old".parse().unwrap(),
                    reason: "retired".parse().unwrap(),
                },
                "drop column dbo.t.old (reason: retired)",
            ),
            (
                Intent::DropRole {
                    role: "old".into(),
                    reason: "retired".parse().unwrap(),
                },
                "drop role old (reason: retired)",
            ),
        ];
        for (intent, expected) in cases {
            assert_eq!(super::intent(&intent), expected);
            let blocker = pbps_diff::Blocker::UnusedIntent { intent };
            let text = super::blockers(std::slice::from_ref(&blocker));
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains("renamed_from"), "{text}");
            for source in ["annotation", "command", "prompt answer"] {
                assert!(text.contains(source), "{text}");
            }
            let finding = super::blocker_finding(&blocker);
            assert_eq!(finding.id, "identity.unused-intent");
            assert!(finding.remedy.unwrap().contains("retry"));
        }
    }

    #[test]
    fn a_prompt_answer_is_reported_as_an_operation_and_can_be_corrected() {
        let answers = crate::prompt::ask_from(
            &[pbps_diff::Blocker::AmbiguousColumns {
                table: "dbo.t".parse().unwrap(),
                disappeared: vec!["old".into()],
                appeared: vec!["new".into()],
            }],
            std::io::Cursor::new("1\n"),
        )
        .unwrap();
        assert_eq!(answers.len(), 1);
        let text = super::blockers(&[pbps_diff::Blocker::UnusedIntent {
            intent: answers[0].clone(),
        }]);
        assert!(
            text.contains("rename column dbo.t.old -> dbo.t.new"),
            "{text}"
        );
        assert!(!text.contains("renamed_from"), "{text}");
        assert!(text.contains("prompt answer"), "{text}");
    }
    use super::*;
    use pbps_model::change::PlannedChange;
    use pbps_model::{GrantTarget, Permission, Uid, UidKind};

    fn uid(kind: UidKind, seed: &str) -> Uid {
        Uid::derived(kind, seed, 0)
    }

    fn set(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }

    /// A `RenameRole` answers for its old name and the `Grant` that follows
    /// it in the same plan for the new one; counted as they come, one role
    /// read as two in the summary line and in the JSON count.
    #[test]
    fn a_renamed_role_whose_grants_also_change_is_one_role() {
        let cs = set(vec![
            Change::RenameRole {
                uid: uid(UidKind::Role, "a"),
                from: "a".into(),
                to: "b".into(),
            },
            Change::Grant {
                role: "b".into(),
                target: GrantTarget::Object("dbo.t".parse().unwrap()),
                permissions: [Permission::Select].into_iter().collect(),
            },
        ]);
        let after = pbps_model::IdsFile {
            roles: [(uid(UidKind::Role, "a"), "b".into())].into(),
            ..Default::default()
        };
        let names = PlanNames::of(&cs.changes, &after).unwrap();
        assert_eq!(touched_roles(&names), 1);
        assert!(
            summary(&cs, &names).contains("1 role(s)"),
            "{}",
            summary(&cs, &names)
        );
    }

    /// The same shape one namespace over: `RenameTable` answers for the old
    /// name, `AddColumn` on the renamed table for the new one.
    #[test]
    fn a_renamed_table_that_also_changes_is_one_table() {
        let cs = set(vec![
            Change::RenameTable {
                uid: uid(UidKind::Table, "dbo.old"),
                from: "dbo.old".parse().unwrap(),
                to: "dbo.new".parse().unwrap(),
                defaults: Vec::new(),
            },
            Change::AddColumn {
                uid: uid(UidKind::Column, "dbo.new.extra"),
                table: "dbo.new".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
            },
        ]);
        let after = pbps_model::IdsFile {
            tables: [(uid(UidKind::Table, "dbo.old"), "dbo.new".parse().unwrap())].into(),
            columns: [(
                uid(UidKind::Column, "dbo.new.extra"),
                "dbo.new.extra".parse().unwrap(),
            )]
            .into(),
            ..Default::default()
        };
        let names = PlanNames::of(&cs.changes, &after).unwrap();
        assert_eq!(touched(&names), (1, 0));
        assert!(
            summary(&cs, &names).contains("1 table(s)"),
            "{}",
            summary(&cs, &names)
        );
    }

    /// Two roles that are not the two ends of one rename stay two: the
    /// collapse is by the rename's own pair, not by "a rename is present".
    #[test]
    fn a_renamed_role_and_an_unrelated_one_stay_two_roles() {
        let cs = set(vec![
            Change::RenameRole {
                uid: uid(UidKind::Role, "a"),
                from: "a".into(),
                to: "b".into(),
            },
            Change::Grant {
                role: "c".into(),
                target: GrantTarget::Schema("dbo".into()),
                permissions: [Permission::Select].into_iter().collect(),
            },
        ]);
        let after = pbps_model::IdsFile {
            roles: [(uid(UidKind::Role, "a"), "b".into())].into(),
            ..Default::default()
        };
        let names = PlanNames::of(&cs.changes, &after).unwrap();
        assert_eq!(touched_roles(&names), 2);
        assert!(
            summary(&cs, &names).contains("2 role(s)"),
            "{}",
            summary(&cs, &names)
        );
    }

    /// A role and a table can render the same subject string: the role
    /// `app.reader` and the table `[role app].reader` both read
    /// `role app.reader`. Keyed by that string, a plan that renames the role
    /// rewrote the table's key to the rename's far end and counted the two
    /// tables as one.
    #[test]
    fn a_role_rename_does_not_merge_two_tables_that_render_like_it() {
        let cs = set(vec![
            Change::RenameRole {
                uid: uid(UidKind::Role, "app.reader"),
                from: "app.reader".into(),
                to: "new.role".into(),
            },
            Change::AddColumn {
                uid: uid(UidKind::Column, "role app.reader.extra"),
                table: "role app.reader".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
            },
            Change::AddColumn {
                uid: uid(UidKind::Column, "role new.role.extra"),
                table: "role new.role".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
            },
        ]);
        let after = pbps_model::IdsFile {
            roles: [(uid(UidKind::Role, "app.reader"), "new.role".into())].into(),
            tables: ["role app.reader", "role new.role"]
                .into_iter()
                .map(|name| (uid(UidKind::Table, name), name.parse().unwrap()))
                .collect(),
            columns: ["role app.reader.extra", "role new.role.extra"]
                .into_iter()
                .map(|name| (uid(UidKind::Column, name), name.parse().unwrap()))
                .collect(),
            ..Default::default()
        };
        let names = PlanNames::of(&cs.changes, &after).unwrap();
        assert_eq!(touched(&names), (2, 0));
        assert_eq!(touched_roles(&names), 1);
        let summary = summary(&cs, &names);
        assert!(summary.contains("2 table(s)"), "{summary}");
        assert!(summary.contains("1 role(s)"), "{summary}");
    }

    #[test]
    fn a_table_and_role_renamed_twice_are_each_counted_once() {
        let table = uid(UidKind::Table, "a");
        let role = uid(UidKind::Role, "a");
        let cs = set(vec![
            Change::RenameTable {
                uid: table.clone(),
                from: "app.a".parse().unwrap(),
                to: "app.tmp".parse().unwrap(),
                defaults: vec![],
            },
            Change::RenameTable {
                uid: table.clone(),
                from: "app.tmp".parse().unwrap(),
                to: "app.b".parse().unwrap(),
                defaults: vec![],
            },
            Change::RenameRole {
                uid: role.clone(),
                from: "a".into(),
                to: "tmp".into(),
            },
            Change::RenameRole {
                uid: role.clone(),
                from: "tmp".into(),
                to: "b".into(),
            },
        ]);
        let after = pbps_model::IdsFile {
            tables: [(table, "app.b".parse().unwrap())].into(),
            roles: [(role, "b".into())].into(),
            ..Default::default()
        };
        let names = PlanNames::of(&cs.changes, &after).unwrap();
        assert_eq!(touched(&names), (1, 0));
        assert_eq!(touched_roles(&names), 1);
        assert!(summary(&cs, &names).contains("1 table(s) and 1 role(s)"));
    }

    /// The placeholder is one literal argument in POSIX shells, PowerShell
    /// and `cmd`; bare, `<plan.json>` is a redirection that creates a file
    /// named after the next option the moment its input exists.
    #[test]
    fn a_placeholder_is_one_quoted_argument_and_never_a_redirection() {
        assert_eq!(placeholder("plan path"), "\"<plan path>\"");
        assert!(!has_bare_placeholder(&placeholder("approved-checksum")));
        assert!(has_bare_placeholder("--plan <plan.json>"));
        assert!(has_bare_placeholder("--checksum '<approved-checksum>'"));
        assert!(!has_bare_placeholder(
            "--plan \"<plan.json>\" --checksum \"<x>\""
        ));
        // `env_arg`'s fallback is the same spelling: a name no shell can
        // carry becomes a quoted placeholder, not a redirection.
        assert_eq!(env_arg("prod&rm"), "\"<environment>\"");
        // And every drop remedy a blocker prints carries its `--reason`
        // placeholder quoted.
        let text = one_blocker(&Blocker::DropTableNeedsReason {
            table: "dbo.t".parse().unwrap(),
        });
        assert!(text.contains("--reason \"\""), "{text}");
        assert!(!has_bare_placeholder(&text), "{text}");
    }

    /// A conflict names every intent that caused it, not just the count.
    ///
    /// The user's next act is to delete one of them from a file, and they can
    /// only do that if they are told which ones are contending. A message that
    /// said "two intents conflict" would send them to read the whole tree.
    #[test]
    fn a_rename_conflict_lists_the_intents_that_caused_it() {
        let claim = |to: &str| Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: to.into(),
        };
        let text = one_blocker(&Blocker::ConflictingRenameIntents {
            side: RenameSide::Source,
            name: "dbo.t.old".into(),
            intents: vec![claim("aaa"), claim("zzz")],
        });
        assert!(
            text.contains("rename column dbo.t.old -> dbo.t.aaa"),
            "{text}"
        );
        assert!(
            text.contains("rename column dbo.t.old -> dbo.t.zzz"),
            "{text}"
        );

        // And the finding splits into a message and a remedy, like every other
        // blocker: a caller reading JSON gets the same two halves the human
        // view prints.
        let f = blocker_finding(&Blocker::ConflictingRenameIntents {
            side: RenameSide::Target,
            name: "dbo.t.new".into(),
            intents: vec![claim("new"), claim("new")],
        });
        assert_eq!(f.id, "identity.conflicting-rename-intents");
        assert!(
            f.remedy.as_deref().is_some_and(|r| r.contains("keep one")),
            "{f:?}"
        );
    }

    #[test]
    fn provisional_blockers_keep_their_finding_id_and_render_the_assumption_in_human_and_json() {
        let conflict = Blocker::ConflictingRenameIntents {
            side: RenameSide::Source,
            name: "dbo.a.x".into(),
            intents: ["p", "q"]
                .map(|to| Intent::RenameColumn {
                    table: "dbo.a".parse().unwrap(),
                    from: "x".into(),
                    to: to.into(),
                })
                .to_vec(),
        };
        let ordinary = blocker_finding(&conflict);
        assert!(!ordinary.message.contains("provisional"));
        assert!(!one_blocker(&conflict).contains("if dbo.old"));
        let derived = Blocker::ProvisionalTableIdentity {
            from: "dbo.old".parse().unwrap(),
            to: "dbo.a".parse().unwrap(),
            blocker: Box::new(conflict),
        };
        let text = one_blocker(&derived);
        assert!(
            text.contains("if dbo.old -> dbo.a is what you meant"),
            "{text}"
        );
        assert!(
            text.contains("dbo.a.p") && text.contains("dbo.a.q"),
            "{text}"
        );
        let finding = blocker_finding(&derived);
        assert_eq!(finding.id, ordinary.id);
        assert_eq!(finding.remedy, ordinary.remedy);
        let json = serde_json::to_value(&finding).unwrap();
        assert_eq!(json["id"], "identity.conflicting-rename-intents");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("provisional table identity")
        );
    }

    /// Three claimants need two deletions, and the remedy has to say so.
    ///
    /// Nothing stops a third column carrying `renamed_from: old`. Told to drop
    /// "the other", a user removes one, runs again and meets the same blocker
    /// — a remedy that has to be applied twice without saying it is one that
    /// reads as not having worked.
    #[test]
    fn a_rename_conflict_asks_for_every_extra_claimant() {
        let claim = |to: &str| Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: to.into(),
        };
        let text = one_blocker(&Blocker::ConflictingRenameIntents {
            side: RenameSide::Source,
            name: "dbo.t.old".into(),
            intents: vec![claim("aaa"), claim("mmm"), claim("zzz")],
        });
        assert!(text.contains("3 intents"), "{text}");
        assert!(
            !text.contains("drop the other"),
            "with three claimants, dropping one leaves two: {text}"
        );
        assert!(text.contains("drop the rest"), "{text}");
    }

    /// The remedy does not say where the contradiction lives, because it does
    /// not know.
    ///
    /// One claimant can be a `renamed_from:` line and the other the
    /// `pbps rename` being run right now — `cmd_intent` appends the
    /// command-line intent to the loaded ones and resolves them together — so
    /// a remedy that says "edit the declarations" is advice to delete the
    /// annotation that is right, which turns the next plan into an ambiguity.
    #[test]
    fn a_rename_conflict_does_not_claim_both_claimants_are_in_the_files() {
        let claim = |to: &str| Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: to.into(),
        };
        let text = one_blocker(&Blocker::ConflictingRenameIntents {
            side: RenameSide::Source,
            name: "dbo.t.old".into(),
            intents: vec![claim("aaa"), claim("zzz")],
        });
        assert!(
            !text.contains("edit the declarations"),
            "the remedy must not assume both claimants are annotations: {text}"
        );
        assert!(text.contains("pbps rename"), "{text}");
        assert!(text.contains("renamed_from"), "{text}");
    }

    /// An occupied rename target gets a precise finding rather than the
    /// generic unused-intent wording, and the target is named for the user.
    #[test]
    fn a_rename_target_collision_names_the_declared_target() {
        let blocker = Blocker::RenameTargetExists {
            target: "dbo.customer.full_name".into(),
        };
        let text = one_blocker(&blocker);
        assert!(
            text.contains("rename target dbo.customer.full_name"),
            "{text}"
        );
        assert!(
            text.contains("already exists in the declarations"),
            "{text}"
        );
        assert!(text.contains("keep the existing declaration"), "{text}");
        assert!(
            text.contains("remove or correct the `renamed_from:` annotation or rename intent"),
            "{text}"
        );
        assert!(text.contains("reuse that name for the source"), "{text}");
        // And the source the keep branch may leave behind (#554), unless
        // another rename already accounts for it.
        assert!(
            text.contains(
                "unless another rename or a recorded drop already accounts for the source, decide what becomes of it"
            ),
            "{text}"
        );
        for choice in [
            "declare it again",
            "record its drop with a reason",
            "rename it to a name nothing holds",
        ] {
            assert!(text.contains(choice), "{choice}: {text}");
        }
        assert!(
            text.contains("free the occupied target in an earlier revision"),
            "{text}"
        );
        assert!(!text.contains("remove the existing declaration"), "{text}");

        let finding = blocker_finding(&blocker);
        assert_eq!(finding.id, "identity.rename-target-exists");
        assert!(
            finding.message.contains("dbo.customer.full_name"),
            "{finding:?}"
        );
        assert!(!finding.message.contains("likely a typo"), "{finding:?}");
    }

    #[test]
    fn a_rename_target_collision_keeps_guidance_in_the_remedy() {
        for target in ["dbo.customer", "dbo.customer.full_name", "reader"] {
            let finding = blocker_finding(&Blocker::RenameTargetExists {
                target: target.into(),
            });
            assert_eq!(finding.id, "identity.rename-target-exists");
            assert_eq!(
                finding.message,
                format!("rename target {target} already exists in the declarations")
            );
            let remedy = finding
                .remedy
                .as_deref()
                .expect("occupied-target guidance must have a dedicated remedy");
            assert!(!remedy.is_empty());
            assert!(!remedy.contains("already exists in the declarations"));
        }
    }

    /// A column pbps cannot mint an identity for (issue #108) must name the
    /// column and the table, not just count as one more blocker — this is the
    /// one blocker `pull` can hit on an otherwise empty identity file, so its
    /// text is the only thing standing between the user and a bare "1
    /// blocker(s)".
    #[test]
    fn an_unrepresentable_column_names_the_table_and_the_part() {
        let blocker = Blocker::UnrepresentableName {
            what: "column",
            part: "a.b".into(),
            table: Some("dbo.customer".parse().unwrap()),
        };
        let text = one_blocker(&blocker);
        assert!(text.contains("dbo.customer"), "{text}");
        assert!(text.contains("`a.b`"), "{text}");

        let finding = blocker_finding(&blocker);
        assert_eq!(finding.id, "identity.unrepresentable-name");
        assert!(finding.message.contains("a.b"), "{finding:?}");
    }

    #[test]
    fn missing_type_diagnostics_distinguish_absence_from_unrepresentability() {
        for detail in [None, Some("domain base app._mood is unsupported".into())] {
            let blocker = Blocker::MissingType {
                name: "app.mood".parse().unwrap(),
                unrepresentable: detail.clone(),
            };
            let finding = blocker_finding(&blocker);
            assert_eq!(finding.id, "identity.missing-type");
            assert!(finding.message.contains("app.mood"), "{finding:?}");
            assert!(finding.message.contains("pull cannot record a drop"));
            assert_eq!(finding.message.contains("is absent"), detail.is_none());
            assert_eq!(
                finding.message.contains("is unrepresentable"),
                detail.is_some()
            );
            if let Some(detail) = detail {
                assert!(finding.message.contains(&detail));
            }
            assert!(finding.remedy.is_none());
            assert!(!blockers(&[blocker]).contains("pbps drop"));
        }
    }

    /// The table-level case must not claim to be about a column.
    #[test]
    fn an_unrepresentable_table_name_does_not_mention_a_column() {
        let blocker = Blocker::UnrepresentableName {
            what: "table",
            part: "a.b".into(),
            table: None,
        };
        let text = one_blocker(&blocker);
        assert!(text.contains("table `a.b`"), "{text}");
        // The general "schema, table and column" explanation of the
        // separator is fine here; claiming *this* name is a column is not.
        assert!(!text.contains("mint an identity for this column"), "{text}");
    }

    #[test]
    fn type_descriptions_show_each_type_once_in_every_format() {
        use pbps_model::{ColumnType, TypeRef};
        let raw = ColumnType::reference(TypeRef::new("app", "type\n\\\u{1b}\"名前").unwrap());
        let visible = ColumnType::reference(TypeRef::new("app", r#"type\n\\\u{1b}"名前"#).unwrap());
        let raw_text = "app.\"type\n\\\u{1b}\"\"名前\"";
        let visible_text = r#"app."type\n\\\u{1b}""名前""#;
        let change = Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: TableName::new("app", "t").column("v"),
            from: raw,
            to: visible,
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        };
        assert_eq!(
            describe(&change),
            format!(
                "~ v type {} -> {}",
                Escaped(raw_text),
                Escaped(visible_text)
            )
        );
        assert_ne!(describe(&change), Escaped(&describe(&change)).to_string());
    }

    #[test]
    fn merged_type_lifecycle_descriptions_show_names_once_and_keep_raw_identities() {
        use pbps_model::{Change, ManagedType, TypeRef};
        for (name, shown) in [
            (
                "type\n\u{1b}[31m\u{202e}",
                r#"app."type\n\u{1b}[31m\u{202e}""#,
            ),
            (
                r"type\n\u{1b}[31m\u{202e}",
                r#"app."type\\n\\u{1b}[31m\\u{202e}""#,
            ),
            ("名前_ünï", "app.\"名前_ünï\""),
            ("ordinary", "app.ordinary"),
        ] {
            let from = TypeRef::new("app", name).unwrap();
            for definition in [
                ManagedType::Enum {
                    values: vec!["ok".into()],
                },
                ManagedType::Domain {
                    base: "integer".parse().unwrap(),
                    not_null: false,
                    default: None,
                    checks: Default::default(),
                    collation: None,
                },
            ] {
                let uid: pbps_model::Uid = "y_aaaaaa".parse().unwrap();
                let to = TypeRef::new("other", name).unwrap();
                let target_shown = shown.replacen("app.", "other.", 1);
                for (change, expected, key) in [
                    (
                        Change::CreateType {
                            uid: uid.clone(),
                            name: from.clone(),
                            definition: Box::new(definition.clone()),
                        },
                        format!("+ create {} {shown}", definition.kind()),
                        "name",
                    ),
                    (
                        Change::DropType {
                            uid: uid.clone(),
                            name: from.clone(),
                            definition: Box::new(definition.clone()),
                        },
                        format!("- drop {} {shown}", definition.kind()),
                        "name",
                    ),
                    (
                        Change::RenameType {
                            uid,
                            from: from.clone(),
                            to,
                            definition: Box::new(definition.clone()),
                        },
                        format!("~ rename type {shown} -> {target_shown}"),
                        "from",
                    ),
                ] {
                    assert_eq!(super::describe(&change), expected);
                    assert_eq!(
                        serde_json::to_value(&change).unwrap()[key],
                        serde_json::to_value(&from).unwrap()
                    );
                    assert_eq!(from.name(), name);
                }
            }
        }
    }

    #[test]
    fn pending_rebuild_descriptions_show_subjects_once_and_keep_raw_identities() {
        use pbps_model::*;
        for (name, shown) in [
            ("table\n\u{1b}[31m\u{202e}", r"table\n\u{1b}[31m\u{202e}"),
            (
                r"table\n\u{1b}[31m\u{202e}",
                r"table\\n\\u{1b}[31m\\u{202e}",
            ),
            ("名前_ünï", "名前_ünï"),
            ("ordinary", "ordinary"),
        ] {
            let table = TableName::new("app", name);
            let relation = RebuildRelation {
                uid: "t_aaaaaa".parse().unwrap(),
                name: table.clone(),
            };
            let identities = RebuildIdentities {
                tables: Default::default(),
                columns: Default::default(),
            };
            for change in [
                Change::RebuildPartitionedTable(Box::new(RebuildPartitionedTable {
                    earlier: Vec::new(),
                    table: table.clone(),
                    before: Default::default(),
                    after: Default::default(),
                    intent: Rebuild {
                        source: identities.clone(),
                        replacement: identities,
                        replacement_root: "t_aaaaaa".parse().unwrap(),
                        continues: Default::default(),
                        target_checksum: "not an execution fixture".into(),
                        max_rows: 0,
                        allowed_at: "2026-10-24T00:00:00Z".into(),
                        reason: "planned rewrite".parse().unwrap(),
                        operator: "operator".into(),
                        types: Default::default(),
                        conversions: Default::default(),
                    },
                })),
                Change::CopyTableRows(Box::new(CopyTableRows {
                    source: relation.clone(),
                    target: relation.clone(),
                    columns: Vec::new(),
                    source_partitions: Vec::new(),
                    target_partitions: Vec::new(),
                    max_rows: 0,
                })),
                Change::AnalyzeTableTree(Box::new(AnalyzeTableTree {
                    parent: relation,
                    partitions: Vec::new(),
                })),
            ] {
                let (operation, _) = change.pending_rebuild().unwrap();
                assert_eq!(
                    super::describe(&change),
                    format!("pending blocking rewrite: {operation} for app.{shown} (unavailable)")
                );
                let unchecked = pbps_dialect::Unchecked::for_change(&change, r"tool text \x");
                assert_eq!(unchecked.description, format!("app.{shown}"));
                assert_eq!(unchecked.reason, r"tool text \x");
                assert_eq!(change.subject(), format!("app.{name}"));
                assert_eq!(change.table(), Some(&table));
            }
        }
    }
}

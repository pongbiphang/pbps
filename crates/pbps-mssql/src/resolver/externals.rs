//! File batches, complete scratch inventory and automatic external staging.
//! Owned cleanup, not transaction framing, contains partially committed files.

use super::capture::ObjectIdentity;
use pbps_db::DbError;
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use std::collections::{BTreeMap, BTreeSet};

pub mod render;

/// Only ordinary GO is a client command. Preserve every other byte, including
/// comments on either side of a separator, for the engine's own parser.
pub fn batches(script: &str) -> Result<Vec<&str>, DbError> {
    Ok(scan(script)?.batches)
}

/// The marker is a complete line outside quotes and comments, not a token
/// searched inside source text (DEC-2376.1).
pub const DEFERRED_MARKER: &str = "-- pbps:deferred";

#[derive(Debug, PartialEq, Eq)]
pub struct Sections<'a> {
    pub immediate: &'a str,
    pub deferred: Option<&'a str>,
}

pub fn sections(script: &str) -> Result<Sections<'_>, DbError> {
    let scan = scan(script)?;
    Ok(match scan.marker {
        Some(marker) => Sections {
            immediate: &script[..marker.start],
            deferred: Some(&script[marker.end..]),
        },
        None => Sections {
            immediate: script,
            deferred: None,
        },
    })
}

struct Scan<'a> {
    batches: Vec<&'a str>,
    marker: Option<std::ops::Range<usize>>,
}

fn scan(script: &str) -> Result<Scan<'_>, DbError> {
    #[derive(Clone, Copy)]
    enum State {
        Sql,
        Quote(u8),
        Block(usize),
    }
    let mut state = State::Sql;
    let mut offset = 0;
    let mut start = 0;
    let mut result = Vec::new();
    let mut marker = None;
    let mut lines = script.split_inclusive(['\r', '\n']).peekable();
    while let Some(line) = lines.next() {
        let line = if line.ends_with('\r') && lines.peek() == Some(&"\n") {
            lines.next();
            &script[offset..offset + line.len() + 1]
        } else {
            line
        };

        if matches!(state, State::Sql) && line.trim() == DEFERRED_MARKER {
            if marker.replace(offset..offset + line.len()).is_some() {
                return Err(DbError::Refused(
                    "the externals file has a second deferred marker".into(),
                ));
            }
            offset += line.len();
            continue;
        }
        let mut visible = if matches!(state, State::Quote(_)) {
            "x".into()
        } else {
            String::new()
        };
        let bytes = line.as_bytes();
        let mut first = None;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let next = bytes.get(i + 1).copied();
            match state {
                State::Sql if b == b'-' && next == Some(b'-') => break,
                State::Sql if b == b'/' && next == Some(b'*') => {
                    visible.push(' ');
                    state = State::Block(1);
                    i += 2;
                }
                State::Sql if matches!(b, b'\'' | b'"' | b'[') => {
                    visible.push('x');
                    state = State::Quote(if b == b'[' { b']' } else { b });
                    i += 1;
                }
                State::Sql => {
                    if !b.is_ascii_whitespace() && first.is_none() {
                        first = Some(i);
                    }
                    visible.push(if b.is_ascii() { char::from(b) } else { 'x' });
                    i += 1;
                }
                State::Quote(end) if b == end && next == Some(end) => i += 2,
                State::Quote(end) if b == end => {
                    state = State::Sql;
                    i += 1;
                }
                State::Quote(_) => i += 1,
                State::Block(n) if b == b'/' && next == Some(b'*') => {
                    state = State::Block(n + 1);
                    i += 2;
                }
                State::Block(n) if b == b'*' && next == Some(b'/') => {
                    state = if n == 1 {
                        State::Sql
                    } else {
                        State::Block(n - 1)
                    };
                    i += 2;
                }
                State::Block(_) => i += 1,
            }
        }
        let control = visible.trim();
        if control.starts_with(':') || control.starts_with("!!") || control.contains("$(") {
            return Err(DbError::Refused(format!(
                "unsupported sqlcmd directive or variable in externals file: {control:?}"
            )));
        }
        let mut words = control.split_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case("GO")) {
            if words.next().is_some() {
                return Err(DbError::Refused(format!(
                    "unsupported GO repetition or arguments in externals file: {control:?}"
                )));
            }
            let at = offset + first.expect("a GO token has an offset");
            if !script[start..at].trim().is_empty() {
                result.push(&script[start..at]);
            }
            start = at + 2;
        }
        offset += line.len();
    }
    if !script[start..].trim().is_empty() {
        result.push(&script[start..]);
    }
    Ok(Scan {
        batches: result,
        marker,
    })
}

/// The caller has already entered its dedicated sandbox session. Execute
/// drains each batch through the ordinary driver; a COMMIT cannot escape the
/// run's recorded cleanup obligation (DEC-2269.1).
pub async fn run(conn: &mut impl ExecuteConnection, script: &str) -> Result<(), DbError> {
    for batch in batches(script)? {
        conn.execute(batch).await?;
    }
    Ok(())
}

#[derive(Debug)]
struct Entry {
    root: ObjectIdentity,
    parent: Option<ObjectIdentity>,
}

#[derive(Debug)]
pub struct Inventory(BTreeMap<String, Entry>);

/// Reuse the cleanup inventory's full catalog coverage and owner requirement.
/// Hidden catalogs and failed reads must never become an empty delta.
pub async fn inventory(
    conn: &mut impl QueryConnection,
    sandbox: &str,
) -> Result<Inventory, DbError> {
    // Membership alone is not evidence of complete visibility after a file
    // changes permissions. Require the engine's effective metadata answer.
    require_visible_catalog(conn).await?;
    let inventory = super::scratch::inventory::read(conn, Some(sandbox)).await?;
    Ok(Inventory(
        inventory
            .entries
            .iter()
            .map(|entry| {
                let root = entry.staged_root.clone().unwrap_or_else(|| ObjectIdentity {
                    class: "unsupported scratch inventory".into(),
                    name: vec![entry.name.clone()],
                    signature: entry.staged_parent.iter().cloned().collect(),
                });
                (
                    entry.staging_key(),
                    Entry {
                        root,
                        parent: entry.staged_parent.clone(),
                    },
                )
            })
            .collect(),
    ))
}

pub fn created(before: &Inventory, after: &Inventory) -> BTreeSet<ObjectIdentity> {
    let delta: Vec<_> = after
        .0
        .iter()
        .filter(|(key, _)| !before.0.contains_key(*key))
        .map(|(_, entry)| entry)
        .collect();
    let roots: BTreeSet<_> = delta.iter().map(|entry| entry.root.clone()).collect();
    delta
        .into_iter()
        .filter(|entry| {
            !entry
                .parent
                .as_ref()
                .is_some_and(|parent| roots.contains(parent))
        })
        .map(|entry| entry.root.clone())
        .collect()
}

/// Effective metadata visibility is checked independently of role membership,
/// since the file can change its own permissions in another session.
async fn require_visible_catalog(conn: &mut impl QueryConnection) -> Result<(), DbError> {
    let rows = conn
        .query("SELECT HAS_PERMS_BY_NAME(DB_NAME(),N'DATABASE',N'VIEW DEFINITION') AS visible;")
        .await?;
    if rows.len() != 1 || rows[0].try_get::<i32>("visible")? != Some(1) {
        return Err(DbError::Refused(
            "external inventory requires database VIEW DEFINITION".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::capture;
    #[test]
    fn go_separator_split_respects_quotes_and_comments() {
        let script = "SELECT N'one\nGO\ntwo', 'a''GO', [a]]GO], \"GO\";\n  gO -- next\n/* outer\n/* GO */\nGO\n*/ SELECT 2;\nGO /* trailing\nGO\n*/\nSELECT 3;";
        let got = batches(script).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].contains("GO\ntwo"));
        assert!(got[1].contains("*/ SELECT 2"));
        assert!(got[2].contains("/* trailing\nGO"));
        assert_eq!(batches("SELECT [GO];").unwrap(), ["SELECT [GO];"]);
        assert!(
            batches("GO 5")
                .unwrap_err()
                .to_string()
                .contains("GO repetition")
        );
        assert!(batches("go -1 -- no").is_err());
        assert_eq!(batches("GO\r\nSELECT 1;").unwrap(), ["\r\nSELECT 1;"]);
    }
    #[test]
    fn deferred_section_uses_one_top_level_marker() {
        let initial = "SELECT N'x\n-- pbps:deferred\nx';\n/* outer\n/* nested */\n-- pbps:deferred\n*/\nSELECT [a\n-- pbps:deferred\na];\n";
        let script = format!("{initial}  {DEFERRED_MARKER}\r\nSELECT 2;");
        let parts = sections(&script).unwrap();
        assert_eq!(parts.immediate, initial);
        assert_eq!(parts.deferred, Some("SELECT 2;"));
        assert_eq!(sections(initial).unwrap().deferred, None);
        for newline in ["\n", "\r\n", "\r"] {
            let text = format!("SELECT 1;{newline}{DEFERRED_MARKER}{newline}SELECT 2;");
            let sections = sections(&text).unwrap();
            assert_eq!(sections.immediate, format!("SELECT 1;{newline}"));
            assert_eq!(sections.deferred, Some("SELECT 2;"));
        }

        assert!(
            sections(&format!("{DEFERRED_MARKER}\n{DEFERRED_MARKER}"))
                .unwrap_err()
                .to_string()
                .contains("second deferred marker")
        );
        assert!(sections(&format!("{DEFERRED_MARKER}\n:r forbidden.sql")).is_err());
        assert_eq!(
            sections("-- pbps:deferred extra\nSELECT 1;")
                .unwrap()
                .deferred,
            None
        );
    }

    #[test]
    fn sqlcmd_directive_refuses() {
        for text in [
            ":r file.sql",
            ":setvar x 1",
            "!! echo x",
            ":connect server",
            ":on error exit",
            "SELECT $(x);",
        ] {
            assert!(batches(text).unwrap_err().to_string().contains("sqlcmd"));
        }
        for text in [
            "SELECT N':r file';",
            "-- !! echo x",
            "/* :r file */ SELECT 1;",
            "SELECT N'x\n:r file\nGO\nx';",
        ] {
            assert_eq!(batches(text).unwrap(), [text]);
        }
    }
    #[test]
    fn only_children_of_created_parents_are_carried() {
        let root = capture::object("dbo", "t");
        let child = capture::object("dbo", "pk");
        let before = Inventory(BTreeMap::new());
        let after = Inventory(BTreeMap::from([
            (
                "t".into(),
                Entry {
                    root: root.clone(),
                    parent: None,
                },
            ),
            (
                "pk".into(),
                Entry {
                    root: child.clone(),
                    parent: Some(root.clone()),
                },
            ),
        ]));
        assert_eq!(created(&before, &after), BTreeSet::from([root]));
        let existing = Inventory(BTreeMap::from([(
            "t".into(),
            Entry {
                root: capture::object("dbo", "t"),
                parent: None,
            },
        )]));
        assert_eq!(created(&existing, &after), BTreeSet::from([child]));
    }
}

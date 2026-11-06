//! The identity tree as a test sees it (#2600 PR 5a).
//!
//! Tests used to read and write one file, `schema.ids.json`. The identity now
//! lives as one file per declaration under `.pbps/ids`, so these helpers give
//! back the two things a test wants of it: the whole tree as one comparable
//! value (`snapshot`, `text`), which keeps "the identities were left exactly as
//! they were" a one-line assertion, and the mapping it records (`model`). `write`
//! puts a mapping in place as a tree, each object at the path its declaration
//! mirrors, found by reading the project's own YAML.
//!
//! Test fixtures, not production code: nothing here is what `pbps plan` writes.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pbps_model::ids_tree::{self, Placement};
use pbps_model::{IdsFile, UidKind};

pub const DIR: &str = ".pbps/ids";

pub fn dir(project: &Path) -> PathBuf {
    project.join(DIR)
}

/// Every file of the tree, by path relative to it.
pub fn files(project: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.insert(
                    rel.to_str().unwrap().replace('\\', "/"),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let root = dir(project);
    let mut out = BTreeMap::new();
    walk(&root, &root, &mut out);
    out
}

pub fn exists(project: &Path) -> bool {
    !files(project).is_empty()
}

const MARK: &str = "=== ";

/// The tree as one comparable string, the way the tests used to compare the
/// one identity file. A tree that reads back is its mapping as pretty JSON, so
/// "the identities were left as they were" stays one assertion and a test can
/// still parse what it read; a tree that does not read back (a test damaged it
/// on purpose) is `=== <path>` and the file's text, in path order, so that
/// "left exactly as it was" still holds for it. Empty when there is no tree.
pub fn text(project: &Path) -> String {
    let files = files(project);
    if files.is_empty() {
        return String::new();
    }
    let texts: BTreeMap<String, String> = files
        .iter()
        .filter_map(|(rel, bytes)| Some((rel.clone(), String::from_utf8(bytes.clone()).ok()?)))
        .collect();
    if texts.len() == files.len()
        && let Ok(ids) = ids_tree::assemble(&texts)
    {
        return format!("{}\n", serde_json::to_string_pretty(&ids).unwrap());
    }
    let mut out = String::new();
    for (rel, bytes) in files {
        out.push_str(&format!("{MARK}{rel}\n"));
        out.push_str(&String::from_utf8_lossy(&bytes));
    }
    out
}

pub fn snapshot(project: &Path) -> Vec<u8> {
    text(project).into_bytes()
}

/// Puts a [`snapshot`] back: a mapping is written as a tree, a damaged tree as
/// exactly its files.
pub fn restore(project: &Path, snapshot: &[u8]) {
    let text = std::str::from_utf8(snapshot).unwrap();
    if text.trim_start().starts_with('{') {
        write(project, &serde_json::from_str(text).unwrap());
        return;
    }
    let _ = std::fs::remove_dir_all(dir(project));
    let mut current: Option<(String, String)> = None;
    let put = |entry: Option<(String, String)>| {
        if let Some((rel, body)) = entry {
            let path = dir(project).join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    };
    for line in text.split_inclusive('\n') {
        if let Some(rel) = line.strip_prefix(MARK) {
            put(current.take());
            current = Some((rel.trim_end().to_owned(), String::new()));
        } else if let Some((_, body)) = &mut current {
            body.push_str(line);
        }
    }
    put(current);
}

/// The mapping the tree records; empty when there is no tree.
pub fn model(project: &Path) -> IdsFile {
    let texts: BTreeMap<String, String> = files(project)
        .into_iter()
        .map(|(rel, bytes)| (rel, String::from_utf8(bytes).unwrap()))
        .collect();
    if texts.is_empty() {
        return IdsFile::default();
    }
    ids_tree::assemble(&texts).unwrap()
}

/// Where each declaration's YAML is, by the name it declares, relative to
/// `schema/`: read from the files, as a person would.
fn declared(project: &Path) -> BTreeMap<(UidKind, String), String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<(UidKind, String), String>) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("yml" | "yaml")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let Some(first) = text
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
            else {
                continue;
            };
            let Some((key, name)) = first.split_once(':') else {
                continue;
            };
            let kind = match key {
                "table" => UidKind::Table,
                "role" => UidKind::Role,
                "enum" | "domain" => UidKind::Type,
                "extension" => UidKind::Extension,
                _ => continue,
            };
            let rel = path.strip_prefix(root).unwrap().to_str().unwrap();
            out.insert(
                (kind, name.trim().trim_matches('"').to_owned()),
                rel.replace('\\', "/"),
            );
        }
    }
    let root = project.join("schema");
    let mut out = BTreeMap::new();
    walk(&root, &root, &mut out);
    out
}

/// Writes `ids` as a tree, each object at the path its declaration mirrors
/// (`schema/a/b.yml` is `a/b.json`), or at `<name>.json` when no declaration
/// names it. A dropped object, with no declaration, goes aside under its uid.
/// Replaces whatever tree was there.
pub fn write(project: &Path, ids: &IdsFile) {
    let at = declared(project);
    let mirror = |kind: UidKind, name: String, uid: &pbps_model::Uid| -> String {
        match at.get(&(kind, name.clone())) {
            Some(yaml) => format!(
                "{}.json",
                yaml.strip_suffix(".yml")
                    .or_else(|| yaml.strip_suffix(".yaml"))
                    .unwrap_or(yaml)
            ),
            None => format!("{name}.{uid}.json"),
        }
    };
    let mut placement = Placement::default();
    for (uid, name) in &ids.tables {
        placement.paths.insert(
            uid.clone(),
            mirror(UidKind::Table, name.raw().to_string(), uid),
        );
        // The columns in the order the declaration gives them, as the writer
        // keeps them: a tree in any other order is not what `plan` would write.
        if let Some(yaml) = at.get(&(UidKind::Table, name.raw().to_string())) {
            let text =
                std::fs::read_to_string(project.join("schema").join(yaml)).unwrap_or_default();
            let mut in_columns = false;
            let mut order = Vec::new();
            for line in text.lines() {
                if line.starts_with("columns:") {
                    in_columns = true;
                } else if in_columns && !line.starts_with(' ') && !line.is_empty() {
                    in_columns = false;
                } else if in_columns
                    && line.starts_with("  ")
                    && !line.starts_with("   ")
                    && let Some((key, _)) = line.trim().split_once(':')
                    && let Some((u, _)) = ids
                        .columns
                        .iter()
                        .find(|(_, c)| c.table == *name && c.name.raw() == key.trim())
                {
                    order.push(u.clone());
                }
            }
            placement.order.insert(uid.clone(), order);
        }
    }
    for (uid, name) in &ids.roles {
        placement.paths.insert(
            uid.clone(),
            mirror(UidKind::Role, name.raw().to_owned(), uid),
        );
    }
    for (uid, name) in &ids.types {
        placement.paths.insert(
            uid.clone(),
            mirror(UidKind::Type, name.raw().to_string(), uid),
        );
    }
    for (uid, name) in &ids.extensions {
        placement.paths.insert(
            uid.clone(),
            mirror(UidKind::Extension, name.raw().to_owned(), uid),
        );
    }
    for (uid, tomb) in &ids.tombstones {
        if matches!(
            uid.kind(),
            UidKind::Table | UidKind::Role | UidKind::Type | UidKind::Extension
        ) {
            placement
                .paths
                .insert(uid.clone(), format!("{}.{uid}.json", tomb.was));
        }
    }
    // A column tombstone whose `was` names no table lives in the one table there is.
    if let [only] = ids.tables.keys().collect::<Vec<_>>()[..] {
        for uid in ids
            .tombstones
            .keys()
            .filter(|u| u.kind() == UidKind::Column)
        {
            placement.owners.insert(uid.clone(), only.clone());
        }
    }
    let _ = std::fs::remove_dir_all(dir(project));
    for (rel, text) in ids_tree::split(ids, &placement).unwrap() {
        let path = dir(project).join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

/// Puts one malformed file where an identity file would be, so a test can see
/// how a damaged tree is refused. `name` is the file's name in the tree.
pub fn write_raw(project: &Path, name: &str, bytes: impl AsRef<[u8]>) {
    let path = dir(project).join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The tree as git has it at `rev`: path under the tree, to bytes.
pub fn revision_files(project: &Path, rev: &str) -> BTreeMap<String, Vec<u8>> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        out.stdout
    };
    let listing = git(&["ls-tree", "-r", "--name-only", "-z", rev, "--", DIR]);
    let mut out = BTreeMap::new();
    for path in listing.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(path).unwrap();
        let bytes = git(&["show", &format!("{rev}:./{path}")]);
        out.insert(
            path.strip_prefix(&format!("{DIR}/")).unwrap().to_owned(),
            bytes,
        );
    }
    out
}

/// The mapping the tree recorded at `rev`.
pub fn model_at(project: &Path, rev: &str) -> IdsFile {
    let texts: BTreeMap<String, String> = revision_files(project, rev)
        .into_iter()
        .map(|(rel, bytes)| (rel, String::from_utf8(bytes).unwrap()))
        .collect();
    ids_tree::assemble(&texts).unwrap()
}

/// Copies the tree as it was at `rev` into `dest`, replacing what is there.
pub fn copy_revision(project: &Path, rev: &str, dest: &Path) {
    let _ = std::fs::remove_dir_all(dir(dest));
    for (rel, bytes) in revision_files(project, rev) {
        let path = dir(dest).join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

/// Sets the reason of the allowed row move recorded for `uid`, in whichever
/// file holds it, leaving every other file and every other byte of that file's
/// layout to the writer. For tests that need a record a command would refuse to
/// write, such as a blank reason.
pub fn set_move_reason(project: &Path, uid: &pbps_model::Uid, reason: &str) {
    for (rel, bytes) in files(project) {
        let text = String::from_utf8(bytes).unwrap();
        let mut file = ids_tree::parse(&rel, &text).unwrap();
        if let ids_tree::Body::Table(table) = &mut file.body
            && let Some((_, record)) = table.moves.iter_mut().find(|(u, _)| u == uid)
        {
            record.reason = reason.into();
            std::fs::write(dir(project).join(rel), ids_tree::render(&file)).unwrap();
            return;
        }
    }
    panic!("no file records a move for {uid}");
}

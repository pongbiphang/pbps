//! The identity-tree validator (#2600, design §4): every way a tree can be
//! wrong, as findings with stable codes, over the tree's text and the
//! declaration files it mirrors.
//!
//! It does no I/O. The caller lists the ids files (path relative to the ids
//! directory, to text) and the declarations, and gets back every finding, not
//! the first: a merge usually leaves several at once, and one run should show
//! all of them. [`assemble`] and the per-file [`parse`] stay the single
//! reader; this module only maps their refusals onto codes and adds the checks
//! that need more than one file or a declaration.

use std::collections::{BTreeMap, BTreeSet};

use crate::escape::Escaped;
use crate::ids::IdsError;
use crate::ids_tree::{Body, TreeError, TreeFile, assemble, parse, render};
use crate::uid::{Uid, UidKind};

/// A stable, documented code (DEC-2600.6). Scripts and CI match on
/// [`Code::as_str`], so a variant is never renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Code {
    YamlWithoutIds,
    Orphan,
    NameMismatch,
    DuplicateUid,
    DroppedButDeclared,
    LiveColumnInDropped,
    DuplicateName,
    PathCollision,
    CaseCollision,
    ConflictMarkers,
    Malformed,
    SentinelMissing,
    Format,
    LegacyLayout,
    LegacyBesideTree,
    NotCanonical,
    /// `IdsFile::validate` refused the assembled mapping for a reason that has
    /// no code of its own (an orphan column, a bad rebuild, ...).
    InvalidMapping,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::YamlWithoutIds => "ids.yaml-without-ids",
            Code::Orphan => "ids.orphan",
            Code::NameMismatch => "ids.name-mismatch",
            Code::DuplicateUid => "ids.duplicate-uid",
            Code::DroppedButDeclared => "ids.dropped-but-declared",
            Code::LiveColumnInDropped => "ids.live-column-in-dropped",
            Code::DuplicateName => "ids.duplicate-name",
            Code::PathCollision => "ids.path-collision",
            Code::CaseCollision => "ids.case-collision",
            Code::ConflictMarkers => "ids.conflict-markers",
            Code::Malformed => "ids.malformed",
            Code::SentinelMissing => "ids.sentinel-missing",
            Code::Format => "ids.format",
            Code::LegacyLayout => "ids.legacy-layout",
            Code::LegacyBesideTree => "ids.legacy-beside-tree",
            Code::NotCanonical => "ids.not-canonical",
            Code::InvalidMapping => "ids.invalid-mapping",
        }
    }
}

/// One thing wrong, with the file it is about and what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: Code,
    /// The ids file (or declaration, for a collision) as the user sees it.
    pub path: String,
    pub message: String,
    pub remedy: String,
}

/// A declaration file that carries an identity: a table, role, type or
/// extension. Modules and dependencies carry none and are not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Project-relative path of the YAML file.
    pub yaml: String,
    pub kind: UidKind,
    /// The name the file declares (`schema.table`, a role, ...).
    pub name: String,
    /// The old name when the declaration carries `renamed_from`.
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Layout<'a> {
    /// Both project-relative with no trailing slash, e.g. `schema` and
    /// `.pbps/ids`.
    pub schema_dir: &'a str,
    pub ids_dir: &'a str,
}

pub struct Input<'a> {
    pub layout: Layout<'a>,
    pub declared: &'a [Declaration],
    /// Path relative to the ids directory, to the file's text.
    pub ids: &'a BTreeMap<String, String>,
    /// `schema.ids.json` of the old layout exists.
    pub legacy_file: bool,
}

/// The ids path (relative to the ids directory) a declaration file mirrors:
/// `<schema_dir>/<rel>/<name>.yml` is `<rel>/<name>.json` (design §3.2).
pub fn mirrored(layout: Layout<'_>, yaml: &str) -> Option<String> {
    let rel = yaml.strip_prefix(layout.schema_dir)?.strip_prefix('/')?;
    let stem = rel
        .strip_suffix(".yml")
        .or_else(|| rel.strip_suffix(".yaml"))?;
    (!stem.is_empty()).then(|| format!("{stem}.json"))
}

/// Checks the whole tree. Findings come back sorted by path and code, so a
/// run is reproducible.
pub fn check(input: &Input<'_>) -> Vec<Finding> {
    let mut run = Run {
        layout: input.layout,
        out: Vec::new(),
    };
    run.legacy(input);
    let parsed = run.read(input.ids);
    let clean = parsed.len() == input.ids.len();
    let unique = run.uids(&parsed);
    if clean && unique {
        run.mapping(input.ids, &parsed);
    }
    run.declarations(input, &parsed);
    run.collisions(input);
    run.out
        .sort_by(|a, b| (&a.path, a.code).cmp(&(&b.path, b.code)));
    run.out
}

struct Run<'a> {
    layout: Layout<'a>,
    out: Vec<Finding>,
}

impl Run<'_> {
    fn shown(&self, rel: &str) -> String {
        format!("{}/{rel}", self.layout.ids_dir)
    }

    fn add(&mut self, code: Code, path: String, message: String, remedy: &str) {
        self.out.push(Finding {
            code,
            path,
            message,
            remedy: remedy.to_owned(),
        });
    }

    fn legacy(&mut self, input: &Input<'_>) {
        let dir = self.layout.ids_dir;
        if !input.legacy_file {
            return;
        }
        if input.ids.is_empty() {
            self.add(
                Code::LegacyLayout,
                "schema.ids.json".into(),
                format!("schema.ids.json is the old layout; this pbps reads {dir}/"),
                "pbps ids migrate",
            );
        } else {
            self.add(
                Code::LegacyBesideTree,
                "schema.ids.json".into(),
                format!("both schema.ids.json and {dir}/ exist"),
                "delete the old file after checking `git diff`",
            );
        }
    }

    /// Parses every file on its own and reports what each refusal means.
    fn read(&mut self, ids: &BTreeMap<String, String>) -> BTreeMap<String, TreeFile> {
        let mut parsed = BTreeMap::new();
        for (rel, text) in ids {
            let shown = self.shown(rel);
            match parse(&shown, text) {
                Ok(file) => {
                    if render(&file) != *text {
                        self.add(
                            Code::NotCanonical,
                            shown.clone(),
                            format!("{shown} is not in canonical form"),
                            "pbps ids fmt",
                        );
                    }
                    parsed.insert(rel.clone(), file);
                }
                Err(e) => self.refused(shown, e),
            }
        }
        parsed
    }

    fn refused(&mut self, shown: String, e: TreeError) {
        let message = e.to_string();
        let (code, remedy) = match &e {
            TreeError::ConflictMarkers { .. } => (
                Code::ConflictMarkers,
                "resolve the YAML, then `pbps ids resolve`",
            ),
            TreeError::SentinelMissing { .. } | TreeError::SentinelNotLast { .. } => (
                Code::SentinelMissing,
                "`pbps ids resolve`, or restore the file from git",
            ),
            TreeError::Format { .. } => (
                Code::Format,
                "`pbps ids resolve`, or restore the file from git",
            ),
            TreeError::DroppedWithLive { .. } => (Code::LiveColumnInDropped, "`pbps ids resolve`"),
            // A key repeated inside one file is a uid listed twice.
            TreeError::DuplicateKey { key, .. } if key.parse::<Uid>().is_ok() => (
                Code::DuplicateUid,
                "keep one entry, delete the other, run `pbps ids fmt`",
            ),
            TreeError::Malformed { .. }
            | TreeError::DuplicateKey { .. }
            | TreeError::KindMismatch { .. }
            | TreeError::DuplicateUid { .. }
            | TreeError::Misplaced { .. }
            | TreeError::BadName { .. }
            | TreeError::Ids(_)
            | TreeError::NoPath { .. }
            | TreeError::UnknownPath { .. }
            | TreeError::PathCollision { .. }
            | TreeError::UnownedTombstone { .. } => (
                Code::Malformed,
                "`pbps ids resolve`, or restore the file from git",
            ),
        };
        self.add(code, shown, message, remedy);
    }

    /// One uid in two files. Returns whether the tree is free of that.
    fn uids(&mut self, parsed: &BTreeMap<String, TreeFile>) -> bool {
        let mut seen: BTreeMap<(&'static str, Uid), Vec<String>> = BTreeMap::new();
        for (rel, file) in parsed {
            let mut claim = |section: &'static str, uid: &Uid| {
                seen.entry((section, uid.clone()))
                    .or_default()
                    .push(rel.clone());
            };
            claim("identity", &file.uid);
            if let Body::Table(t) = &file.body {
                for (u, _) in t.columns.iter().chain(&t.partitions) {
                    claim("identity", u);
                }
                t.moves.iter().for_each(|(u, _)| claim("moves", u));
                t.conversions
                    .iter()
                    .for_each(|(u, _)| claim("conversions", u));
                t.rebuilds.iter().for_each(|(u, _)| claim("rebuilds", u));
            }
        }
        let mut unique = true;
        for ((_, uid), paths) in seen.into_iter().filter(|(_, p)| p.len() > 1) {
            unique = false;
            let shown: Vec<String> = paths.iter().map(|p| self.shown(p)).collect();
            let message = match shown.as_slice() {
                [a, b] if a != b => format!("{uid} appears in {a} and {b}"),
                [a, ..] => format!("{uid} appears twice in {a}"),
                [] => continue,
            };
            self.add(
                Code::DuplicateUid,
                shown[0].clone(),
                message,
                "keep one entry, delete the other, run `pbps ids fmt`",
            );
        }
        unique
    }

    /// The assembled mapping: the checks `IdsFile` already owns.
    fn mapping(&mut self, ids: &BTreeMap<String, String>, parsed: &BTreeMap<String, TreeFile>) {
        let Err(e) = assemble(ids) else { return };
        let TreeError::Ids(inner) = &e else {
            // Every per-file refusal was reported above.
            return;
        };
        let owner = |uid: &Uid| -> String {
            parsed
                .iter()
                .find(|(_, f)| holds(f, uid))
                .map_or_else(|| "the identity tree".into(), |(p, _)| self.shown(p))
        };
        if let IdsError::DuplicateName { a, b, .. } = inner {
            let (pa, pb) = (owner(a), owner(b));
            let message = format!("{inner} (in {pa} and {pb})");
            self.add(
                Code::DuplicateName,
                pa,
                message,
                "delete one of the two entries",
            );
        } else {
            let path = invalid_uid(inner).map_or_else(|| "the identity tree".into(), owner);
            self.add(
                Code::InvalidMapping,
                path,
                inner.to_string(),
                "`pbps ids resolve`, or restore the file from git",
            );
        }
    }

    /// The checks that compare the tree with the declarations.
    fn declarations(&mut self, input: &Input<'_>, parsed: &BTreeMap<String, TreeFile>) {
        let mut declared: BTreeMap<String, &Declaration> = BTreeMap::new();
        for d in input.declared {
            match mirrored(self.layout, &d.yaml) {
                Some(rel) => {
                    declared.entry(rel).or_insert(d);
                }
                None => self.add(
                    Code::Malformed,
                    d.yaml.clone(),
                    format!(
                        "{} is not a .yml or .yaml file under {}",
                        Escaped(&d.yaml),
                        self.layout.schema_dir
                    ),
                    "move the declaration under the schema directory",
                ),
            }
        }
        // Live files nobody declares: orphans, or a file waiting to be moved.
        let unpaired: Vec<(&String, &TreeFile)> = parsed
            .iter()
            .filter(|(rel, f)| f.dropped.is_none() && !declared.contains_key(*rel))
            .collect();
        for (rel, file) in &unpaired {
            let (shown, name) = (self.shown(rel), name_of(file));
            self.add(
                Code::Orphan,
                shown.clone(),
                format!(
                    "{shown} names {} but no declaration is at its mirrored path",
                    Escaped(&name)
                ),
                "moved the file: `pbps plan`; removed it: `pbps drop <name> --reason ...`",
            );
        }
        for (rel, d) in &declared {
            let shown = self.shown(rel);
            let Some(file) = parsed.get(rel) else {
                if !input.ids.contains_key(rel) && !relocatable(d, &unpaired) {
                    self.add(
                        Code::YamlWithoutIds,
                        d.yaml.clone(),
                        format!(
                            "{} ({}) has no identity file at {shown}",
                            Escaped(&d.name),
                            d.yaml
                        ),
                        "pbps plan",
                    );
                }
                continue;
            };
            if file.dropped.is_some() {
                self.add(
                    Code::DroppedButDeclared,
                    shown.clone(),
                    format!("{shown} is marked dropped but {} exists", d.yaml),
                    "restore the table with a new uid (`pbps plan` after moving the old file aside), or remove the declaration",
                );
            } else if file.body.kind() != d.kind || !agrees(d, &name_of(file)) {
                self.add(
                    Code::NameMismatch,
                    shown.clone(),
                    format!(
                        "{shown} says {} but the declaration {} says {}",
                        Escaped(&name_of(file)),
                        d.yaml,
                        Escaped(&d.name)
                    ),
                    "`pbps rename <old> <new>`, or add `renamed_from`",
                );
            }
        }
    }

    /// Two declarations on one ids path, or two paths equal ignoring case.
    fn collisions(&mut self, input: &Input<'_>) {
        let mut by_ids: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for d in input.declared {
            if let Some(rel) = mirrored(self.layout, &d.yaml) {
                by_ids.entry(rel).or_default().push(&d.yaml);
            }
        }
        for (rel, yamls) in by_ids.iter().filter(|(_, y)| y.len() > 1) {
            self.add(
                Code::PathCollision,
                yamls[0].to_owned(),
                format!("{} both map to {}", yamls.join(" and "), self.shown(rel)),
                "rename one",
            );
        }
        // Fold the ids paths, not the YAML paths: `a.yml` and `A.yaml` differ in
        // case and extension, yet map to `a.json` and `A.json`, which cannot
        // coexist on a case-insensitive checkout. The YAML tree is covered
        // too, since two YAML paths equal ignoring case map to paths that are.
        let mut folded: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let declared = input
            .declared
            .iter()
            .filter_map(|d| mirrored(self.layout, &d.yaml));
        for rel in declared.chain(input.ids.keys().cloned()) {
            let shown = self.shown(&rel);
            folded
                .entry(shown.to_lowercase())
                .or_default()
                .insert(shown);
        }
        for set in folded.values().filter(|s| s.len() > 1) {
            let list: Vec<&str> = set.iter().map(String::as_str).collect();
            self.add(
                Code::CaseCollision,
                list[0].to_owned(),
                format!("{} differ only in case", list.join(" and ")),
                "rename one",
            );
        }
    }
}

fn name_of(file: &TreeFile) -> String {
    match &file.body {
        Body::Table(t) => t.table.clone(),
        Body::Role(n) | Body::Type(n) | Body::Extension(n) => n.clone(),
    }
}

/// A rename in progress is not a mismatch: the declaration says the old name
/// out loud, and `pbps plan` updates the file.
fn agrees(d: &Declaration, recorded: &str) -> bool {
    d.name == recorded || d.renamed_from.as_deref() == Some(recorded)
}

/// Exactly one unpaired live file of the declaration's kind carries its name
/// (or its `renamed_from`): `pbps plan` relocates it, so the declaration is not
/// missing an identity (design §3.4 step 2).
fn relocatable(d: &Declaration, unpaired: &[(&String, &TreeFile)]) -> bool {
    unpaired
        .iter()
        .filter(|(_, f)| f.body.kind() == d.kind && agrees(d, &name_of(f)))
        .count()
        == 1
}

fn holds(file: &TreeFile, uid: &Uid) -> bool {
    file.uid == *uid
        || matches!(&file.body, Body::Table(t)
            if t.columns.iter().chain(&t.partitions).any(|(u, _)| u == uid)
                || t.rebuilds.iter().any(|(u, _)| u == uid))
}

fn invalid_uid(e: &IdsError) -> Option<&Uid> {
    match e {
        IdsError::LiveAndTombstoned { uid }
        | IdsError::KindMismatch { uid }
        | IdsError::OrphanColumn { uid, .. }
        | IdsError::OrphanMove { uid }
        | IdsError::InvalidRebuild { uid }
        | IdsError::OrphanConversion { uid }
        | IdsError::InvalidConversion { uid }
        | IdsError::InvalidDropRequirement { uid }
        | IdsError::NotHeld { uid, .. } => Some(uid),
        IdsError::UnsupportedVersion { .. } | IdsError::DuplicateName { .. } => None,
    }
}

#[cfg(test)]
mod tests;

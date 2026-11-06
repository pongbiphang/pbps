//! The identity tree (#2600): one file per declaration file under `.pbps/ids/`.
//!
//! [`IdsFile`] stays the one in-memory mapping that plans and state snapshots
//! embed, so no plan or state format moves. This module only converts between
//! it and the files that carry it on disk: [`assemble`] reads a tree into an
//! [`IdsFile`], and [`split`] writes one back. It does no I/O and knows no
//! YAML path; the caller says where each object lives ([`Placement`]).
//!
//! The canonical bytes are the "T-both" layout the merge measurements chose
//! (design-2600): a record per line, a blank line after the opener and after
//! every record, a `_end` sentinel closing each section, and tombstones kept in
//! place. Two branches editing different records then never share a hunk, so
//! git merges them without help. [`render`] is the only writer of those bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use crate::escape::Escaped;
use serde::{Deserialize, Serialize};

use crate::ids::{Conversion, IdsError, IdsFile, Move, Tombstone};
use crate::name::{ColumnRef, ObjectLocalName, RoleName, TableName};
use crate::uid::{Uid, UidKind};
use crate::{Rebuild, TypeRef};

/// The layout version of one file. The mapping's meaning keeps its own
/// [`IdsFile::version`]; two numbers, two jobs (DEC-2600.1).
pub const FORMAT: u32 = 1;

/// The constant first line of every file, exactly 400 bytes.
///
/// It is padding as much as prose: git keeps a rename or move paired only
/// while the two sides are at least half alike, and a three-column table file
/// is smaller than the edits that accompany a move. A constant 400-byte line
/// kept the least similarity measured at 70% where none left it at 0%. Changing
/// one byte rewrites every file, so it is a wire constant (design-2600 §2.5).
pub const GENERATED: &str = "This file is written by pbps and records the identity of one declaration file. Do not edit it by hand: change the YAML declaration and run `pbps plan`. After a git merge conflict, resolve the YAML first, then run `pbps ids resolve`; `pbps ids fmt` restores the canonical form. This line is a wire constant and never changes: its length keeps git's rename detection working for small files in a merge.";

/// What the tree got wrong. Every variant names the file, so a refusal can be
/// acted on without opening anything.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TreeError {
    // Every path and every piece of text a serde error repeats is the file's own
    // spelling, so it is shown escaped: a line break in a file name or an
    // unknown field must not start a line of the refusal (DEC-2032.1).
    #[error("{} still has merge conflict markers", Escaped(.path))]
    ConflictMarkers { path: String },
    #[error("{} is not a valid identity file: {}", Escaped(.path), Escaped(.message))]
    Malformed { path: String, message: String },
    #[error("{} names `{}` twice", Escaped(.path), Escaped(.key))]
    DuplicateKey { path: String, key: String },
    #[error("{} is layout format {found}, but this tool reads {FORMAT}", Escaped(.path))]
    Format { path: String, found: u32 },
    #[error("{}: the prefix of {uid} does not match where it appears", Escaped(.path))]
    KindMismatch { path: String, uid: Uid },
    #[error("{}: section `{section}` has no `_end` entry", Escaped(.path))]
    SentinelMissing { path: String, section: &'static str },
    #[error("{}: `_end` is not the last entry of section `{section}`", Escaped(.path))]
    SentinelNotLast { path: String, section: &'static str },
    #[error("{} is marked dropped but holds live entries or intent", Escaped(.path))]
    DroppedWithLive { path: String },
    #[error("{uid} appears in {} and {}", Escaped(.a), Escaped(.b))]
    DuplicateUid { uid: Uid, a: String, b: String },
    #[error("{}: the `{section}` entry {uid} does not belong to this file", Escaped(.path))]
    Misplaced {
        path: String,
        section: &'static str,
        uid: Uid,
    },
    #[error(
        "{}: `{}` is not a name the identity mapping can read back: {why}",
        Escaped(.path),
        Escaped(.name)
    )]
    BadName {
        path: String,
        name: String,
        why: String,
    },
    #[error("{0}")]
    Ids(IdsError),
    #[error("no identity file is placed for {uid}")]
    NoPath { uid: Uid },
    #[error("the placement names a file for {uid}, which the mapping does not hold")]
    UnknownPath { uid: Uid },
    #[error("{} and {} would both be written to {}", Escaped(.a.as_str()), Escaped(.b.as_str()), Escaped(.path))]
    PathCollision { path: String, a: Uid, b: Uid },
    #[error("the tombstone {uid} has no table to live in; name its owner in the placement")]
    UnownedTombstone { uid: Uid },
}

/// A record that is live, carrying its local name, or retired in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Rec {
    Live(String),
    Dead(Tombstone),
}

/// What a file is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Table(TableBody),
    Role(String),
    Type(String),
    Extension(String),
}

/// The sections of a table file, each in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableBody {
    /// The qualified name, as the declaration spells it. For a dropped table,
    /// the last live name.
    pub table: String,
    pub columns: Vec<(Uid, Rec)>,
    /// Partition children, which have a table uid but no column uids
    /// (SPEC §4.3). A live record holds the child's qualified name.
    pub partitions: Vec<(Uid, Rec)>,
    pub moves: Vec<(Uid, Move)>,
    pub conversions: Vec<(Uid, Conversion)>,
    pub rebuilds: Vec<(Uid, Rebuild)>,
}

/// One parsed file: the object it identifies, what it holds, and, for a
/// dropped object, the tombstone that stays in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    pub uid: Uid,
    pub body: Body,
    pub dropped: Option<Tombstone>,
}

impl Body {
    pub(crate) fn kind(&self) -> UidKind {
        match self {
            Body::Table(_) => UidKind::Table,
            Body::Role(_) => UidKind::Role,
            Body::Type(_) => UidKind::Type,
            Body::Extension(_) => UidKind::Extension,
        }
    }
}

// ---------------------------------------------------------------- writer ----

/// `serde_json` with a space after `:` and `,`, so a record reads on one line
/// the way the layout shows it. Struct fields keep their declared order.
struct Spaced;

impl serde_json::ser::Formatter for Spaced {
    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b": ")
    }
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
}

fn line<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Spaced);
    // Every value here is a string-keyed tree of plain data: it cannot fail.
    value.serialize(&mut ser).expect("plain data serializes");
    String::from_utf8(out).expect("JSON is UTF-8")
}

fn section<V: Serialize>(name: &str, records: &[(Uid, V)]) -> String {
    let mut s = format!("  \"{name}\": {{\n\n");
    for (uid, value) in records {
        s.push_str(&format!("    {}: {},\n\n", line(uid.as_str()), line(value)));
    }
    s.push_str("    \"_end\": true\n\n  }");
    s
}

/// The canonical bytes of one file. The only writer of the layout.
#[must_use]
pub fn render(file: &TreeFile) -> String {
    let mut top = vec![
        format!("  \"_generated\": {}", line(GENERATED)),
        format!("  \"format\": {FORMAT}"),
        format!("  \"uid\": {}", line(file.uid.as_str())),
    ];
    let key = match &file.body {
        Body::Table(t) => ("table", &t.table),
        Body::Role(n) => ("role", n),
        Body::Type(n) => ("type", n),
        Body::Extension(n) => ("extension", n),
    };
    top.push(format!("  \"{}\": {}", key.0, line(key.1)));
    if let Some(dropped) = &file.dropped {
        top.push(format!("  \"dropped\": {}", line(dropped)));
    }
    if let Body::Table(t) = &file.body {
        top.push(section("columns", &t.columns));
        if !t.partitions.is_empty() {
            top.push(section("partitions", &t.partitions));
        }
        if !t.moves.is_empty() {
            top.push(section("moves", &t.moves));
        }
        if !t.conversions.is_empty() {
            top.push(section("conversions", &t.conversions));
        }
        if !t.rebuilds.is_empty() {
            top.push(section("rebuilds", &t.rebuilds));
        }
    }
    format!("{{\n{}\n}}\n", top.join(",\n"))
}

// ---------------------------------------------------------------- loader ----

enum End {
    Missing,
    Last,
    NotLast,
}

/// A JSON object read as an ordered list. A duplicate key and the `_end`
/// sentinel are recorded rather than rejected, so the loader can name them.
struct Section<V> {
    records: Vec<(String, V)>,
    end: End,
    duplicate: Option<String>,
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Section<V> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Visit<V> {
            type Value = Section<V>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Section<V>, A::Error> {
                let mut s = Section {
                    records: Vec::new(),
                    end: End::Missing,
                    duplicate: None,
                };
                let mut seen = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "_end" {
                        if !map.next_value::<bool>()? {
                            return Err(serde::de::Error::custom("`_end` must be true"));
                        }
                        if !matches!(s.end, End::Missing) {
                            s.duplicate.get_or_insert(key);
                        }
                        s.end = End::Last;
                    } else {
                        let value = map.next_value::<V>()?;
                        if matches!(s.end, End::Last) {
                            s.end = End::NotLast;
                        }
                        if !seen.insert(key.clone()) {
                            s.duplicate.get_or_insert(key.clone());
                        }
                        s.records.push((key, value));
                    }
                }
                Ok(s)
            }
        }
        d.deserialize_map(Visit(PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(rename = "_generated")]
    _generated: String,
    format: u32,
    uid: Uid,
    table: Option<String>,
    role: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    extension: Option<String>,
    dropped: Option<Tombstone>,
    columns: Option<Section<Rec>>,
    partitions: Option<Section<Rec>>,
    moves: Option<Section<Move>>,
    conversions: Option<Section<Conversion>>,
    rebuilds: Option<Section<Rebuild>>,
}

fn malformed(path: &str, message: impl Into<String>) -> TreeError {
    TreeError::Malformed {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn read_error(path: &str, e: &serde_json::Error) -> TreeError {
    let message = e.to_string();
    // serde names a repeated field as "duplicate field `x`"; keep that apart.
    match message
        .strip_prefix("duplicate field `")
        .and_then(|m| m.split_once('`'))
    {
        Some((key, _)) => TreeError::DuplicateKey {
            path: path.to_owned(),
            key: key.to_owned(),
        },
        None => malformed(path, message),
    }
}

/// Unwraps a section into uid-keyed records, checking the sentinel, the
/// duplicates and the uid prefix.
fn keyed<V>(
    path: &str,
    name: &'static str,
    section: Option<Section<V>>,
    kind: UidKind,
    required: bool,
) -> Result<Vec<(Uid, V)>, TreeError> {
    let Some(section) = section else {
        return if required {
            Err(TreeError::SentinelMissing {
                path: path.to_owned(),
                section: name,
            })
        } else {
            Ok(Vec::new())
        };
    };
    match section.end {
        End::Missing => {
            return Err(TreeError::SentinelMissing {
                path: path.to_owned(),
                section: name,
            });
        }
        End::NotLast => {
            return Err(TreeError::SentinelNotLast {
                path: path.to_owned(),
                section: name,
            });
        }
        End::Last => {}
    }
    if let Some(key) = section.duplicate {
        return Err(TreeError::DuplicateKey {
            path: path.to_owned(),
            key,
        });
    }
    section
        .records
        .into_iter()
        .map(|(key, value)| {
            let uid: Uid = key
                .parse()
                .map_err(|e| malformed(path, format!("`{key}`: {e}")))?;
            if uid.kind() != kind {
                return Err(TreeError::KindMismatch {
                    path: path.to_owned(),
                    uid,
                });
            }
            Ok((uid, value))
        })
        .collect()
}

/// Reads one file strictly. Spacing is not checked: blank lines carry no
/// meaning, and a merge may leave one more or fewer.
pub fn parse(path: &str, text: &str) -> Result<TreeFile, TreeError> {
    if text
        .lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l.trim_end() == "=======")
    {
        return Err(TreeError::ConflictMarkers {
            path: path.to_owned(),
        });
    }
    let raw: Raw = serde_json::from_str(text).map_err(|e| read_error(path, &e))?;
    if raw.format != FORMAT {
        return Err(TreeError::Format {
            path: path.to_owned(),
            found: raw.format,
        });
    }
    let named = [&raw.table, &raw.role, &raw.ty, &raw.extension];
    if named.iter().filter(|n| n.is_some()).count() != 1 {
        return Err(malformed(
            path,
            "exactly one of `table`, `role`, `type` and `extension` is required",
        ));
    }
    let body = if let Some(table) = raw.table {
        Body::Table(TableBody {
            table,
            columns: keyed(path, "columns", raw.columns, UidKind::Column, true)?,
            partitions: keyed(path, "partitions", raw.partitions, UidKind::Table, false)?,
            moves: keyed(path, "moves", raw.moves, UidKind::Table, false)?,
            conversions: keyed(path, "conversions", raw.conversions, UidKind::Column, false)?,
            rebuilds: keyed(path, "rebuilds", raw.rebuilds, UidKind::Table, false)?,
        })
    } else {
        if raw.columns.is_some()
            || raw.partitions.is_some()
            || raw.moves.is_some()
            || raw.conversions.is_some()
            || raw.rebuilds.is_some()
        {
            return Err(malformed(path, "only a table file has sections"));
        }
        match (raw.role, raw.ty, raw.extension) {
            (Some(n), _, _) => Body::Role(n),
            (_, Some(n), _) => Body::Type(n),
            (_, _, Some(n)) => Body::Extension(n),
            _ => unreachable!("exactly one name was checked above"),
        }
    };
    let file = TreeFile {
        uid: raw.uid,
        body,
        dropped: raw.dropped,
    };
    check(path, &file)?;
    Ok(file)
}

/// The rules a file must keep whoever built it: the reader checks what it
/// read, and the writer checks what it is about to write.
fn check(path: &str, file: &TreeFile) -> Result<(), TreeError> {
    if file.uid.kind() != file.body.kind() {
        return Err(TreeError::KindMismatch {
            path: path.to_owned(),
            uid: file.uid.clone(),
        });
    }
    check_names(path, file)?;
    let Body::Table(t) = &file.body else {
        return Ok(());
    };
    let misplaced = |section, uid: &Uid| TreeError::Misplaced {
        path: path.to_owned(),
        section,
        uid: uid.clone(),
    };
    let live = |recs: &[(Uid, Rec)]| -> BTreeSet<Uid> {
        recs.iter()
            .filter(|(_, r)| matches!(r, Rec::Live(_)))
            .map(|(u, _)| u.clone())
            .collect()
    };
    let (columns, partitions) = (live(&t.columns), live(&t.partitions));
    if file.dropped.is_some()
        && !(columns.is_empty()
            && partitions.is_empty()
            && t.moves.is_empty()
            && t.conversions.is_empty())
    {
        return Err(TreeError::DroppedWithLive {
            path: path.to_owned(),
        });
    }
    if let Some((uid, _)) = t.conversions.iter().find(|(u, _)| !columns.contains(u)) {
        return Err(misplaced("conversions", uid));
    }
    if let Some((uid, _)) = t
        .moves
        .iter()
        .find(|(u, _)| u != &file.uid && !partitions.contains(u))
    {
        return Err(misplaced("moves", uid));
    }
    // The record is keyed by the source parent; the file is the replacement's.
    if let Some((uid, _)) = t
        .rebuilds
        .iter()
        .find(|(_, r)| r.replacement_root != file.uid)
    {
        return Err(misplaced("rebuilds", uid));
    }
    Ok(())
}

/// Every name the file stores must read back through the unchanged
/// `IdsFile` JSON, so each live one is held to the parser that JSON uses for
/// its kind. A column joins as `schema.table.column` and splits on `.`, so a
/// local column name that is empty or holds a `.` would validate here and then
/// fail in a plan or snapshot that embeds it; tables and types parse as
/// qualified names. Roles and extensions are standalone strings, and the
/// loader allows a `.` in them, so only the empty name is refused. A `was` is
/// never parsed anywhere (it is compared as text), so it is only required to
/// say something. `IdsFile` does not stop `ColumnRef::new` from building a
/// bad column name, so the writer runs this too.
fn check_names(path: &str, file: &TreeFile) -> Result<(), TreeError> {
    let bad = |name: &str, why: &dyn std::fmt::Display| TreeError::BadName {
        path: path.to_owned(),
        name: name.to_owned(),
        why: why.to_string(),
    };
    let nonempty = |name: &str| {
        if name.is_empty() {
            Err(bad(name, &"a name cannot be empty"))
        } else {
            Ok(())
        }
    };
    let table = |name: &str| {
        name.parse::<TableName>()
            .map(drop)
            .map_err(|e| bad(name, &e))
    };
    let column = |name: &str| {
        nonempty(name)?;
        crate::name::check_segment(name).map_err(|e| bad(name, &e))
    };
    if let Some(tomb) = &file.dropped {
        nonempty(&tomb.was)?;
    }
    // A dropped file's name line only repeats `was`, so it is not parsed.
    let live = file.dropped.is_none();
    match &file.body {
        Body::Role(n) | Body::Extension(n) => nonempty(n)?,
        Body::Type(n) if live => n.parse::<TypeRef>().map(drop).map_err(|e| bad(n, &e))?,
        Body::Type(n) => nonempty(n)?,
        Body::Table(t) => {
            if live {
                table(&t.table)?;
            } else {
                nonempty(&t.table)?;
            }
            for (_, rec) in &t.columns {
                match rec {
                    Rec::Live(name) => column(name)?,
                    Rec::Dead(tomb) => nonempty(&tomb.was)?,
                }
            }
            for (_, rec) in &t.partitions {
                match rec {
                    Rec::Live(name) => table(name)?,
                    Rec::Dead(tomb) => nonempty(&tomb.was)?,
                }
            }
        }
    }
    Ok(())
}

/// Reads a whole tree (relative path to file text) into the one mapping, and
/// validates the result.
pub fn assemble(files: &BTreeMap<String, String>) -> Result<IdsFile, TreeError> {
    let mut ids = IdsFile::default();
    // One namespace for identities; moves, conversions and rebuilds have
    // their own, since each is keyed by an uid an identity already uses.
    let mut seen: BTreeMap<(&'static str, Uid), String> = BTreeMap::new();
    let mut claim = |section: &'static str, uid: &Uid, path: &str| -> Result<(), TreeError> {
        match seen.insert((section, uid.clone()), path.to_owned()) {
            Some(first) => Err(TreeError::DuplicateUid {
                uid: uid.clone(),
                a: first,
                b: path.to_owned(),
            }),
            None => Ok(()),
        }
    };
    for (path, text) in files {
        let file = parse(path, text)?;
        claim("identity", &file.uid, path)?;
        let bad_name = |e: &dyn std::fmt::Display| malformed(path, format!("name: {e}"));
        match &file.body {
            Body::Role(n) if file.dropped.is_none() => {
                ids.roles
                    .insert(file.uid.clone(), RoleName::new(n.as_str()));
            }
            Body::Type(n) if file.dropped.is_none() => {
                let name: TypeRef = n.parse().map_err(|e| bad_name(&e))?;
                ids.types.insert(file.uid.clone(), name);
            }
            Body::Extension(n) if file.dropped.is_none() => {
                ids.extensions
                    .insert(file.uid.clone(), ObjectLocalName::new(n.as_str()));
            }
            Body::Table(t) => {
                // A dropped file has no live column to name a table for.
                let table: TableName = if file.dropped.is_none() {
                    t.table.parse().map_err(|e| bad_name(&e))?
                } else {
                    TableName::new("", "")
                };
                if file.dropped.is_none() {
                    ids.tables.insert(file.uid.clone(), table.clone());
                }
                for (uid, rec) in &t.columns {
                    claim("identity", uid, path)?;
                    match rec {
                        Rec::Live(local) => {
                            let column = ColumnRef::new(table.clone(), local.as_str());
                            ids.columns.insert(uid.clone(), column);
                        }
                        Rec::Dead(tomb) => {
                            ids.tombstones.insert(uid.clone(), tomb.clone());
                        }
                    }
                }
                for (uid, rec) in &t.partitions {
                    claim("identity", uid, path)?;
                    match rec {
                        Rec::Live(name) => {
                            ids.tables
                                .insert(uid.clone(), name.parse().map_err(|e| bad_name(&e))?);
                        }
                        Rec::Dead(tomb) => {
                            ids.tombstones.insert(uid.clone(), tomb.clone());
                        }
                    }
                }
                for (uid, m) in &t.moves {
                    claim("moves", uid, path)?;
                    ids.moves.insert(uid.clone(), m.clone());
                }
                for (uid, c) in &t.conversions {
                    claim("conversions", uid, path)?;
                    ids.conversions.insert(uid.clone(), c.clone());
                }
                for (uid, r) in &t.rebuilds {
                    claim("rebuilds", uid, path)?;
                    ids.rebuilds.insert(uid.clone(), r.clone());
                }
            }
            Body::Role(_) | Body::Type(_) | Body::Extension(_) => {}
        }
        if let Some(tomb) = &file.dropped {
            ids.tombstones.insert(file.uid.clone(), tomb.clone());
        }
    }
    ids.validate().map_err(TreeError::Ids)?;
    Ok(ids)
}

// ---------------------------------------------------------------- splitter --

/// Where each object lives, decided by the caller from the declaration paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// A table, role, type or extension that owns a file, to that file's path
    /// under the ids directory.
    pub paths: BTreeMap<Uid, String>,
    /// A partition child, to the parent table whose file declares it.
    pub children: BTreeMap<Uid, Uid>,
    /// The record order wanted in a table's `columns`; see [`next_order`].
    /// Records it omits follow in uid order.
    pub order: BTreeMap<Uid, Vec<Uid>>,
    /// The table a column tombstone belongs to, where its `was` name no longer
    /// finds it (the table was renamed after the drop).
    pub owners: BTreeMap<Uid, Uid>,
}

fn ordered<V>(mut records: Vec<(Uid, V)>, order: Option<&Vec<Uid>>) -> Vec<(Uid, V)> {
    let rank: BTreeMap<&Uid, usize> = order
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, u)| (u, i))
        .collect();
    records.sort_by(|(a, _), (b, _)| {
        let key = |u: &Uid| rank.get(u).copied().unwrap_or(usize::MAX);
        key(a).cmp(&key(b)).then_with(|| a.cmp(b))
    });
    records
}

/// Writes the mapping as a tree: relative path to canonical file text.
///
/// `assemble(split(x)) == x` for every valid mapping and placement.
pub fn split(ids: &IdsFile, placement: &Placement) -> Result<BTreeMap<String, String>, TreeError> {
    ids.validate().map_err(TreeError::Ids)?;
    let primary = |u: &Uid| placement.paths.contains_key(u);
    let no_path = |u: &Uid| TreeError::NoPath { uid: u.clone() };
    let tomb_name = |t: &Tombstone| t.was.clone();

    // A table file per primary table, a plain file per other object.
    let mut files: BTreeMap<Uid, TreeFile> = BTreeMap::new();
    let mut objects: Vec<(Uid, Body, Option<Tombstone>)> = Vec::new();
    for (uid, name) in &ids.tables {
        objects.push((
            uid.clone(),
            Body::Table(table_body(name.raw().to_string())),
            None,
        ));
    }
    for (uid, name) in &ids.roles {
        objects.push((uid.clone(), Body::Role(name.raw().to_owned()), None));
    }
    for (uid, name) in &ids.types {
        objects.push((uid.clone(), Body::Type(name.raw().to_string()), None));
    }
    for (uid, name) in &ids.extensions {
        objects.push((uid.clone(), Body::Extension(name.raw().to_owned()), None));
    }
    for (uid, tomb) in &ids.tombstones {
        let name = tomb_name(tomb);
        let body = match uid.kind() {
            UidKind::Table => Body::Table(table_body(name)),
            UidKind::Role => Body::Role(name),
            UidKind::Type => Body::Type(name),
            UidKind::Extension => Body::Extension(name),
            _ => continue,
        };
        objects.push((uid.clone(), body, Some(tomb.clone())));
    }
    for (uid, body, dropped) in objects {
        if primary(&uid) {
            files.insert(uid.clone(), TreeFile { uid, body, dropped });
        } else if !(matches!(body, Body::Table(_)) && placement.children.contains_key(&uid)) {
            return Err(no_path(&uid));
        }
    }
    if let Some(uid) = placement.paths.keys().find(|u| !files.contains_key(*u)) {
        return Err(TreeError::UnknownPath { uid: uid.clone() });
    }

    // Names to the tables that hold them, live and dropped, for ownership.
    let mut by_name: BTreeMap<TableName, Vec<Uid>> = BTreeMap::new();
    let mut live_by_name: BTreeMap<&TableName, &Uid> = BTreeMap::new();
    for (uid, name) in &ids.tables {
        by_name.entry(name.clone()).or_default().push(uid.clone());
        live_by_name.insert(name, uid);
    }
    for (uid, tomb) in &ids.tombstones {
        if let (UidKind::Table, Ok(name)) = (uid.kind(), tomb.was.parse::<TableName>()) {
            by_name.entry(name).or_default().push(uid.clone());
        }
    }
    let table_file = |uid: &Uid, files: &mut BTreeMap<Uid, TreeFile>| -> Result<(), TreeError> {
        match files.get(uid).map(|f| &f.body) {
            Some(Body::Table(_)) => Ok(()),
            _ => Err(no_path(uid)),
        }
    };
    let home = |uid: &Uid| -> Option<Uid> {
        if primary(uid) {
            Some(uid.clone())
        } else {
            placement.children.get(uid).filter(|p| primary(p)).cloned()
        }
    };
    let mut columns: BTreeMap<Uid, Vec<(Uid, Rec)>> = BTreeMap::new();
    for (uid, column) in &ids.columns {
        let owner = live_by_name
            .get(&column.table)
            .ok_or_else(|| no_path(uid))?;
        table_file(owner, &mut files)?;
        columns
            .entry((*owner).clone())
            .or_default()
            .push((uid.clone(), Rec::Live(column.name.raw().to_owned())));
    }
    let mut partitions: BTreeMap<Uid, Vec<(Uid, Rec)>> = BTreeMap::new();
    for (uid, tomb) in &ids.tombstones {
        match uid.kind() {
            UidKind::Column => {
                let owner = match placement.owners.get(uid) {
                    Some(owner) => owner.clone(),
                    None => {
                        let table = tomb.was.parse::<ColumnRef>().map(|c| c.table).ok();
                        match table.and_then(|t| by_name.get(&t)).map(Vec::as_slice) {
                            Some([only]) => only.clone(),
                            _ => return Err(TreeError::UnownedTombstone { uid: uid.clone() }),
                        }
                    }
                };
                table_file(&owner, &mut files)?;
                columns
                    .entry(owner)
                    .or_default()
                    .push((uid.clone(), Rec::Dead(tomb.clone())));
            }
            UidKind::Table if !primary(uid) => {
                let parent = placement.children.get(uid).ok_or_else(|| no_path(uid))?;
                table_file(parent, &mut files)?;
                partitions
                    .entry(parent.clone())
                    .or_default()
                    .push((uid.clone(), Rec::Dead(tomb.clone())));
            }
            UidKind::Table | UidKind::Role | UidKind::Type | UidKind::Extension => {}
        }
    }
    for (child, parent) in &placement.children {
        if let Some(name) = ids.tables.get(child) {
            table_file(parent, &mut files)?;
            partitions
                .entry(parent.clone())
                .or_default()
                .push((child.clone(), Rec::Live(name.raw().to_string())));
        }
    }

    let mut moves: BTreeMap<Uid, Vec<(Uid, Move)>> = BTreeMap::new();
    for (uid, m) in &ids.moves {
        let home = home(uid).ok_or_else(|| no_path(uid))?;
        moves
            .entry(home)
            .or_default()
            .push((uid.clone(), m.clone()));
    }
    let mut conversions: BTreeMap<Uid, Vec<(Uid, Conversion)>> = BTreeMap::new();
    for (uid, c) in &ids.conversions {
        // Validated: the column is live, so its table is.
        let owner = live_by_name[&ids.columns[uid].table];
        conversions
            .entry(owner.clone())
            .or_default()
            .push((uid.clone(), c.clone()));
    }
    let mut rebuilds: BTreeMap<Uid, Vec<(Uid, Rebuild)>> = BTreeMap::new();
    for (uid, r) in &ids.rebuilds {
        let root = &r.replacement_root;
        table_file(root, &mut files)?;
        rebuilds
            .entry(root.clone())
            .or_default()
            .push((uid.clone(), r.clone()));
    }

    let mut out = BTreeMap::new();
    let mut owner_of_path: BTreeMap<String, Uid> = BTreeMap::new();
    for (uid, mut file) in files {
        let path = &placement.paths[&uid];
        if let Some(other) = owner_of_path.insert(path.clone(), uid.clone()) {
            return Err(TreeError::PathCollision {
                path: path.clone(),
                a: other,
                b: uid,
            });
        }
        if let Body::Table(t) = &mut file.body {
            t.columns = ordered(
                columns.remove(&uid).unwrap_or_default(),
                placement.order.get(&uid),
            );
            t.partitions = ordered(partitions.remove(&uid).unwrap_or_default(), None);
            t.moves = ordered(moves.remove(&uid).unwrap_or_default(), None);
            t.conversions = ordered(conversions.remove(&uid).unwrap_or_default(), None);
            t.rebuilds = ordered(rebuilds.remove(&uid).unwrap_or_default(), None);
        }
        check(path, &file)?;
        out.insert(path.clone(), render(&file));
    }
    Ok(out)
}

fn table_body(table: String) -> TableBody {
    TableBody {
        table,
        ..TableBody::default()
    }
}

/// The record order for a table's `columns` after its declaration changed
/// (design-2600 §2.4 rule 1).
///
/// `existing` is the order on disk, `declared` the live columns in YAML order,
/// and `retired` the uids that are tombstones. A tombstone keeps its position;
/// a record that vanished without being retired is removed; a new column goes
/// right after its YAML predecessor, or first; and when the surviving live
/// columns changed order, they take the YAML order in the slots they hold.
#[must_use]
pub fn next_order(existing: &[Uid], declared: &[Uid], retired: &BTreeSet<Uid>) -> Vec<Uid> {
    let want: BTreeSet<&Uid> = declared.iter().collect();
    let mut order: Vec<Uid> = existing
        .iter()
        .filter(|u| want.contains(u) || retired.contains(u))
        .cloned()
        .collect();
    for (i, uid) in declared.iter().enumerate() {
        if order.contains(uid) {
            continue;
        }
        let at = i
            .checked_sub(1)
            .and_then(|p| order.iter().position(|u| u == &declared[p]))
            .map_or(0, |p| p + 1);
        order.insert(at, uid.clone());
    }
    let slots: Vec<usize> = (0..order.len())
        .filter(|&i| want.contains(&order[i]))
        .collect();
    for (slot, uid) in slots.into_iter().zip(declared) {
        order[slot] = uid.clone();
    }
    order
}

#[cfg(test)]
pub(crate) mod tests;

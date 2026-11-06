//! A three-way merge of one identity file by uid (#2600, design §5).
//!
//! `pbps ids resolve` reads base, ours and theirs of a path git left unmerged
//! and asks this module for the one file that follows. Git already merged
//! every record it could line by line; what is left are records the two sides
//! changed differently, and files one side deleted or both added. A record is
//! decided by what each side did to it: a side that did not change it yields to
//! the side that did. Two different changes are decided by the resolved YAML
//! where the YAML can say (the name it declares), and otherwise refused. There
//! is no "take ours" and no "take theirs": choosing a side by hand is how a
//! duplicate name gets in (design §5).
//!
//! It does no I/O and writes nothing; [`render`](crate::ids_tree::render) turns
//! the result into bytes.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ids::Tombstone;
use crate::ids_tree::{Body, Rec, TableBody, TreeFile, next_order};
use crate::uid::Uid;

/// What the resolved YAML says about the file's declaration.
#[derive(Debug, Clone, Default)]
pub struct YamlView {
    /// The name the declaration at this path spells; `None` when none does.
    pub name: Option<String>,
    /// A table's columns by local name, in YAML order.
    pub columns: Vec<String>,
}

/// A record the two sides changed differently, with both sides shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub uid: Uid,
    pub section: &'static str,
    pub ours: String,
    pub theirs: String,
    pub remedy: &'static str,
}

const REMEDY: &str = "state the intent (`renamed_from` in the YAML, `pbps rename`, `pbps drop`), then run `pbps ids resolve` again";

fn show<T: Serialize>(value: Option<&T>) -> String {
    value.map_or_else(
        || "(absent)".to_owned(),
        |v| serde_json::to_string(v).expect("plain data serializes"),
    )
}

/// The side that changed, or `Err` when both changed it differently.
fn pick<'a, T: PartialEq>(
    base: Option<&'a T>,
    ours: Option<&'a T>,
    theirs: Option<&'a T>,
) -> Result<Option<&'a T>, ()> {
    if ours == theirs || theirs == base {
        Ok(ours)
    } else if ours == base {
        Ok(theirs)
    } else {
        Err(())
    }
}

/// One section by uid. `decide` may settle two different live values.
fn records<V: Clone + PartialEq + Serialize>(
    section: &'static str,
    base: &[(Uid, V)],
    ours: &[(Uid, V)],
    theirs: &[(Uid, V)],
    decide: &dyn Fn(&V, &V) -> Option<V>,
    conflicts: &mut Vec<Conflict>,
) -> Vec<(Uid, V)> {
    let index = |side: &[(Uid, V)]| side.iter().cloned().collect::<BTreeMap<_, _>>();
    let (b, o, t) = (index(base), index(ours), index(theirs));
    // Ours first, in its order, then what only theirs holds: a stable base for
    // the order the caller restores from the YAML.
    let mut uids: Vec<&Uid> = ours.iter().map(|(u, _)| u).collect();
    uids.extend(
        theirs
            .iter()
            .map(|(u, _)| u)
            .filter(|u| !o.contains_key(*u)),
    );
    let mut out = Vec::new();
    for uid in uids {
        let (bv, ov, tv) = (b.get(uid), o.get(uid), t.get(uid));
        let chosen = match pick(bv, ov, tv) {
            Ok(v) => v.cloned(),
            Err(()) => match (ov, tv) {
                (Some(x), Some(y)) => decide(x, y),
                _ => None,
            }
            .or_else(|| {
                conflicts.push(Conflict {
                    uid: uid.clone(),
                    section,
                    ours: show(ov),
                    theirs: show(tv),
                    remedy: REMEDY,
                });
                None
            }),
        };
        if let Some(value) = chosen {
            out.push((uid.clone(), value));
        }
    }
    // A record only one side deleted and the other left alone is gone: `pick`
    // yields `None` for it above and nothing is pushed.
    out
}

fn settle(
    uid: &Uid,
    section: &'static str,
    base: Option<&String>,
    ours: &String,
    theirs: &String,
    yaml: Option<&String>,
    conflicts: &mut Vec<Conflict>,
) -> String {
    match pick(base, Some(ours), Some(theirs)) {
        Ok(v) => v.cloned().unwrap_or_else(|| ours.clone()),
        Err(()) => match yaml {
            Some(y) if y == ours || y == theirs => y.clone(),
            _ => {
                conflicts.push(Conflict {
                    uid: uid.clone(),
                    section,
                    ours: show(Some(ours)),
                    theirs: show(Some(theirs)),
                    remedy: REMEDY,
                });
                ours.clone()
            }
        },
    }
}

fn name_of(body: &Body) -> (&'static str, &str) {
    match body {
        Body::Table(t) => ("table", &t.table),
        Body::Role(n) => ("role", n),
        Body::Type(n) => ("type", n),
        Body::Extension(n) => ("extension", n),
    }
}

/// The file that follows, `None` when it is deleted, or every conflict.
///
/// A path one side deleted and the other left alone is deleted. One side
/// deleted and the other changed it is refused, as is two files of different
/// identity at one path. A file both sides added with one identity merges from
/// an empty base.
pub fn merge(
    base: Option<&TreeFile>,
    ours: Option<&TreeFile>,
    theirs: Option<&TreeFile>,
    yaml: &YamlView,
) -> Result<Option<TreeFile>, Vec<Conflict>> {
    let (ours, theirs) = match (ours, theirs) {
        (None, None) => return Ok(None),
        (Some(only), None) | (None, Some(only)) => {
            let deleted_side_is_ours = ours.is_none();
            return if Some(only) == base {
                Ok(None)
            } else if base.is_none() {
                Ok(Some(only.clone()))
            } else {
                let (gone, kept) = ("(deleted)".to_owned(), show_file(only));
                let (o, t) = if deleted_side_is_ours {
                    (gone, kept)
                } else {
                    (kept, gone)
                };
                Err(vec![Conflict {
                    uid: only.uid.clone(),
                    section: "file",
                    ours: o,
                    theirs: t,
                    remedy: REMEDY,
                }])
            };
        }
        (Some(o), Some(t)) => (o, t),
    };
    let same_kind = name_of(&ours.body).0 == name_of(&theirs.body).0;
    if ours.uid != theirs.uid || !same_kind {
        return Err(vec![Conflict {
            uid: ours.uid.clone(),
            section: "uid",
            ours: show_file(ours),
            theirs: show_file(theirs),
            remedy: REMEDY,
        }]);
    }
    let mut conflicts = Vec::new();
    let uid = &ours.uid;
    let base = base.filter(|b| b.uid == *uid);
    let base_name = base.map(|b| name_of(&b.body).1.to_owned());
    let (kind, o_name) = name_of(&ours.body);
    let name = settle(
        uid,
        "name",
        base_name.as_ref(),
        &o_name.to_owned(),
        &name_of(&theirs.body).1.to_owned(),
        yaml.name.as_ref(),
        &mut conflicts,
    );
    let dropped: Option<Tombstone> = match pick(
        base.and_then(|b| b.dropped.as_ref()),
        ours.dropped.as_ref(),
        theirs.dropped.as_ref(),
    ) {
        Ok(v) => v.cloned(),
        Err(()) => {
            conflicts.push(Conflict {
                uid: uid.clone(),
                section: "dropped",
                ours: show(ours.dropped.as_ref()),
                theirs: show(theirs.dropped.as_ref()),
                remedy: REMEDY,
            });
            None
        }
    };
    let body = match (&ours.body, &theirs.body) {
        (Body::Table(o), Body::Table(t)) => {
            let b = match base.map(|b| &b.body) {
                Some(Body::Table(b)) => b.clone(),
                _ => TableBody::default(),
            };
            Body::Table(merge_table(&name, &b, o, t, yaml, &mut conflicts))
        }
        _ => match kind {
            "role" => Body::Role(name),
            "type" => Body::Type(name),
            _ => Body::Extension(name),
        },
    };
    if conflicts.is_empty() {
        Ok(Some(TreeFile {
            uid: uid.clone(),
            body,
            dropped,
        }))
    } else {
        Err(conflicts)
    }
}

/// Never settles two different values: only the YAML can, and only for a name.
fn never<V>(_: &V, _: &V) -> Option<V> {
    None
}

/// The sections a file lists by uid are written in uid order (`split`'s rule).
fn sorted<V>(mut records: Vec<(Uid, V)>) -> Vec<(Uid, V)> {
    records.sort_by(|a, b| a.0.cmp(&b.0));
    records
}

fn show_file(file: &TreeFile) -> String {
    format!("{} {}", file.uid, name_of(&file.body).1)
}

fn merge_table(
    name: &str,
    base: &TableBody,
    ours: &TableBody,
    theirs: &TableBody,
    yaml: &YamlView,
    conflicts: &mut Vec<Conflict>,
) -> TableBody {
    // Two live local names for one column: the YAML keeps the one it declares.
    let by_yaml = |a: &Rec, b: &Rec| match (a, b) {
        (Rec::Live(x), Rec::Live(y)) => {
            let (kx, ky) = (yaml.columns.contains(x), yaml.columns.contains(y));
            match (kx, ky) {
                (true, false) => Some(a.clone()),
                (false, true) => Some(b.clone()),
                (true, true) | (false, false) => None,
            }
        }
        (Rec::Live(_), Rec::Dead(_)) | (Rec::Dead(_), _) => None,
    };
    let mut columns = records(
        "columns",
        &base.columns,
        &ours.columns,
        &theirs.columns,
        &by_yaml,
        conflicts,
    );
    let partitions = records(
        "partitions",
        &base.partitions,
        &ours.partitions,
        &theirs.partitions,
        &never,
        conflicts,
    );
    let moves = records(
        "moves",
        &base.moves,
        &ours.moves,
        &theirs.moves,
        &never,
        conflicts,
    );
    let conversions = records(
        "conversions",
        &base.conversions,
        &ours.conversions,
        &theirs.conversions,
        &never,
        conflicts,
    );
    let rebuilds = records(
        "rebuilds",
        &base.rebuilds,
        &ours.rebuilds,
        &theirs.rebuilds,
        &never,
        conflicts,
    );
    // Back to the YAML's order (design §2.4 rule 1), through the one rule the
    // writer uses. With no YAML view there is no order to restore, and the
    // order the sides agree on stands.
    if !yaml.columns.is_empty() {
        restore_order(&mut columns, yaml);
    }
    TableBody {
        table: name.to_owned(),
        columns,
        partitions,
        moves: sorted(moves),
        conversions: sorted(conversions),
        rebuilds: sorted(rebuilds),
    }
}

fn restore_order(columns: &mut [(Uid, Rec)], yaml: &YamlView) {
    let existing: Vec<Uid> = columns.iter().map(|(u, _)| u.clone()).collect();
    let retired: BTreeSet<Uid> = columns
        .iter()
        .filter(|(_, r)| matches!(r, Rec::Dead(_)))
        .map(|(u, _)| u.clone())
        .collect();
    let live: BTreeMap<&str, &Uid> = columns
        .iter()
        .filter_map(|(u, r)| match r {
            Rec::Live(n) => Some((n.as_str(), u)),
            Rec::Dead(_) => None,
        })
        .collect();
    let declared: Vec<Uid> = yaml
        .columns
        .iter()
        .filter_map(|n| live.get(n.as_str()).map(|u| (*u).clone()))
        .collect();
    let order = next_order(&existing, &declared, &retired);
    let rank: BTreeMap<&Uid, usize> = order.iter().enumerate().map(|(i, u)| (u, i)).collect();
    columns.sort_by_key(|(u, _)| rank.get(u).copied().unwrap_or(usize::MAX));
}

#[cfg(test)]
mod tests;

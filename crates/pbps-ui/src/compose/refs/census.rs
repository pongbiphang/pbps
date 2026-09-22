//! Git enumeration omits some broken refs, so raw evidence must corroborate it.

use std::collections::BTreeMap;
use std::path::Path;

use rustix::fs::AtFlags;

use super::super::{
    Error, Result,
    durable::{Directory, ResourceObserver},
    git::{Git, text},
    record::{identity, oid},
};

const PREFIX: &str = "refs/pbps-compose";

fn refusal(reference: &str) -> Error {
    Error::new(&format!(
        "Private compose ref evidence at {reference} is incomplete or unreadable; preserve it for manual recovery"
    ))
}

fn child(parent: &Directory, name: &str) -> Result<Option<Directory>> {
    parent.check()?;
    match rustix::fs::statat(&parent.file, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Ok(_) => {
            // Git directories need not have the private store's 0700 mode.
            // Opening still refuses symlinks, and both identities are checked.
            let directory = Directory::open(&parent.path.join(name), parent.observer.clone())?;
            parent.check()?;
            Ok(Some(directory))
        }
        Err(_) => Err(refusal(&parent.path.join(name).display().to_string())),
    }
}

pub(in super::super) fn parts(reference: &str) -> Result<(&str, &str)> {
    let Some((operation, kind)) = reference
        .strip_prefix("refs/pbps-compose/")
        .and_then(|tail| tail.split_once('/'))
    else {
        return Err(refusal(reference));
    };
    if !identity(operation) || !matches!(kind, "base" | "commit") {
        return Err(refusal(reference));
    }
    Ok((operation, kind))
}

fn insert(pins: &mut BTreeMap<String, String>, reference: &str, value: &str) -> Result<()> {
    parts(reference)?;
    if !oid(value) {
        return Err(refusal(reference));
    }
    pins.insert(reference.into(), value.into());
    // At most two roots per admitted resource record.
    if pins.len() > 2 * 32768 {
        return Err(refusal(PREFIX));
    }
    Ok(())
}

pub(in super::super) fn read(
    common: &Path,
    observer: ResourceObserver,
) -> Result<BTreeMap<String, String>> {
    let root = Directory::open(common, observer)?;
    let mut pins = BTreeMap::new();
    // Read the physical packed table too: Git may suppress malformed names or
    // dangling symbolic refs from for-each-ref. Only private names are text;
    // unrelated Git refs may contain non-UTF-8 bytes.
    if let Some(bytes) = root.read("packed-refs", 64 * 1024 * 1024)? {
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(refusal("packed-refs"));
        }
        for line in bytes
            .split_inclusive(|b| *b == b'\n')
            .map(|line| &line[..line.len() - 1])
        {
            if line.starts_with(b"#") {
                continue;
            }
            if let Some(peeled) = line.strip_prefix(b"^") {
                if !std::str::from_utf8(peeled).is_ok_and(oid) {
                    return Err(refusal("packed-refs"));
                }
                continue;
            }
            let Some(space) = line.iter().position(|b| *b == b' ') else {
                return Err(refusal("packed-refs"));
            };
            let (value, name) = (&line[..space], &line[space + 1..]);
            let value = std::str::from_utf8(value).map_err(|_| refusal("packed-refs"))?;
            if !oid(value) || name.is_empty() {
                return Err(refusal("packed-refs"));
            }
            if name == PREFIX.as_bytes() || name.starts_with(b"refs/pbps-compose/") {
                let name = std::str::from_utf8(name).map_err(|_| refusal(PREFIX))?;
                if pins.contains_key(name) {
                    return Err(refusal(name));
                }
                insert(&mut pins, name, value)?;
            }
        }
    }
    if let Some(refs) = child(&root, "refs")?
        && let Some(namespace) = child(&refs, "pbps-compose")?
    {
        for operation in namespace.names()? {
            if !identity(&operation) {
                return Err(refusal(&format!("{PREFIX}/{operation}")));
            }
            let directory = child(&namespace, &operation)?
                .ok_or_else(|| refusal(&format!("{PREFIX}/{operation}")))?;
            for kind in directory.names()? {
                let reference = format!("{PREFIX}/{operation}/{kind}");
                parts(&reference)?;
                let bytes = directory
                    .read(&kind, 4096)
                    .map_err(|_| refusal(&reference))?
                    .ok_or_else(|| refusal(&reference))?;
                let value = std::str::from_utf8(&bytes).map_err(|_| refusal(&reference))?;
                // A symbolic ref is evidence even when Git omits its missing
                // target. Never dereference it or infer a cleanup capability.
                insert(
                    &mut pins,
                    &reference,
                    value.strip_suffix('\n').unwrap_or(value),
                )?;
            }
            directory.check()?;
        }
        namespace.check()?;
        refs.check()?;
    }
    root.check()?;
    Ok(pins)
}

pub(in super::super) fn verify(git: &Git, physical: &BTreeMap<String, String>) -> Result<()> {
    let output = git.output(
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            "--",
            PREFIX,
        ],
        &[],
        None,
    )?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(refusal(PREFIX));
    }
    let mut visible = BTreeMap::new();
    if !output.stdout.is_empty() {
        for line in text(output.stdout)?.lines() {
            let fields: Vec<_> = line.split('\0').collect();
            if fields.len() != 3 || !fields[2].is_empty() {
                return Err(refusal(PREFIX));
            }
            insert(&mut visible, fields[0], fields[1])?;
        }
    }
    if visible != *physical {
        return Err(refusal(PREFIX));
    }
    Ok(())
}

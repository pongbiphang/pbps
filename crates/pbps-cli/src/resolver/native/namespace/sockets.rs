//! Socket holder observations, not an exhaustive inventory (DECISIONS 533).

use super::*;
use crate::resolver::native::{Reading, proc_base};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
#[path = "socket_tests.rs"]
mod tests;

/// Observe thread groups through the anchor's procfs view. Reparenting does
/// not remove a group from this view. Shared descriptors in several threads
/// count once; a worker's separate descriptor table is still inspected.
///
/// A zero result gets one fresh observation for a backend created after the
/// first directory enumeration. Only absence is retried: unreadability and
/// observed ambiguity refuse immediately. Neither pass proves completeness
/// against concurrent fork or descriptor transfer by the trusted environment.
pub(crate) fn observed_socket_holders(
    anchor: &ProcessLease,
    inode: u64,
) -> Result<Vec<ProcessLease>, UnqualifiedProcess> {
    observe_with(anchor, inode, |_| {})
}

fn observe_with(
    anchor: &ProcessLease,
    inode: u64,
    mut before_task: impl FnMut(u32),
) -> Result<Vec<ProcessLease>, UnqualifiedProcess> {
    let view = NamespaceProcfs::capture(anchor)?;
    let expected = std::path::PathBuf::from(format!("socket:[{inode}]"));
    for attempt in 0..2 {
        let mut groups = BTreeMap::new();
        view.visit(|task| {
            before_task(task.id.number);
            let result = (|| {
                if matches!(task.status()?, TaskReading::Exited) {
                    return Ok(());
                }
                let descriptors = open(&task.directory, "fd", OFlags::DIRECTORY)
                    .map_err(Reading::ScopeDescriptors.named())?;
                let mut holds = false;
                for entry in rustix::fs::Dir::read_from(&descriptors)
                    .map_err(Reading::ScopeDescriptors.named())?
                {
                    let entry = entry.map_err(Reading::ScopeDescriptors.named())?;
                    let name = entry.file_name().to_bytes();
                    if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
                        continue;
                    }
                    match rustix::fs::readlinkat(&descriptors, entry.file_name(), Vec::new()) {
                        Ok(path) => {
                            holds |= path.to_bytes() == expected.as_os_str().as_encoded_bytes()
                        }
                        // An FD may close while its live table is read. Other
                        // errors do not mean that this task holds nothing.
                        Err(rustix::io::Errno::NOENT) => (),
                        Err(_) => return Err(Reading::ScopeDescriptors.refuse()),
                    }
                }
                if holds && !groups.contains_key(&task.group) {
                    let directory = open(
                        &view.directory,
                        &task.group.number.to_string(),
                        OFlags::DIRECTORY,
                    )
                    .map_err(Reading::CaptureOpen.named())?;
                    let group = ProcessLease::capture_held(
                        directory,
                        ProcSource::Namespace(Arc::clone(&view.directory)),
                    )?;
                    // A live member pins its TGID. Bracketing through the
                    // held member prevents a reused group number substituting
                    // a different process during this lookup.
                    if matches!(task.status()?, TaskReading::Exited) {
                        return Ok(());
                    }
                    group.check()?;
                    groups.insert(task.group, group);
                    // Every caller permits at most three groups (the fixed
                    // bash/cat/cat forwarder). A fourth already refuses and
                    // must not consume one lease per arbitrary extra holder.
                    if groups.len() > 3 {
                        return Err(Reading::OwnerCount(groups.len()).refuse());
                    }
                }
                task.status()?;
                Ok(())
            })();
            if let Err(error) = result
                && !matches!(task.status()?, TaskReading::Exited)
            {
                return Err(error);
            }
            Ok(())
        })?;
        if !groups.is_empty() || attempt == 1 {
            for group in groups.values() {
                group.check()?;
            }
            return Ok(groups.into_values().collect());
        }
    }
    unreachable!("the second observation returns even when empty")
}

/// Positive service/backend relation, independent of the namespace census.
/// A process in the same namespace is not necessarily part of this service.
/// Walk upward only from the observed holder, retaining every parent's lease
/// through the final checks; no descendant-list completeness is inferred.
pub(crate) fn belongs_to_service(
    owner: &ProcessLease,
    service: &ProcessLease,
) -> Result<bool, UnqualifiedProcess> {
    relation_with(owner, service, |_| {})
}

fn relation_with(
    owner: &ProcessLease,
    service: &ProcessLease,
    mut after_parent: impl FnMut(u32),
) -> Result<bool, UnqualifiedProcess> {
    if !owner.same_namespace(service, "pid")? {
        return Ok(false);
    }
    let ProcSource::Namespace(view) = &owner.source else {
        return Err(Reading::Scope.refuse());
    };
    let mut visited = BTreeSet::new();
    let mut ancestors: Vec<ProcessLease> = Vec::new();
    loop {
        let child = ancestors.last().unwrap_or(owner);
        if child.same_process(service)? {
            owner.check()?;
            // Live endpoints do not preserve their relation: an intermediate
            // exit reparents the holder while both endpoints survive (#766).
            for ancestor in &ancestors {
                ancestor.check()?;
            }
            service.check()?;
            return Ok(true);
        }
        child.check()?;
        let parent = parent_id(child)?;
        if parent == 0 || !visited.insert(parent) {
            return Ok(false);
        }
        let directory =
            open(view, &parent.to_string(), OFlags::DIRECTORY).map_err(Reading::Scope.named())?;
        let next = ProcessLease::capture_held(directory, ProcSource::Namespace(Arc::clone(view)))?;
        child.check()?;
        if parent_id(child)? != parent {
            return Err(Reading::Scope.refuse());
        }
        ancestors.push(next);
        after_parent(parent);
    }
}

fn parent_id(process: &ProcessLease) -> Result<u32, UnqualifiedProcess> {
    let stat = std::fs::read_to_string(proc_base(&process.directory).join("stat"))
        .map_err(Reading::Scope.named())?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(1))
        .and_then(|parent| parent.parse().ok())
        .ok_or_else(|| Reading::Scope.refuse())
}

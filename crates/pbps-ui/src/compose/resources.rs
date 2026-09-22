//! Durable ownership is acquired explicitly; uncertain acquisition is retained.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{
    Error, Result,
    durable::{self, Directory, Identity, ResourceObserver, ResourceOperation},
    git::Git,
    inventory::Inventory,
    record::{RepositoryIdentity, identity, oid},
    refs::{self, RefEvidence},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Capturing,
    Sealed,
    Confirmed,
    Retiring,
    Retained,
    Spent,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceReport {
    pub operation_id: String,
    pub state: ResourceState,
    pub cleanup_pending: bool,
    pub location: PathBuf,
    pub instruction: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    target: String,
    value: String,
    owned: bool,
    retired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    version: u32,
    operation: String,
    created: u64,
    repository: RepositoryIdentity,
    state: ResourceState,
    snapshot: Option<Identity>,
    snapshot_retired: bool,
    binding: Option<String>,
    inventory: Option<Inventory>,
    pins: BTreeMap<String, Pin>,
    keep_commit: bool,
}

pub(super) struct Resources {
    root: Directory,
    records: Directory,
    snapshots: Directory,
    repository: RepositoryIdentity,
    git: Git,
}

impl Resources {
    pub fn open(git: &Git, observer: ResourceObserver) -> Result<Self> {
        durable::refuse_legacy(git)?;
        let repository = RepositoryIdentity::capture(git)?;
        let root = durable::store(&repository.common, observer)?;
        let records = root.child("resources", true)?;
        let snapshots = root.child("snapshots", true)?;
        let git = Git {
            root: git.root.clone(),
            hooks: git.hooks.clone(),
            deadline: git.deadline,
        };
        Ok(Self {
            root,
            records,
            snapshots,
            repository,
            git,
        })
    }

    fn locked<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lease = self.root.lock("resources.lock")?;
        lease.check()?;
        let result = operation();
        lease.check()?;
        lease.release()?;
        result
    }

    fn binding(&self) -> Result<()> {
        self.root.check()?;
        if RepositoryIdentity::capture(&self.git)? != self.repository {
            return Err(Error::new(
                "Compose resource repository changed; preserve the original evidence",
            ));
        }
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Resource> {
        let r = self.read(id)?;
        if !r.repository.source_scope(&self.repository)? {
            return Err(Error::new(
                "Compose resource belongs to another source worktree",
            ));
        }
        Ok(r)
    }

    fn read(&self, id: &str) -> Result<Resource> {
        if !identity(id) {
            return Err(Error::new("Invalid resource operation identity"));
        }
        self.binding()?;
        for name in self.records.names()? {
            if !name.strip_suffix(".json").is_some_and(identity) {
                return Err(Error::new(&format!(
                    "Unacknowledged resource evidence at {}; preserve it for manual recovery",
                    self.records.path.join(name).display()
                )));
            }
        }
        let bytes = self
            .records
            .read(&format!("{id}.json"), 64 * 1024 * 1024)?
            .ok_or_else(|| {
                Error::new("Compose resource ownership is unavailable; preserve its evidence")
            })?;
        let r: Resource = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new("Unknown compose resource evidence; preserve it for manual recovery")
        })?;
        self.validate(&r, id)?;
        Ok(r)
    }

    fn validate(&self, r: &Resource, id: &str) -> Result<()> {
        r.repository.source_scope(&self.repository)?;
        if !identity(id)
            || r.version != 1
            || r.operation != id
            || r.pins.iter().any(|(kind, p)| {
                !matches!(kind.as_str(), "base" | "commit") || !oid(&p.target) || !oid(&p.value)
            })
        {
            return Err(Error::new(
                "Compose resource ownership does not match this repository",
            ));
        }
        let base = r.pins.get("base");
        let commit = r.pins.get("commit");
        let sealed = r
            .snapshot
            .as_ref()
            .zip(r.inventory.as_ref())
            .is_some_and(|(identity, inventory)| identity == &inventory.root);
        let complete_base = base.is_some_and(|pin| pin.owned && !pin.retired);
        let shape = match r.state {
            ResourceState::Capturing => {
                !r.snapshot_retired
                    && r.inventory.is_none()
                    && commit.is_none()
                    && (base.is_none() || r.snapshot.is_some())
            }
            ResourceState::Sealed => {
                !r.snapshot_retired && sealed && complete_base && commit.is_none()
            }
            ResourceState::Confirmed => {
                !r.snapshot_retired
                    && sealed
                    && complete_base
                    && commit.is_none_or(|pin| !pin.retired)
            }
            ResourceState::Retiring => {
                (sealed || (r.snapshot_retired && r.snapshot.is_none() && r.inventory.is_none()))
                    && base.is_some_and(|pin| pin.owned)
                    && commit.is_none_or(|pin| pin.owned)
                    && (!r.keep_commit || commit.is_some_and(|pin| !pin.retired))
            }
            ResourceState::Retained => {
                r.snapshot_retired
                    && r.snapshot.is_none()
                    && r.inventory.is_none()
                    && r.keep_commit
                    && base.is_some_and(|pin| pin.owned && pin.retired)
                    && commit.is_some_and(|pin| pin.owned && !pin.retired)
            }
            ResourceState::Spent => {
                !r.keep_commit
                    && r.snapshot_retired
                    && r.snapshot.is_none()
                    && r.inventory.is_none()
                    && r.pins.is_empty()
            }
        };
        let binding = match r.state {
            ResourceState::Capturing | ResourceState::Spent => r.binding.is_none(),
            ResourceState::Sealed
            | ResourceState::Confirmed
            | ResourceState::Retiring
            | ResourceState::Retained => r.binding.as_deref().is_some_and(identity),
        };
        if !shape || !binding {
            return Err(Error::new(
                "Compose resource lifecycle evidence is incomplete; preserve it",
            ));
        }
        Ok(())
    }

    fn save(&self, r: &Resource) -> Result<()> {
        self.binding()?;
        self.validate(r, &r.operation)?;
        if !r.repository.source_scope(&self.repository)? {
            return Err(Error::new(
                "Cannot write another source worktree's resource evidence",
            ));
        }
        let bytes =
            serde_json::to_vec(r).map_err(|_| Error::new("Cannot encode resource ownership"))?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(Error::new(
                "Compose resource inventory exceeds its size limit",
            ));
        }
        self.records.save(&format!("{}.json", r.operation), &bytes)
    }

    pub fn begin(&self, id: &str, base: &str) -> Result<PathBuf> {
        self.locked(|| {
            if self.records.names()?.len() >= 32768 {
                return Err(Error::new(
                    "Compose resource record limit reached; retain existing recovery evidence",
                ));
            }
            if !identity(id)
                || !oid(base)
                || self
                    .records
                    .read(&format!("{id}.json"), 64 * 1024 * 1024)?
                    .is_some()
            {
                return Err(Error::new(
                    "Compose operation resource identity already exists or is invalid",
                ));
            }
            let mut r = Resource {
                version: 1,
                operation: id.into(),
                created: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::new("Cannot timestamp candidate lifetime"))?
                    .as_secs(),
                repository: self.repository.clone(),
                state: ResourceState::Capturing,
                snapshot: None,
                snapshot_retired: false,
                binding: None,
                inventory: None,
                pins: BTreeMap::new(),
                keep_commit: false,
            };
            self.save(&r)?;
            let directory = self.snapshots.create_owned(id)?;
            r.snapshot = Some(directory.identity()?);
            self.save(&r)?;
            self.pin(&mut r, "base", base)?;
            Ok(directory.path)
        })
    }

    fn pin_ref(id: &str, kind: &str) -> String {
        format!("refs/pbps-compose/{id}/{kind}")
    }

    fn sync_path(&self, path: &Path) -> Result<()> {
        self.root
            .observer
            .at(ResourceOperation::Sync, false, path)?;
        let fd = rustix::fs::openat2(
            rustix::fs::CWD,
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::new("Git durability evidence is unavailable"))?;
        File::from(fd)
            .sync_all()
            .map_err(|_| Error::new("Git evidence flush is unresolved"))?;
        self.root.observer.at(ResourceOperation::Sync, true, path)?;
        self.binding()
    }

    fn flush_ref(&self, reference: &str) -> Result<()> {
        // Git fsyncs ref contents, but its lockfile rename does not establish
        // directory-entry durability. Flush each affected parent explicitly.
        let path = self.repository.common.join(reference);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => self.sync_path(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Maintenance can pack a ref between its acknowledged write
                // and restart. Absence of its loose file is not ref absence.
                if !matches!(refs::observe(&self.git, reference), RefEvidence::Direct(_)) {
                    return Err(Error::new("Git reference durability is unresolved"));
                }
                self.sync_path(&self.repository.common.join("packed-refs"))?;
                return self.sync_path(&self.repository.common);
            }
            Err(_) => return Err(Error::new("Cannot inspect Git reference durability")),
        }
        let mut parent = path.parent();
        while let Some(path) = parent {
            self.sync_path(path)?;
            if path == self.repository.common {
                break;
            }
            parent = path.parent();
        }
        Ok(())
    }

    pub fn flush_publication_ref(&self, reference: &str) -> Result<()> {
        self.flush_ref(reference)
    }

    pub fn flush_import(&self) -> Result<()> {
        let objects = PathBuf::from(self.git.line(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?);
        self.sync_path(&objects.join("pack"))?;
        self.sync_path(&objects)
    }

    pub fn flush_object(&self, value: &str) -> Result<()> {
        if !oid(value) {
            return Err(Error::new("Invalid object durability identity"));
        }
        let objects = PathBuf::from(self.git.line(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?);
        let loose = objects.join(&value[..2]).join(&value[2..]);
        match std::fs::symlink_metadata(&loose) {
            Ok(_) => {
                self.sync_path(&loose)?;
                self.sync_path(loose.parent().unwrap())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // An existing packed object needs no new loose entry. A missing
                // object is never accepted merely because its loose path is absent.
                self.git.bytes(&["cat-file", "-e", value], &[], None)?;
                self.sync_path(&objects.join("pack"))?;
            }
            Err(_) => return Err(Error::new("Cannot inspect Git object durability")),
        }
        self.sync_path(&objects)
    }

    fn localize_pin_target(&self, target: &str) -> Result<()> {
        let objects = PathBuf::from(self.git.line(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?);
        let alternate = match std::fs::symlink_metadata(objects.join("info/alternates")) {
            Ok(metadata) => metadata.len() != 0,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(Error::new("Cannot inspect borrowed Git object evidence")),
        };
        let packs = objects.join("pack");
        let mut promised = false;
        // Git can use an entirely loose store without this optional directory.
        // Inspect the entry before reading it so a dangling link is not absence.
        let entries = match std::fs::symlink_metadata(&packs) {
            Ok(metadata) if metadata.is_dir() => Some(
                std::fs::read_dir(&packs)
                    .map_err(|_| Error::new("Cannot inspect promised Git object evidence"))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            _ => {
                return Err(Error::new(
                    "The local Git pack store is conflicting or unreadable",
                ));
            }
        };
        for entry in entries.into_iter().flatten() {
            let entry =
                entry.map_err(|_| Error::new("Git object dependency discovery is incomplete"))?;
            promised |= entry.file_name().as_encoded_bytes().ends_with(b".promisor");
        }
        if !alternate && !promised {
            return Ok(());
        }
        // A borrower ref cannot protect an alternate donor from its own GC.
        // Import a full, non-thin reachable closure before acknowledging the
        // pin. Promisor packs need the same local guarantee. The existing
        // subprocess size/deadline bounds apply; failure leaves ownership
        // unacknowledged rather than promising a root into another store.
        let pack = self.git.bytes(
            &["pack-objects", "--stdout", "--revs"],
            format!("{target}\n").as_bytes(),
            None,
        )?;
        self.root
            .observer
            .at(ResourceOperation::Write, false, &packs)?;
        self.git.bytes(
            &[
                "-c",
                "core.fsync=all",
                "-c",
                "core.fsyncMethod=fsync",
                "index-pack",
                "--stdin",
            ],
            &pack,
            None,
        )?;
        self.root
            .observer
            .at(ResourceOperation::Write, true, &packs)?;
        self.flush_import()
    }

    fn pin(&self, r: &mut Resource, kind: &str, target: &str) -> Result<()> {
        let reference = Self::pin_ref(&r.operation, kind);
        // A unique annotated tag is a private reachability root, not a public
        // branch. The durable owned bit records acknowledged acquisition; a
        // matching name/value without that acknowledgment is never reclaimed.
        let content = format!(
            "object {target}\ntype commit\ntag pbps-compose-{}-{kind}\ntagger pbps compose <compose@pbps.invalid> 0 +0000\n\nPrivate compose recovery root\n",
            r.operation
        );
        let value = super::git::text(self.git.bytes(
            &["hash-object", "-t", "tag", "--stdin"],
            content.as_bytes(),
            None,
        )?)?;
        r.pins.insert(
            kind.into(),
            Pin {
                target: target.into(),
                value: value.clone(),
                owned: false,
                retired: false,
            },
        );
        self.save(r)?;
        self.localize_pin_target(target)?;
        self.root.observer.at(
            ResourceOperation::Create,
            false,
            &self.repository.common.join(&reference),
        )?;
        self.git.bytes(
            &[
                "-c",
                "core.fsync=all",
                "-c",
                "core.fsyncMethod=fsync",
                "hash-object",
                "-t",
                "tag",
                "-w",
                "--stdin",
            ],
            content.as_bytes(),
            None,
        )?;
        self.flush_object(&value)?;
        refs::Prepared::create(&self.git, &reference, &value)?.commit()?;
        self.root.observer.at(
            ResourceOperation::Create,
            true,
            &self.repository.common.join(&reference),
        )?;
        self.flush_ref(&reference)?;
        r.pins.get_mut(kind).unwrap().owned = true;
        self.save(r)
    }

    fn verify_pins(&self, r: &Resource) -> Result<()> {
        for (kind, pin) in &r.pins {
            if pin.retired {
                continue;
            }
            if !pin.owned
                || refs::observe(&self.git, &Self::pin_ref(&r.operation, kind))
                    != RefEvidence::Direct(pin.value.clone())
            {
                return Err(Error::new(
                    "Private Git pin ownership is unresolved; preserve its resource record",
                ));
            }
            self.git.bytes(
                &["cat-file", "-e", &format!("{}^{{commit}}", pin.value)],
                &[],
                None,
            )?;
        }
        Ok(())
    }

    pub fn seal(&self, id: &str, tree: &str, binding: &[u8]) -> Result<()> {
        self.locked(|| {
            let mut r = self.load(id)?;
            if r.state != ResourceState::Capturing {
                return Err(Error::new("Invalid candidate resource sealing transition"));
            }
            self.verify_pins(&r)?;
            let snapshot = self.snapshots.child(id, false)?;
            if r.snapshot.as_ref() != Some(&snapshot.identity()?) {
                return Err(Error::new("Private snapshot acquisition is unresolved"));
            }
            let git = Git {
                root: snapshot.path.join("repository"),
                hooks: self.git.hooks.clone(),
                deadline: self.git.deadline,
            };
            git.bytes(
                &[
                    "-c",
                    "core.fsync=all",
                    "-c",
                    "core.fsyncMethod=fsync",
                    "update-ref",
                    "refs/pbps-compose/candidate",
                    tree,
                ],
                &[],
                None,
            )?;
            if binding.len() > 64 * 1024 * 1024 {
                return Err(Error::new("Candidate manifest exceeds its size limit"));
            }
            snapshot.save("manifest.json", binding)?;
            r.binding = Some(super::digest(binding));
            r.inventory = Some(Inventory::capture(&snapshot)?);
            r.state = ResourceState::Sealed;
            self.save(&r)
        })
    }

    pub fn confirm(&self, id: &str) -> Result<()> {
        self.locked(|| {
            let mut r = self.load(id)?;
            if r.state == ResourceState::Retained {
                return self.verify_pins(&r);
            }
            if !matches!(r.state, ResourceState::Sealed | ResourceState::Confirmed) {
                return Err(Error::new(
                    "This candidate was retired or has unresolved resources",
                ));
            }
            self.verify_pins(&r)?;
            r.state = ResourceState::Confirmed;
            self.save(&r)
        })
    }

    pub fn known_commit(&self, id: &str, commit: &str) -> Result<()> {
        self.locked(|| {
            let mut r = self.load(id)?;
            if r.state != ResourceState::Confirmed || r.pins.contains_key("commit") {
                return Err(Error::new("The exact commit root cannot be regenerated"));
            }
            self.flush_object(commit)?;
            self.pin(&mut r, "commit", commit)
        })
    }

    pub fn preparation_recorded(&self, id: &str) -> Result<bool> {
        // A missing receipt cannot erase the exact commit's independent witness,
        // including an acquisition whose ownership acknowledgment was lost.
        Ok(self.load(id)?.pins.contains_key("commit"))
    }

    pub fn preparation_ready(
        &self,
        id: &str,
        binding: &str,
        base: &str,
        commit: &str,
    ) -> Result<bool> {
        let resource = self.load(id)?;
        if !resource.pins.get("commit").is_some_and(|pin| pin.owned) {
            return Ok(false);
        }
        self.admit(id, binding, base, Some(commit))?;
        Ok(true)
    }

    pub fn admit(&self, id: &str, binding: &str, base: &str, commit: Option<&str>) -> Result<()> {
        self.locked(|| {
            let r = self.load(id)?;
            if !matches!(r.state, ResourceState::Confirmed | ResourceState::Retained) {
                return Err(Error::new("Compose resources do not authorize publication"));
            }
            self.verify_pins(&r)?;
            if r.binding.as_deref() != Some(binding)
                || !r.pins.get("base").is_some_and(|pin| pin.target == base)
            {
                return Err(Error::new(
                    "Candidate resource evidence does not match the reviewed binding",
                ));
            }
            if !r.snapshot_retired {
                let snapshot = self.snapshots.child(id, false)?;
                if r.snapshot.as_ref() != Some(&snapshot.identity()?) {
                    return Err(Error::new("Candidate snapshot identity changed"));
                }
                let bytes = snapshot
                    .read("manifest.json", 64 * 1024 * 1024)?
                    .ok_or_else(|| Error::new("Candidate manifest is unavailable"))?;
                if super::digest(&bytes) != binding {
                    return Err(Error::new("Candidate manifest changed after review"));
                }
            }
            if let Some(commit) = commit
                && !r
                    .pins
                    .get("commit")
                    .is_some_and(|pin| pin.target == commit && pin.owned && !pin.retired)
            {
                return Err(Error::new("The exact commit has no owned durable root"));
            }
            Ok(())
        })
    }

    pub fn retire(&self, id: &str, keep_commit: bool) -> Result<()> {
        self.locked(|| self.retire_locked(id, keep_commit))
    }

    fn retire_locked(&self, id: &str, keep_commit: bool) -> Result<()> {
        let mut r = self.load(id)?;
        if r.state == ResourceState::Spent || (keep_commit && r.state == ResourceState::Retained) {
            return Ok(());
        }
        if r.state != ResourceState::Retiring {
            if !matches!(
                r.state,
                ResourceState::Sealed | ResourceState::Confirmed | ResourceState::Retained
            ) {
                return Err(Error::new(
                    "An interrupted unsealed candidate requires manual recovery; preserve its resources",
                ));
            }
            self.verify_pins(&r)?;
            r.state = ResourceState::Retiring;
            r.keep_commit = keep_commit;
            self.save(&r)?;
        } else if r.keep_commit != keep_commit {
            return Err(Error::new(
                "Finish the existing resource retirement before changing its intent",
            ));
        }
        if !r.snapshot_retired {
            let inventory = r.inventory.as_ref().ok_or_else(|| {
                Error::new("Private snapshot has no acknowledged inventory; preserve it")
            })?;
            inventory.retire(&self.snapshots, id)?;
            r.snapshot_retired = true;
            r.snapshot = None;
            r.inventory = None;
            self.save(&r)?;
        }
        for kind in ["base", "commit"] {
            let Some(pin) = r.pins.get(kind) else {
                continue;
            };
            if pin.retired || (kind == "commit" && keep_commit) {
                continue;
            }
            if !pin.owned {
                return Err(Error::new(
                    "Cannot retire an unacknowledged private Git pin",
                ));
            }
            let reference = Self::pin_ref(id, kind);
            let path = self.repository.common.join(&reference);
            self.root
                .observer
                .at(ResourceOperation::Remove, false, &path)?;
            match refs::observe(&self.git, &reference) {
                RefEvidence::Absent => (),
                RefEvidence::Direct(value) if value == pin.value => {
                    refs::Prepared::delete_owned(&self.git, &reference, &value)?
                }
                RefEvidence::Direct(_) | RefEvidence::Symbolic | RefEvidence::Unreadable => {
                    return Err(Error::new(
                        "Private pin changed or is unreadable; preserve it",
                    ));
                }
            }
            self.root
                .observer
                .at(ResourceOperation::Remove, true, &path)?;
            // Git may remove now-empty ref directories. Flush the nearest
            // surviving parent and its ancestors before acknowledging retirement.
            let mut parent = path.parent();
            while let Some(path) = parent {
                match std::fs::symlink_metadata(path) {
                    Ok(_) => self.sync_path(path)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(_) => return Err(Error::new("Cannot inspect private pin retirement")),
                }
                if path == self.repository.common {
                    break;
                }
                parent = path.parent();
            }
            r.pins.get_mut(kind).unwrap().retired = true;
            self.save(&r)?;
        }
        r.state = if keep_commit {
            ResourceState::Retained
        } else {
            ResourceState::Spent
        };
        if !keep_commit {
            // A compact tombstone revokes old candidate handles even if the
            // user later deletes the public output branch and receipt.
            r.snapshot = None;
            r.inventory = None;
            r.pins.clear();
            r.binding = None;
        }
        self.save(&r)
    }

    pub fn discard_preview(&self, id: &str) -> Result<()> {
        self.locked(|| {
            if self.load(id)?.state != ResourceState::Sealed {
                return Err(Error::new(
                    "Only a sealed unconfirmed preview may be discarded",
                ));
            }
            self.retire_locked(id, false)
        })
    }

    pub fn expire(&self, id: &str, now: SystemTime) -> Result<()> {
        self.locked(|| {
            let r = self.load(id)?;
            let now = now
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::new("Preview clock is unavailable"))?
                .as_secs();
            if r.state == ResourceState::Sealed
                && r.created
                    .checked_add(24 * 60 * 60)
                    .is_some_and(|expiry| now >= expiry)
            {
                self.retire_locked(id, false)?;
            }
            Ok(())
        })
    }

    pub fn resume_retirement(&self, id: &str) -> Result<ResourceReport> {
        let r = self.load(id)?;
        if r.state == ResourceState::Retiring {
            self.retire(id, r.keep_commit)?;
        }
        self.report(id)
    }

    pub fn report(&self, id: &str) -> Result<ResourceReport> {
        self.report_resource(self.load(id)?)
    }

    fn report_resource(&self, r: Resource) -> Result<ResourceReport> {
        let id = &r.operation;
        if r.state == ResourceState::Retained {
            self.verify_pins(&r)?;
        }
        Ok(ResourceReport {
            operation_id: id.clone(),
            state: r.state,
            cleanup_pending: !matches!(r.state, ResourceState::Retained | ResourceState::Spent),
            location: self.records.path.join(format!("{id}.json")),
            instruction: if r.state == ResourceState::Capturing {
                "Preserve this unsealed operation; diagnose surviving children and unacknowledged resources before manual recovery."
            } else {
                "Preserve conflicting or unreadable resources; retry only the recorded retirement. Git output branches are never cleanup targets."
            },
        })
    }

    pub fn list(&self) -> Result<Vec<ResourceReport>> {
        let names = self.records.names()?;
        for snapshot in self.snapshots.names()? {
            if !identity(&snapshot) || !names.contains(&format!("{snapshot}.json")) {
                return Err(Error::new(&format!(
                    "Unattributable snapshot at {}; preserve it for manual recovery",
                    self.snapshots.path.join(snapshot).display()
                )));
            }
        }
        let mut result = Vec::new();
        for name in names {
            let id = name
                .strip_suffix(".json")
                .filter(|id| identity(id))
                .ok_or_else(|| {
                    Error::new("Unknown resource acquisition evidence requires manual recovery")
                })?;
            let record = self.read(id)?;
            if record.repository.source_scope(&self.repository)? {
                result.push(self.report_resource(record)?);
            }
        }
        Ok(result)
    }
}

//! The release step for the three persisted and published formats (#2412,
//! DEC-2412.3, docs/RELEASE-FORMATS.md).
//!
//! One module, two modes. `check` runs in every `cargo test`: the record of
//! what the last release froze is well formed, its archive exists and says the
//! same thing, and this build has not changed a frozen format without moving its
//! number. `freeze` is run by hand for a release and writes the record and the
//! set's archive. It is a test and not a command or a script because the three
//! fingerprints come out of this binary's own types: a script would have to
//! start the binary to learn them, and a new command would widen the command
//! surface for something a release does once.

use std::path::{Path, PathBuf};

use pbps_model::format::{self, Frozen, Released};

use super::{PUBLISHED, SCHEMA_VERSION, rendered, schema_set_fingerprint, set_fingerprint};

/// The variable that asks for a freeze. Its value is the release tag.
///
/// A variable and not a flag or an `#[ignore]`d test: `cargo test` offers no
/// way to pass a flag to one test, and an ignored test needs an inventory entry
/// that a release would have to keep in step. An unset variable makes the test
/// return before it touches anything, and the tag has to be typed, so a freeze
/// is neither accidental nor anonymous.
const FREEZE_VARIABLE: &str = "PBPS_FREEZE_FORMATS";

/// The two places a release writes. Paths and not constants so that a test can
/// aim the same code at a scratch directory.
struct Layout {
    /// `released_formats.json`, read by `pbps_model::format::released`.
    record: PathBuf,
    /// `published-schemas`, under which `released/<n>/` holds a set's archive.
    archives: PathBuf,
}

impl Layout {
    fn checked_in() -> Self {
        let cli = Path::new(env!("CARGO_MANIFEST_DIR"));
        Layout {
            record: cli.join("../pbps-model/src/released_formats.json"),
            archives: cli.join("tests/fixtures/published-schemas"),
        }
    }

    /// Under `released/`, apart from the numbered directories of the per-change
    /// archives: those keep their numbers as history, and after the reset to 1
    /// they would collide with the released numbers.
    fn archive(&self, number: u32) -> PathBuf {
        self.archives.join("released").join(number.to_string())
    }
}

/// What this build says about each format, in the shape the record freezes.
struct Build {
    plan: Frozen,
    state: Frozen,
    schema_set: Frozen,
}

impl Build {
    fn current() -> Self {
        let frozen = |version: u32, fingerprint: &str| Frozen {
            version,
            fingerprint: fingerprint.to_owned(),
        };
        Build {
            plan: frozen(format::PLAN.current, format::PLAN.identity()),
            state: frozen(format::STATE.current, format::STATE.identity()),
            schema_set: frozen(SCHEMA_VERSION, &schema_set_fingerprint()),
        }
    }
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The set's documents as the archive holds them, or why they cannot be read.
fn archived_documents(dir: &Path) -> Result<Vec<(&'static str, serde_json::Value)>, String> {
    let mut documents = Vec::new();
    for (_, file) in PUBLISHED {
        let path = dir.join(file);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("the archive lacks {}: {e}", path.display()))?;
        let document = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
        documents.push((file, document));
    }
    Ok(documents)
}

/// Every condition of the `check` mode, a function of what is compared.
///
/// `None` is a record that says nothing was released: well formed, and nothing
/// to hold the build to. The shape of the file is already the loader's
/// (`deny_unknown_fields`), so what is checked here is what the shape cannot
/// say: a tag, numbers, digests and the archive.
fn check_record(record: Option<&Released>, layout: &Layout, build: &Build) -> Result<(), String> {
    let Some(record) = record else {
        return Ok(());
    };
    if record.tag.trim().is_empty() {
        return Err("the release has no tag".to_owned());
    }
    let formats = [
        ("plan", &record.plan, &build.plan),
        ("state", &record.state, &build.state),
        ("schema set", &record.schema_set, &build.schema_set),
    ];
    for (name, frozen, _) in &formats {
        if frozen.version == 0 || !is_digest(&frozen.fingerprint) {
            return Err(format!(
                "the released {name} is not a number from 1 and a SHA-256 digest: {frozen:?}"
            ));
        }
    }

    // The archive is the set at its release, so it has the released number and
    // hashes to the released fingerprint. Both are asked: an archive copied from
    // the wrong set can carry the right stamp, and a stamp can be edited by hand.
    let dir = layout.archive(record.schema_set.version);
    let documents = archived_documents(&dir)?;
    for (file, document) in &documents {
        if document["x-pbps-schema-version"] != record.schema_set.version {
            return Err(format!(
                "{} is stamped {} and not the released set's number {}",
                dir.join(file).display(),
                document["x-pbps-schema-version"],
                record.schema_set.version
            ));
        }
    }
    let archived = set_fingerprint(documents);
    if archived != record.schema_set.fingerprint {
        return Err(format!(
            "the archive {} hashes to {archived}, and the record froze {}",
            dir.display(),
            record.schema_set.fingerprint
        ));
    }

    // Where this build has not moved the number, it must not have changed the
    // format: the release froze what that number means.
    for (name, frozen, current) in &formats {
        if frozen.version == current.version && frozen.fingerprint != current.fingerprint {
            return Err(format!(
                "the {name} format is still number {}, which the release froze with another \
                 fingerprint; a change to it moves the number to {}",
                current.version,
                frozen.version + 1
            ));
        }
    }
    Ok(())
}

/// The record a freeze replaces. It must exist and parse: in the repository the
/// file always does, so an absent or unreadable one is a wrong path or a damaged
/// checkout, and reading it as "nothing was released" would skip the one check
/// that a freeze can still make.
fn read_record(path: &Path) -> Result<Option<Released>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the record {}: {e}", path.display()))?;
    format::parse_record(&text)
        .map_err(|e| format!("the record {} is not a release record: {e}", path.display()))
}

/// Writes `documents` into a directory this call makes, or leaves no directory.
///
/// The leaf is made with `create_dir`, so it is never one that was there: a
/// half-written archive would otherwise be found by the next freeze, which
/// cannot read it as a set and stops. The documents are a parameter so that a
/// test can hand over one whose write fails.
fn write_archive(dir: &Path, documents: &[(&str, String)]) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::create_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (file, text) in documents {
        let path = dir.join(file);
        if let Err(e) = std::fs::write(&path, text) {
            return Err(match std::fs::remove_dir_all(dir) {
                Ok(()) => format!("cannot write {}: {e}", path.display()),
                Err(removed) => format!(
                    "cannot write {}: {e}; the directory {} remains, and could not be \
                     removed: {removed}",
                    path.display(),
                    dir.display()
                ),
            });
        }
    }
    Ok(())
}

/// Replaces `path` with `text` in one step, or leaves `path` as it was.
///
/// A plain `write` truncates first, so a failure part way (a full disk) would
/// leave the record it replaces half written. The text goes to a sibling in the
/// same directory, which `rename` then puts over the record; `rename` replaces
/// an existing file on Windows as well. If either step fails the sibling is
/// removed and the error names the record. `rename` is a parameter so that a
/// test can make it fail.
fn replace_record_with(
    path: &Path,
    text: &str,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let mut name = path.file_name().expect("the record is a file").to_owned();
    name.push(".tmp");
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, text)
        .and_then(|()| rename(&temporary, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temporary);
            format!("cannot replace the record {}: {e}", path.display())
        })
}

/// Writes the record and the set's archive, or says why it will not.
///
/// Everything is checked, then the record is written once. Before anything is
/// written it reads the record it replaces and holds each format of this build
/// to it with `format::check_increment`, the whole rule: a format changed under
/// its released number, or a number moved with nothing changed, is refused.
/// Without that the freeze would check the build against the record it has just
/// written, which always agrees, and a missed increment could not be found
/// again. A record that is absent, unreadable or of no legal shape is refused
/// and not read as "no release". A blank tag is refused.
///
/// An archive is never rewritten (the rule of the numbered ones, kept). One that
/// exists is reused when it is this set, which is what a second release finds if
/// the documents did not change between two releases, and refused when it is not.
/// A new one is written into a directory the call makes, and a failed write
/// removes it. With the archive there, the record's text is built and put
/// through the loader's own parser and `check_record`, stamps included, before
/// it is written, and it replaces the old record in one step
/// (`replace_record_with`). A failed freeze therefore leaves the record's bytes
/// as they were, and the archive directory absent or complete; a complete one
/// is not removed, and a second freeze reuses it.
fn freeze(layout: &Layout, tag: &str, build: &Build) -> Result<(), String> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err(format!("{FREEZE_VARIABLE} must hold the release tag"));
    }
    let previous = read_record(&layout.record)?;
    for (name, now, frozen) in [
        ("plan", &build.plan, previous.as_ref().map(|r| &r.plan)),
        ("state", &build.state, previous.as_ref().map(|r| &r.state)),
        (
            "schema set",
            &build.schema_set,
            previous.as_ref().map(|r| &r.schema_set),
        ),
    ] {
        format::check_increment(name, now.version, &now.fingerprint, frozen)?;
    }
    let dir = layout.archive(build.schema_set.version);
    if dir.exists() {
        let archived = set_fingerprint(archived_documents(&dir)?);
        if archived != build.schema_set.fingerprint {
            return Err(format!(
                "{} exists and is another set; an archive is never rewritten, so a release \
                 whose set changed needs the set's number moved first",
                dir.display()
            ));
        }
    } else {
        let documents = PUBLISHED.map(|(kind, file)| (file, rendered(kind)));
        write_archive(&dir, &documents)?;
    }
    let released = Released {
        tag: tag.to_owned(),
        plan: build.plan.clone(),
        state: build.state.clone(),
        schema_set: build.schema_set.clone(),
    };
    let text = serde_json::to_string_pretty(&serde_json::json!({ "released": released }))
        .map_err(|e| e.to_string())?
        + "\n";
    let parsed = format::parse_record(&text).map_err(|e| e.to_string())?;
    check_record(parsed.as_ref(), layout, build)?;
    replace_record_with(&layout.record, &text, |from, to| std::fs::rename(from, to))
}

/// The test a release runs. Without the variable it returns at once.
fn freeze_if_asked(request: Option<std::ffi::OsString>, layout: &Layout, build: &Build) {
    let Some(tag) = request else {
        return;
    };
    let tag = tag.to_string_lossy();
    if let Err(why) = freeze(layout, &tag, build) {
        panic!("the formats were not frozen: {why}");
    }
}

#[test]
fn the_released_record_matches_its_archive_and_this_build() {
    if let Err(why) = check_record(format::released(), &Layout::checked_in(), &Build::current()) {
        panic!("{why}");
    }
}

/// The release step. Run by hand, see docs/RELEASE-FORMATS.md:
/// `PBPS_FREEZE_FORMATS=<tag> cargo test -p pbps-cli --bin pbps freeze_the_formats_for_a_release`.
/// Without the variable it does nothing, which is what every `cargo test` runs.
#[test]
fn freeze_the_formats_for_a_release() {
    freeze_if_asked(
        std::env::var_os(FREEZE_VARIABLE),
        &Layout::checked_in(),
        &Build::current(),
    );
}

/// A directory of this test's own, removed when it goes out of scope.
struct Scratch(PathBuf);

/// The record as the repository holds it before the first release.
const NO_RELEASE: &str = "{\"released\": null}\n";

impl Scratch {
    /// A directory laid out like the repository: the record exists and says
    /// nothing was released, and there is no archive directory yet.
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-freeze-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = Scratch(dir);
        std::fs::write(scratch.layout().record, NO_RELEASE).unwrap();
        scratch
    }

    fn layout(&self) -> Layout {
        Layout {
            record: self.0.join("released_formats.json"),
            archives: self.0.join("published-schemas"),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The release a scratch directory's record names.
fn recorded(scratch: &Scratch) -> Released {
    let text = std::fs::read_to_string(scratch.layout().record).unwrap();
    format::parse_record(&text).unwrap().expect("a release")
}

/// Every path under `dir`, relative and in order: what a refused freeze must
/// leave as it found it. A missing `dir` lists nothing.
fn listing(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            walk(root, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// A freeze into a scratch directory, and the record it wrote.
fn frozen_in(scratch: &Scratch, build: &Build) -> Released {
    freeze(&scratch.layout(), "v1.0.0", build).expect("a freeze into an empty directory");
    let text = std::fs::read_to_string(&scratch.layout().record).unwrap();
    format::parse_record(&text).unwrap().expect("a release")
}

#[test]
fn a_freeze_writes_the_numbers_the_fingerprints_and_the_set_archive() {
    let scratch = Scratch::new();
    let build = Build::current();
    let released = frozen_in(&scratch, &build);
    assert_eq!(released.tag, "v1.0.0");
    assert_eq!(released.plan, build.plan);
    assert_eq!(released.state, build.state);
    assert_eq!(released.schema_set, build.schema_set);
    let dir = scratch.layout().archive(SCHEMA_VERSION);
    for (kind, file) in PUBLISHED {
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).unwrap(),
            rendered(kind)
        );
    }
    // The record is what the check accepts, and the file shape is the loader's.
    assert_eq!(
        check_record(Some(&released), &scratch.layout(), &build),
        Ok(())
    );
}

#[test]
fn nothing_is_written_unless_a_freeze_is_asked_for() {
    let scratch = Scratch::new();
    freeze_if_asked(None, &scratch.layout(), &Build::current());
    assert_eq!(
        std::fs::read_to_string(scratch.layout().record).unwrap(),
        NO_RELEASE
    );
    assert!(!scratch.layout().archives.exists());
}

#[test]
fn an_asked_freeze_writes_and_a_blank_tag_writes_nothing() {
    let scratch = Scratch::new();
    freeze_if_asked(Some("v2".into()), &scratch.layout(), &Build::current());
    assert_eq!(recorded(&scratch).tag, "v2");
    // Negative: an empty and a blank tag are refused before any file is made.
    let other = Scratch::new();
    for tag in ["", "  \t"] {
        let why = freeze(&other.layout(), tag, &Build::current()).expect_err("no tag");
        assert!(why.contains(FREEZE_VARIABLE), "{why}");
    }
    assert_eq!(
        std::fs::read_to_string(other.layout().record).unwrap(),
        NO_RELEASE
    );
    assert!(!other.layout().archives.exists());
}

#[test]
fn a_second_release_of_the_same_set_reuses_its_archive() {
    let scratch = Scratch::new();
    let build = Build::current();
    frozen_in(&scratch, &build);
    let archive = scratch.layout().archive(SCHEMA_VERSION);
    let before: Vec<_> = PUBLISHED
        .map(|(_, file)| std::fs::read(archive.join(file)).unwrap())
        .into();
    freeze(&scratch.layout(), "v1.0.1", &build).expect("the same set");
    let text = std::fs::read_to_string(&scratch.layout().record).unwrap();
    assert_eq!(format::parse_record(&text).unwrap().unwrap().tag, "v1.0.1");
    let after: Vec<_> = PUBLISHED
        .map(|(_, file)| std::fs::read(archive.join(file)).unwrap())
        .into();
    assert_eq!(before, after, "an archive is not rewritten");
}

#[test]
fn a_second_freeze_does_not_rewrite_an_archive_of_another_set_or_the_record() {
    let scratch = Scratch::new();
    let build = Build::current();
    frozen_in(&scratch, &build);
    // The archive at this number is another set: the documents changed and the
    // number did not move.
    let file = scratch
        .layout()
        .archive(SCHEMA_VERSION)
        .join(PUBLISHED[0].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    document["description"] = "another set".into();
    let archived = document.to_string();
    std::fs::write(&file, &archived).unwrap();
    let before = std::fs::read(&scratch.layout().record).unwrap();
    let why = freeze(&scratch.layout(), "v1.0.1", &build).expect_err("another set");
    assert!(why.contains("never rewritten"), "{why}");
    assert_eq!(std::fs::read(&scratch.layout().record).unwrap(), before);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), archived);
}

/// A format's name and one way to change a build's record of it.
type Edit = (&'static str, fn(&mut Build));

/// The three formats of a build, each with a change of its fingerprint under
/// the same number.
fn edits() -> [Edit; 3] {
    [
        ("plan", |b| b.plan.fingerprint = "f".repeat(64)),
        ("state", |b| b.state.fingerprint = "f".repeat(64)),
        ("schema set", |b| b.schema_set.fingerprint = "f".repeat(64)),
    ]
}

#[test]
fn a_freeze_refuses_a_format_changed_under_its_released_number() {
    let scratch = Scratch::new();
    let build = Build::current();
    frozen_in(&scratch, &build);
    let record = std::fs::read(scratch.layout().record).unwrap();
    let tree = listing(&scratch.layout().archives);
    for (name, edit) in edits() {
        let mut changed = Build::current();
        edit(&mut changed);
        let why = freeze(&scratch.layout(), "v1.0.1", &changed).expect_err(name);
        assert!(
            why.contains(&format!("the {name} format has changed")),
            "{why}"
        );
        assert_eq!(std::fs::read(scratch.layout().record).unwrap(), record);
        assert_eq!(listing(&scratch.layout().archives), tree, "{name}");
    }
}

#[test]
fn a_freeze_refuses_a_number_moved_with_nothing_changed() {
    let scratch = Scratch::new();
    frozen_in(&scratch, &Build::current());
    let record = std::fs::read(scratch.layout().record).unwrap();
    let tree = listing(&scratch.layout().archives);
    let bumps: [Edit; 3] = [
        ("plan", |b| b.plan.version += 1),
        ("state", |b| b.state.version += 1),
        ("schema set", |b| b.schema_set.version += 1),
    ];
    for (name, bump) in bumps {
        let mut moved = Build::current();
        bump(&mut moved);
        let why = freeze(&scratch.layout(), "v1.0.1", &moved).expect_err(name);
        assert!(
            why.contains(&format!("the {name} format has not changed")),
            "{why}"
        );
        assert_eq!(std::fs::read(scratch.layout().record).unwrap(), record);
        assert_eq!(listing(&scratch.layout().archives), tree, "{name}");
    }
}

#[test]
fn a_freeze_accepts_a_changed_format_whose_number_moved_by_one() {
    let scratch = Scratch::new();
    frozen_in(&scratch, &Build::current());
    let mut next = Build::current();
    next.plan.fingerprint = "f".repeat(64);
    next.plan.version += 1;
    next.state.fingerprint = "e".repeat(64);
    next.state.version += 1;
    freeze(&scratch.layout(), "v1.0.1", &next).expect("each change carries its increment");
    let released = recorded(&scratch);
    assert_eq!(released.tag, "v1.0.1");
    assert_eq!(released.plan, next.plan);
    assert_eq!(released.state, next.state);
}

#[test]
fn a_freeze_refuses_a_missing_or_unreadable_record_naming_its_path() {
    // Absent and unparseable are two failures, and neither reads as "no release".
    let missing = Scratch::new();
    std::fs::remove_file(missing.layout().record).unwrap();
    let why = freeze(&missing.layout(), "v1", &Build::current()).expect_err("no record");
    assert!(
        why.replace('\\', "/").contains("released_formats.json"),
        "{why}"
    );
    assert!(why.contains("cannot read"), "{why}");
    assert!(!missing.layout().archives.exists());

    let broken = Scratch::new();
    std::fs::write(broken.layout().record, "{ not json").unwrap();
    let why = freeze(&broken.layout(), "v1", &Build::current()).expect_err("bad record");
    assert!(
        why.replace('\\', "/").contains("released_formats.json"),
        "{why}"
    );
    assert!(why.contains("not a release record"), "{why}");
    assert_eq!(
        std::fs::read_to_string(broken.layout().record).unwrap(),
        "{ not json"
    );
    assert!(!broken.layout().archives.exists());
}

#[test]
fn a_record_without_the_released_key_is_damage_and_not_no_release() {
    // `{}` parses as JSON and would read as "nothing released" if the key were
    // optional, which turns every increment check off. The freeze and the
    // reader both refuse it, naming the path, and the freeze writes nothing.
    for text in ["{}\n", "{\"note\": 1}\n"] {
        let scratch = Scratch::new();
        std::fs::write(scratch.layout().record, text).unwrap();
        let why = freeze(&scratch.layout(), "v1", &Build::current()).expect_err(text);
        assert!(
            why.replace('\\', "/").contains("released_formats.json"),
            "{why}"
        );
        assert!(why.contains("not a release record"), "{why}");
        let read = read_record(&scratch.layout().record).expect_err(text);
        assert!(
            read.replace('\\', "/").contains("released_formats.json"),
            "{read}"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.layout().record).unwrap(),
            text
        );
        assert!(!scratch.layout().archives.exists());
    }
    // Positive: the explicit shape is read as no release, and the freeze goes on.
    let scratch = Scratch::new();
    assert_eq!(read_record(&scratch.layout().record), Ok(None));
    freeze(&scratch.layout(), "v1", &Build::current()).expect("the repository's own shape");
}

#[test]
fn a_freeze_refuses_an_archive_of_another_set_when_nothing_was_released() {
    // The record says nothing was released, so the record's refusals do not
    // apply, and the archive's is the one left to stop a freeze.
    let scratch = Scratch::new();
    let dir = scratch.layout().archive(SCHEMA_VERSION);
    let documents = PUBLISHED.map(|(kind, file)| (file, rendered(kind)));
    write_archive(&dir, &documents).unwrap();
    let file = dir.join(PUBLISHED[0].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    document["description"] = "another set".into();
    std::fs::write(&file, document.to_string()).unwrap();
    let why = freeze(&scratch.layout(), "v1", &Build::current()).expect_err("another set");
    assert!(why.contains("another set"), "{why}");
    assert_eq!(
        std::fs::read_to_string(scratch.layout().record).unwrap(),
        NO_RELEASE
    );
}

/// Rewrites the stamp of the first document of the archive at `dir`: a change
/// the set's fingerprint does not see, because it leaves the stamps out.
fn restamp(dir: &Path, number: u32) {
    let file = dir.join(PUBLISHED[0].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    document["x-pbps-schema-version"] = number.into();
    std::fs::write(&file, document.to_string()).unwrap();
}

#[test]
fn a_reused_archive_with_a_wrong_stamp_is_refused_before_the_record_is_written() {
    let documents = PUBLISHED.map(|(kind, file)| (file, rendered(kind)));
    let wrong = SCHEMA_VERSION + 1;
    // Nothing was released: the record is `NO_RELEASE` and an archive is there.
    let none = Scratch::new();
    let dir = none.layout().archive(SCHEMA_VERSION);
    write_archive(&dir, &documents).unwrap();
    restamp(&dir, wrong);
    let tree = listing(&none.layout().archives);
    let why = freeze(&none.layout(), "v1", &Build::current()).expect_err("a wrong stamp");
    assert!(why.contains("is stamped"), "{why}");
    assert_eq!(
        std::fs::read_to_string(none.layout().record).unwrap(),
        NO_RELEASE
    );
    assert_eq!(listing(&none.layout().archives), tree);
    // After a release: the same archive, restamped, with a record that names it.
    let released = Scratch::new();
    frozen_in(&released, &Build::current());
    restamp(&released.layout().archive(SCHEMA_VERSION), wrong);
    let record = std::fs::read(released.layout().record).unwrap();
    let tree = listing(&released.layout().archives);
    let why = freeze(&released.layout(), "v1.0.1", &Build::current()).expect_err("a wrong stamp");
    assert!(why.contains("is stamped"), "{why}");
    assert_eq!(std::fs::read(released.layout().record).unwrap(), record);
    assert_eq!(listing(&released.layout().archives), tree);
}

#[test]
fn a_failed_record_write_leaves_the_previous_record_whole_and_no_temporary_file() {
    let scratch = Scratch::new();
    let record = scratch.layout().record;
    let before = std::fs::read(&record).unwrap();
    let files = listing(&scratch.0);
    // The rename fails after the temporary file was written: it is removed.
    let why = replace_record_with(&record, "{\"released\": null, \"x\": 1}", |_, _| {
        Err(std::io::Error::other("no space"))
    })
    .expect_err("the rename fails");
    assert!(
        why.replace('\\', "/").contains("released_formats.json"),
        "{why}"
    );
    assert!(why.contains("no space"), "{why}");
    assert_eq!(std::fs::read(&record).unwrap(), before);
    assert_eq!(listing(&scratch.0), files);
    // The write fails: a directory holds the temporary file's name. The record
    // keeps its bytes, and the directory is not ours to remove.
    let blocker = scratch.0.join("released_formats.json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let why = replace_record_with(&record, "new", |from, to| std::fs::rename(from, to))
        .expect_err("the write fails");
    assert!(
        why.replace('\\', "/").contains("released_formats.json"),
        "{why}"
    );
    assert_eq!(std::fs::read(&record).unwrap(), before);
    assert!(blocker.is_dir());
}

#[test]
fn a_successful_record_write_replaces_the_record_and_leaves_no_temporary_file() {
    let scratch = Scratch::new();
    let record = scratch.layout().record;
    replace_record_with(&record, "replaced", |from, to| std::fs::rename(from, to))
        .expect("an ordinary replace");
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "replaced");
    assert_eq!(listing(&scratch.0), ["released_formats.json"]);
    // The whole freeze leaves the same: the record, and the archive tree.
    let frozen = Scratch::new();
    frozen_in(&frozen, &Build::current());
    let names = listing(&frozen.0);
    assert!(
        names.iter().all(|name| !name.ends_with(".tmp")),
        "{names:?}"
    );
    assert!(
        names.contains(&"released_formats.json".to_owned()),
        "{names:?}"
    );
}

#[test]
fn a_failed_archive_write_leaves_no_directory_and_a_later_freeze_succeeds() {
    let scratch = Scratch::new();
    let dir = scratch.layout().archive(SCHEMA_VERSION);
    // The second file's directory does not exist, so its write fails after the
    // first has succeeded: a partial archive, which must not stay.
    let documents = [
        ("first.json", "{}".to_owned()),
        ("missing/second.json", "{}".to_owned()),
    ];
    let why = write_archive(&dir, &documents).expect_err("the second write fails");
    assert!(why.contains("cannot write"), "{why}");
    assert!(
        why.replace('\\', "/").contains("missing/second.json"),
        "{why}"
    );
    assert!(!dir.exists(), "a partial archive stays: {why}");
    freeze(&scratch.layout(), "v1", &Build::current()).expect("nothing is in the way");
    assert!(dir.join(PUBLISHED[0].1).exists());
}

#[test]
fn an_archive_write_never_touches_a_directory_that_was_already_there() {
    // The failure cleanup removes only what the call made. A directory that
    // exists is refused, with its contents as they were.
    let scratch = Scratch::new();
    let dir = scratch.layout().archive(SCHEMA_VERSION);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.json"), "kept").unwrap();
    let documents = [("a.json", "{}".to_owned())];
    let why = write_archive(&dir, &documents).expect_err("it exists");
    assert!(why.contains("cannot create"), "{why}");
    assert_eq!(listing(&dir), ["keep.json"]);
}

#[test]
fn a_record_that_says_nothing_was_released_asks_nothing_of_the_build() {
    let scratch = Scratch::new();
    assert_eq!(
        check_record(None, &scratch.layout(), &Build::current()),
        Ok(())
    );
    // The checked-in record is parsed by the loader's parser and, today, is this.
    assert!(
        format::parse_record(include_str!("../../pbps-model/src/released_formats.json")).is_ok()
    );
    // Negative: the loader's parser refuses an unnamed format and an unknown key.
    assert!(format::parse_record(r#"{"released": {"tag": "v1"}}"#).is_err());
    assert!(format::parse_record(r#"{"released": null, "note": 1}"#).is_err());
}

/// A release of this build's own formats, in a scratch directory, to be broken
/// one way at a time by the tests below.
fn released_here(scratch: &Scratch) -> (Released, Build) {
    let build = Build::current();
    (frozen_in(scratch, &build), build)
}

#[test]
fn a_record_without_a_tag_a_number_or_a_digest_is_refused() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let refused = |edit: &dyn Fn(&mut Released)| {
        let mut broken = released.clone();
        edit(&mut broken);
        check_record(Some(&broken), &scratch.layout(), &build).expect_err("a malformed record")
    };
    assert!(refused(&|r| r.tag = " ".into()).contains("no tag"));
    assert!(refused(&|r| r.plan.version = 0).contains("plan"));
    assert!(refused(&|r| r.state.fingerprint = "abc".into()).contains("state"));
    assert!(refused(&|r| r.schema_set.fingerprint = "F".repeat(64)).contains("schema set"));
}

#[test]
fn a_released_set_with_no_archive_is_refused() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let mut next = released.clone();
    next.schema_set.version += 1;
    let why = check_record(Some(&next), &scratch.layout(), &build).expect_err("no archive");
    assert!(why.contains("the archive lacks"), "{why}");
    // Negative: an archive that holds only some of the documents is no archive.
    let partial = Scratch::new();
    let layout = partial.layout();
    let dir = layout.archive(released.schema_set.version);
    std::fs::create_dir_all(&dir).unwrap();
    let (kind, file) = PUBLISHED[0];
    std::fs::write(dir.join(file), rendered(kind)).unwrap();
    let why = check_record(Some(&released), &layout, &build).expect_err("one document");
    assert!(why.contains("the archive lacks"), "{why}");
}

#[test]
fn an_archive_that_differs_from_the_frozen_fingerprint_is_refused() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let file = scratch
        .layout()
        .archive(SCHEMA_VERSION)
        .join(PUBLISHED[1].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    // A reworded description is content: the fingerprint counts it.
    document["description"] = "reworded after the release".into();
    std::fs::write(&file, document.to_string()).unwrap();
    let why = check_record(Some(&released), &scratch.layout(), &build).expect_err("edited");
    assert!(why.contains("hashes to"), "{why}");
}

#[test]
fn an_archive_stamped_with_another_number_is_refused() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let file = scratch
        .layout()
        .archive(SCHEMA_VERSION)
        .join(PUBLISHED[2].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    document["x-pbps-schema-version"] = (SCHEMA_VERSION + 1).into();
    std::fs::write(&file, document.to_string()).unwrap();
    let why = check_record(Some(&released), &scratch.layout(), &build).expect_err("restamped");
    assert!(why.contains("is stamped"), "{why}");
}

/// The stamps are not content: the tool version differs between the build that
/// archived the set and the one that checks it, and must not fail the check.
#[test]
fn an_archive_with_another_tool_version_stamp_is_accepted() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let file = scratch
        .layout()
        .archive(SCHEMA_VERSION)
        .join(PUBLISHED[0].1);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    document["x-pbps-tool-version"] = "99.0.0".into();
    std::fs::write(&file, document.to_string()).unwrap();
    assert_eq!(
        check_record(Some(&released), &scratch.layout(), &build),
        Ok(())
    );
}

/// At the frozen number a changed format is refused, for each format, and at the
/// next number it is not: the number is what lets a format change.
#[test]
fn a_format_changed_under_its_frozen_number_is_refused() {
    let scratch = Scratch::new();
    let (released, build) = released_here(&scratch);
    let changed = |edit: &dyn Fn(&mut Build)| {
        let mut moved = Build::current();
        edit(&mut moved);
        check_record(Some(&released), &scratch.layout(), &moved)
    };
    let another = "f".repeat(64);
    for (name, edit) in [
        (
            "plan",
            Box::new(|b: &mut Build| b.plan.fingerprint = "f".repeat(64))
                as Box<dyn Fn(&mut Build)>,
        ),
        ("state", Box::new(|b| b.state.fingerprint = "f".repeat(64))),
        (
            "schema set",
            Box::new(|b| b.schema_set.fingerprint = "f".repeat(64)),
        ),
    ] {
        let why = changed(&*edit).expect_err(name);
        assert!(
            why.contains(&format!("the {name} format is still number")),
            "{why}"
        );
    }
    // With the number moved, the same fingerprint is the next release's to hold.
    assert_eq!(
        changed(&|b| {
            b.plan.fingerprint = another.clone();
            b.plan.version += 1;
        }),
        Ok(())
    );
    assert_eq!(
        check_record(Some(&released), &scratch.layout(), &build),
        Ok(())
    );
}

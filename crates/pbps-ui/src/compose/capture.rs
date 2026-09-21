use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use super::cli::{Cli, Paths};
use super::files::{self, FileBytes, MAX_BYTES, MAX_FILES, Root};
use super::git::{Git, text};
use super::{
    Candidate, CaptureBoundary, Config, Error, Manifest, Preview, Request, Result, SigningPolicy,
    destination, digest, random_id,
};

pub(super) struct Workspace {
    pub path: PathBuf,
}

impl Workspace {
    fn new(id: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("pbps-compose-{id}"));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| Error::new("Could not allocate a private candidate directory"))?;
        let workspace = Self { path };
        let path = &workspace.path;
        fs::create_dir(path.join("no-hooks"))
            .map_err(|_| Error::new("Could not create the private Git hooks directory"))?;
        fs::create_dir(path.join("repository"))
            .map_err(|_| Error::new("Could not create the candidate snapshot"))?;
        Ok(workspace)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // No published operation lives here yet. #747 owns durable recovery
        // and explicit retirement before any write endpoint is enabled.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone)]
struct TreeEntry {
    mode: u32,
    oid: String,
}

fn join(project: &str, name: &str) -> String {
    if project.is_empty() {
        name.to_owned()
    } else {
        format!("{project}/{name}")
    }
}

fn relative(root: &Path, value: &str) -> Result<String> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.strip_prefix(root).map_err(|_| {
            Error::new("The configured compose inputs must remain inside the project")
        })?
    } else {
        path
    };
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                files::path(
                    name.to_str()
                        .ok_or_else(|| Error::new("Compose paths must be UTF-8"))?,
                )?;
                clean.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir if clean.pop() => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(Error::new("Compose input paths cannot leave the project"));
            }
        }
    }
    let value = clean
        .to_str()
        .ok_or_else(|| Error::new("Compose paths must be UTF-8"))?;
    files::path(value)?;
    Ok(value.to_owned())
}

fn inputs(project: &Path, fields: Paths) -> Result<(String, String)> {
    if relative(project, &fields.project_file)? != "pbps.yml" {
        return Err(Error::new("The snapshot must resolve its own pbps.yml"));
    }
    let declarations = relative(project, &fields.declarations)?;
    let ids = relative(project, &fields.identity_file)?;
    if ids == "pbps.yml" || declarations == "pbps.yml" || ids == declarations {
        return Err(Error::new(
            "Compose configuration, declarations and identity paths must be distinct",
        ));
    }
    Ok((declarations, ids))
}

fn tree_entries(git: &Git, base: &str, project: &str) -> Result<BTreeMap<String, TreeEntry>> {
    // read-tree later creates a complete private index. Bound its entries too,
    // including paths outside a nested project that we never materialize.
    let index_paths = git.bytes(&["ls-tree", "-rz", "--name-only", base], &[], None)?;
    if index_paths.iter().filter(|b| **b == 0).count() > MAX_FILES {
        return Err(Error::new(
            "The base tree exceeds the private index entry limit",
        ));
    }
    let listing = git.bytes(
        &[
            "ls-tree",
            "-rz",
            base,
            "--",
            if project.is_empty() { "." } else { project },
        ],
        &[],
        None,
    )?;
    let mut result = BTreeMap::new();
    for row in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let row =
            std::str::from_utf8(row).map_err(|_| Error::new("Compose tree paths must be UTF-8"))?;
        let (header, path) = row
            .split_once('\t')
            .ok_or_else(|| Error::new("Could not read the base tree"))?;
        files::path(path)?;
        let fields: Vec<_> = header.split(' ').collect();
        let [mode, "blob", oid] = fields.as_slice() else {
            return Err(Error::new(
                "Compose project trees cannot contain submodules or non-blob entries",
            ));
        };
        let mode = match *mode {
            "100644" => 0o100644,
            "100755" => 0o100755,
            _ => {
                return Err(Error::new(
                    "Compose project trees cannot contain symbolic links",
                ));
            }
        };
        result.insert(
            path.to_owned(),
            TreeEntry {
                mode,
                oid: (*oid).to_owned(),
            },
        );
        if result.len() > MAX_FILES {
            return Err(Error::new("The project snapshot exceeds the file limit"));
        }
    }
    Ok(result)
}

fn blobs(git: &Git, entries: &BTreeMap<String, TreeEntry>) -> Result<BTreeMap<String, FileBytes>> {
    let request: String = entries
        .values()
        .map(|entry| format!("{}\n", entry.oid))
        .collect();
    let answer = git.bytes(&["cat-file", "--batch"], request.as_bytes(), None)?;
    let mut rest = answer.as_slice();
    let mut result = BTreeMap::new();
    let mut total = 0_usize;
    for (name, entry) in entries {
        let newline = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| Error::new("Git ended in the middle of a base object"))?;
        let header = std::str::from_utf8(&rest[..newline])
            .map_err(|_| Error::new("Git returned invalid object metadata"))?;
        let fields: Vec<_> = header.split(' ').collect();
        let [oid, "blob", length] = fields.as_slice() else {
            return Err(Error::new("Git could not read a required base blob"));
        };
        let length: usize = length
            .parse()
            .map_err(|_| Error::new("Git returned an invalid object length"))?;
        total = total
            .checked_add(length)
            .ok_or_else(|| Error::new("The project snapshot exceeds its size limit"))?;
        rest = &rest[newline + 1..];
        if *oid != entry.oid || total > MAX_BYTES || length >= rest.len() || rest[length] != b'\n' {
            return Err(Error::new(
                "The project snapshot is incomplete or exceeds its size limit",
            ));
        }
        result.insert(
            name.clone(),
            FileBytes {
                bytes: rest[..length].to_vec(),
                mode: entry.mode,
            },
        );
        rest = &rest[length + 1..];
    }
    if !rest.is_empty() {
        return Err(Error::new("Git returned unexpected base object data"));
    }
    Ok(result)
}

fn write_file(root: &Path, name: &str, content: &FileBytes) -> Result<()> {
    files::path(name)?;
    let path = root.join(name);
    // Only this private, fresh directory is written. All materialized names
    // were checked and every base object is a regular file, never a symlink.
    fs::create_dir_all(path.parent().expect("contained file parent"))
        .map_err(|_| Error::new("Could not create a private snapshot directory"))?;
    fs::write(&path, &content.bytes)
        .map_err(|_| Error::new("Could not write a private snapshot file"))?;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if content.mode == 0o100755 {
            0o700
        } else {
            0o600
        }),
    )
    .map_err(|_| Error::new("Could not preserve a candidate Git mode"))
}

fn signing(git: &Git) -> Result<SigningPolicy> {
    let answer = git.output(
        &["config", "--type=bool", "--get", "commit.gpgSign"],
        &[],
        None,
    )?;
    let required = match answer.status.code() {
        Some(0) => match text(answer.stdout)?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err(Error::new("Unusable signing policy")),
        },
        Some(1) if answer.stdout.is_empty() && answer.stderr.is_empty() => false,
        _ => return Err(Error::new("Could not determine signing policy")),
    };
    Ok(SigningPolicy {
        required,
        format: git.config("gpg.format")?,
        key: git.config("user.signingkey")?,
    })
}

fn attributes(git: &Git, name: &str, base: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut args = vec!["check-attr".to_owned(), "--all".into(), "-z".into()];
    if let Some(base) = base {
        args.push(format!("--source={base}"));
    }
    args.extend(["--".into(), name.to_owned()]);
    let output = git.bytes(&args, &[], None)?;
    let parts: Vec<_> = output.split(|b| *b == 0).collect();
    if parts.last() != Some(&&b""[..]) || (parts.len() - 1) % 3 != 0 {
        return Err(Error::new("Could not read complete Git attribute evidence"));
    }
    let mut result = Vec::new();
    for row in parts[..parts.len() - 1].as_chunks::<3>().0 {
        if row[0] != name.as_bytes() {
            return Err(Error::new("Git returned attributes for another input"));
        }
        let key = std::str::from_utf8(row[1])
            .map_err(|_| Error::new("Unsupported Git attribute name"))?;
        let value = std::str::from_utf8(row[2])
            .map_err(|_| Error::new("Unsupported Git attribute value"))?;
        // --all distinguishes an absent filter from one literally named
        // "unspecified". No check here ever executes a conversion program.
        if matches!(key, "filter" | "ident" | "working-tree-encoding") {
            return Err(Error::new(
                "Compose refuses filter, ident and working-tree-encoding attributes",
            ));
        }
        result.push((key.to_owned(), value.to_owned()));
    }
    result.sort();
    Ok(result)
}

/// Ask Git for its resolved policy using a private index and an empty worktree.
/// `ls-files --eol` never executes filters, and cannot read live declaration
/// bytes here. Its typed policy distinguishes `-text` from `text=unset`, which
/// check-attr's text output cannot distinguish.
struct AttributeView {
    directory: PathBuf,
    index: PathBuf,
    recorded: BTreeMap<String, Option<FileBytes>>,
}

impl AttributeView {
    fn new(
        git: &Git,
        base: &str,
        names: &BTreeSet<String>,
        workspace: &Path,
        placeholder: &str,
    ) -> Result<Self> {
        let directory = workspace.join("attribute-view");
        fs::create_dir(&directory)
            .map_err(|_| Error::new("Could not create the private attribute view"))?;
        let index = workspace.join("attribute.index");
        let index_options = [
            "-c",
            "core.splitIndex=false",
            "-c",
            "core.sparseCheckout=false",
            "-c",
            "index.sparse=false",
        ];
        let mut command = index_options.to_vec();
        command.extend(["read-tree", base]);
        git.bytes(&command, &[], Some(&index))?;
        // Placeholders expose both recorded and new input names. Attribute
        // lookup uses GIT_ATTR_SOURCE, never these placeholder contents.
        let mut records = Vec::new();
        for name in names {
            records.extend_from_slice(format!("100644 {placeholder}\t{name}\0").as_bytes());
        }
        let mut command = index_options.to_vec();
        command.extend(["update-index", "-z", "--index-info"]);
        git.bytes(&command, &records, Some(&index))?;
        let mut paths = BTreeSet::new();
        for name in names {
            let mut parent = Path::new(name).parent();
            while let Some(directory) = parent {
                paths.insert(
                    directory
                        .join(".gitattributes")
                        .to_string_lossy()
                        .into_owned(),
                );
                if paths.len() > MAX_FILES {
                    return Err(Error::new("Compose has too many attribute paths"));
                }
                parent = directory.parent();
            }
        }
        let listing = git.bytes(&["ls-tree", "-rz", base], &[], None)?;
        let mut entries = BTreeMap::new();
        for row in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = row.iter().position(|b| *b == b'\t') else {
                return Err(Error::new("Incomplete attribute tree metadata"));
            };
            let Ok(name) = std::str::from_utf8(&row[tab + 1..]) else {
                continue;
            };
            if !paths.contains(name) {
                continue;
            }
            let header = std::str::from_utf8(&row[..tab])
                .map_err(|_| Error::new("Invalid attribute tree metadata"))?;
            let fields: Vec<_> = header.split(' ').collect();
            let [mode @ ("100644" | "100755"), "blob", oid] = fields.as_slice() else {
                return Err(Error::new("Attribute files must be regular files"));
            };
            entries.insert(
                name.to_owned(),
                TreeEntry {
                    mode: if *mode == "100755" {
                        0o100755
                    } else {
                        0o100644
                    },
                    oid: (*oid).to_owned(),
                },
            );
        }
        let mut recorded = blobs(git, &entries)?;
        Ok(Self {
            directory,
            index,
            recorded: paths
                .into_iter()
                .map(|name| {
                    let bytes = recorded.remove(&name);
                    (name, bytes)
                })
                .collect(),
        })
    }

    fn files(&self, root: &Root) -> Result<BTreeMap<String, Option<super::Evidence>>> {
        let mut evidence = BTreeMap::new();
        for (name, expected) in &self.recorded {
            let current = root.read(name)?;
            if &current != expected {
                return Err(Error::new(
                    "Git attribute files changed from the selected base; commit them before composing",
                ));
            }
            evidence.insert(name.clone(), current.as_ref().map(FileBytes::evidence));
        }
        Ok(evidence)
    }

    fn policies(
        &self,
        git: &Git,
        base: &str,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, String>> {
        let mut command = git.command();
        command
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_WORK_TREE", &self.directory)
            .env("GIT_ATTR_SOURCE", base)
            .args(["ls-files", "--eol", "--cached", "-z", "--"])
            .args(names);
        let output = super::process::run(command, &[], git.deadline)?;
        if !output.status.success() {
            return Err(Error::new("Could not resolve Git line-ending policies"));
        }
        let mut result = BTreeMap::new();
        for row in output.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = row.iter().position(|b| *b == b'\t') else {
                return Err(Error::new("Incomplete Git line-ending policy"));
            };
            let Ok(name) = std::str::from_utf8(&row[tab + 1..]) else {
                continue;
            };
            if !names.contains(name) {
                continue;
            }
            let header = std::str::from_utf8(&row[..tab])
                .map_err(|_| Error::new("Invalid Git line-ending policy"))?;
            let (_, policy) = header
                .split_once("attr/")
                .ok_or_else(|| Error::new("Missing Git line-ending policy"))?;
            let policy = policy.trim_end();
            if !matches!(
                policy,
                "" | "-text"
                    | "text"
                    | "text=auto"
                    | "text eol=lf"
                    | "text eol=crlf"
                    | "text=auto eol=lf"
                    | "text=auto eol=crlf"
            ) || result.insert(name.to_owned(), policy.to_owned()).is_some()
            {
                return Err(Error::new(
                    "Unsupported or conflicting Git line-ending policy",
                ));
            }
        }
        if result.len() != names.len() {
            return Err(Error::new("Incomplete Git line-ending policies"));
        }
        Ok(result)
    }
}

fn manifest(
    git: &Git,
    root: &Root,
    names: &BTreeSet<String>,
    base: &str,
    view: &AttributeView,
) -> Result<(Manifest, BTreeMap<String, Option<FileBytes>>)> {
    let mut inputs = BTreeMap::new();
    let mut attr = BTreeMap::new();
    let mut bytes = BTreeMap::new();
    let mut total: usize = view
        .recorded
        .values()
        .flatten()
        .map(|file| file.bytes.len())
        .sum();
    let crlf = git.converts_line_endings()?;
    let attribute_files = view.files(root)?;
    let line_endings = view.policies(git, base, names)?;
    for name in names {
        let file = root.read(name)?;
        let working = attributes(git, name, None)?;
        if working != attributes(git, name, Some(base))? {
            return Err(Error::new(
                "Git attributes changed from the selected base; commit them before composing",
            ));
        }
        if let Some(file) = &file {
            total += file.bytes.len();
            if total > MAX_BYTES {
                return Err(Error::new("Compose inputs exceed the total size limit"));
            }
            let converts = match line_endings[name].as_str() {
                "-text" => false,
                "" => crlf,
                _ => true,
            };
            if file.bytes.contains(&b'\r') && converts {
                return Err(Error::new(
                    "Compose inputs must not require Git line-ending conversion",
                ));
            }
        }
        inputs.insert(name.clone(), file.as_ref().map(FileBytes::evidence));
        attr.insert(name.clone(), working);
        bytes.insert(name.clone(), file);
    }
    Ok((
        Manifest {
            inputs,
            attributes: attr,
            attribute_files,
            line_endings,
            autocrlf: crlf,
        },
        bytes,
    ))
}

pub(super) fn capture(
    config: &Config,
    request: Request,
    observer: &dyn Fn(CaptureBoundary),
) -> Result<Candidate> {
    if request.message.trim().is_empty()
        || request.message.len() > 16 * 1024
        || request.message.contains('\0')
    {
        return Err(Error::new("Provide a nonempty bounded commit message"));
    }
    let operation_id = random_id()?;
    let workspace = Workspace::new(&operation_id)?;
    let hooks = workspace.path.join("no-hooks");
    let selected_root = Root::open(&config.project)?;
    let selected = config
        .project
        .canonicalize()
        .map_err(|_| Error::new("Could not locate the selected project"))?;
    if !selected_root.same_directory(&Root::open(&selected)?)? {
        return Err(Error::new("The selected project changed during capture"));
    }
    let discovery = Git {
        root: selected.clone(),
        hooks: hooks.clone(),
        deadline: config.deadline,
    };
    let source = PathBuf::from(discovery.line(&["rev-parse", "--show-toplevel"])?);
    let relative_project = selected
        .strip_prefix(&source)
        .map_err(|_| Error::new("The selected project is outside its Git root"))?;
    let project = relative_project
        .to_str()
        .ok_or_else(|| Error::new("Compose project paths must be UTF-8"))?
        .to_owned();
    if !project.is_empty() {
        files::path(&project)?;
    }
    let git = Git {
        root: source.clone(),
        hooks: hooks.clone(),
        deadline: config.deadline,
    };
    if git
        .config("extensions.refStorage")?
        .is_some_and(|s| s != "files")
    {
        return Err(Error::new(
            "Compose is qualified for the files ref backend only",
        ));
    }
    let base = git.line(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    git.bytes(&["check-ref-format", &request.remote_base_ref], &[], None)?;
    if !request.remote_base_ref.starts_with("refs/heads/") {
        return Err(Error::new("Select one remote base branch"));
    }
    let destination = destination::resolve(&git, &request.remote)?;
    let signing = signing(&git)?;
    let root = Root::open(&source)?;
    let entries = tree_entries(&git, &base, &project)?;
    let recorded = blobs(&git, &entries)?;
    let project_file = join(&project, "pbps.yml");
    let recorded_config = recorded
        .get(&project_file)
        .ok_or_else(|| Error::new("Commit this project's pbps.yml before composing"))?;
    if root.read(&project_file)?.as_ref() != Some(recorded_config) {
        return Err(Error::new(
            "Project configuration must match the selected base before composing",
        ));
    }
    let snapshot = workspace.path.join("repository");
    for (name, file) in &recorded {
        write_file(&snapshot, name, file)?;
    }
    let snapshot_git = Git {
        root: snapshot.clone(),
        hooks,
        deadline: config.deadline,
    };
    let format = git.line(&["rev-parse", "--show-object-format"])?;
    snapshot_git.bytes(
        &[
            "init",
            "-q",
            &format!("--object-format={format}"),
            &format!("--template={}", snapshot_git.hooks.display()),
        ],
        &[],
        None,
    )?;
    let objects = git.line(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "objects",
    ])?;
    if objects.contains(['\n', '\r']) {
        return Err(Error::new("Unsupported Git object directory path"));
    }
    fs::write(
        snapshot.join(".git/objects/info/alternates"),
        format!("{objects}\n"),
    )
    .map_err(|_| Error::new("Could not connect the private Git object view"))?;
    snapshot_git.bytes(
        &["update-ref", "refs/heads/captured-base", &base],
        &[],
        None,
    )?;
    snapshot_git.bytes(
        &["symbolic-ref", "HEAD", "refs/heads/captured-base"],
        &[],
        None,
    )?;
    if let Some(operator) = git.config("user.name")? {
        snapshot_git.bytes(&["config", "--", "user.name", &operator], &[], None)?;
    }
    let cli = Cli {
        executable: config.executable.clone(),
        deadline: config.deadline,
    };
    let snapshot_project = snapshot.join(&project);
    let (declarations, ids) = inputs(&snapshot_project, cli.paths(&snapshot_project)?)?;
    observer(CaptureBoundary::PathsResolved);
    let declarations = join(&project, &declarations);
    let ids = join(&project, &ids);
    let live = root.declarations(&declarations)?;
    let is_input = |path: &str| {
        path == ids
            || (path.starts_with(&format!("{declarations}/")) && files::declaration_path(path))
    };
    let mut names: BTreeSet<String> = recorded.keys().filter(|p| is_input(p)).cloned().collect();
    names.extend(live.keys().cloned());
    names.extend([project_file.clone(), ids.clone()]);
    if names.len() > MAX_FILES {
        return Err(Error::new("Compose has too many input paths"));
    }
    let attribute_view = AttributeView::new(
        &git,
        &base,
        &names,
        &workspace.path,
        &entries[&project_file].oid,
    )?;
    let (evidence, captured) = manifest(&git, &root, &names, &base, &attribute_view)?;
    // Path admission was based on the base's configuration. The configuration
    // actually overlaid below must be those same bytes; checking the live file
    // before discovery alone leaves a config-change window before this capture.
    if captured.get(&project_file).and_then(Option::as_ref) != Some(recorded_config) {
        return Err(Error::new(
            "Project configuration changed during capture; refresh the preview",
        ));
    }
    if live
        .iter()
        .any(|(name, file)| captured.get(name) != Some(&Some(file.clone())))
    {
        return Err(Error::new("Declarations changed during capture"));
    }
    // Index flags are user intent to leave a path alone. Reading them does
    // not refresh the index, and no status/add command runs in the source.
    let flags = git.bytes(&["ls-files", "-v", "-z"], &[], None)?;
    for row in flags.split(|b| *b == 0).filter(|r| r.len() >= 3) {
        if let Ok(name) = std::str::from_utf8(&row[2..])
            && names.contains(name)
            && row[0] != b'H'
        {
            return Err(Error::new(
                "Compose inputs cannot carry skip-worktree, assume-unchanged or conflict flags",
            ));
        }
    }
    for name in live.keys().filter(|n| !recorded.contains_key(*n)) {
        if git.ignored(name)? {
            return Err(Error::new("A new declaration is ignored"));
        }
    }
    observer(CaptureBoundary::InputsRead);
    for (name, file) in &captured {
        match file {
            Some(file) => write_file(&snapshot, name, file)?,
            None if recorded.contains_key(name) => fs::remove_file(snapshot.join(name))
                .map_err(|_| Error::new("Could not represent an absent input in the snapshot"))?,
            None => {}
        }
    }
    cli.record(&snapshot_project, &request.intent, &base)?;
    observer(CaptureBoundary::IntentRecorded);
    let private = Root::open(&snapshot)?;
    let mut result = BTreeMap::new();
    for name in &names {
        result.insert(name.clone(), private.read(name)?);
    }
    cli.validate(&snapshot_project, &base)?;
    for (name, expected) in &result {
        if private.read(name)? != *expected {
            return Err(Error::new("Candidate inputs changed during validation"));
        }
    }
    let index = workspace.path.join("candidate.index");
    snapshot_git.bytes(&["read-tree", &base], &[], Some(&index))?;
    for (name, file) in &result {
        if name == &project_file {
            continue;
        }
        match file {
            Some(file) => {
                let oid = snapshot_git.store(&file.bytes)?;
                snapshot_git.bytes(
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("{:o},{oid},{name}", file.mode),
                    ],
                    &[],
                    Some(&index),
                )?;
            }
            None => {
                snapshot_git.bytes(
                    &["update-index", "--force-remove", "--", name],
                    &[],
                    Some(&index),
                )?;
            }
        }
    }
    let tree = text(snapshot_git.bytes(&["write-tree"], &[], Some(&index))?)?;
    let diff = String::from_utf8(snapshot_git.bytes(
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--text",
            "--no-color",
            &base,
            &tree,
            "--",
        ],
        &[],
        None,
    )?)
    .map_err(|_| Error::new("The candidate diff is not displayable UTF-8"))?;
    observer(CaptureBoundary::TreeBuilt);
    // Re-enumeration includes new declarations, not only paths from the first
    // read. The expected hashes remain those of the bytes already captured.
    let current_root = Root::open(&source)?;
    if !root.same_directory(&current_root)?
        || !selected_root.same_directory(&Root::open(&selected)?)?
        || current_root.declarations(&declarations)? != live
        || manifest(&git, &current_root, &names, &base, &attribute_view)?.0 != evidence
        || git.line(&["rev-parse", "--verify", "HEAD^{commit}"])? != base
        || destination::resolve(&git, &request.remote)? != destination
        || self::signing(&git)? != signing
    {
        return Err(Error::new(
            "Inputs, base or publication choices changed during capture; refresh the preview",
        ));
    }
    observer(CaptureBoundary::InputsChecked);
    let output_ref = format!("refs/heads/pbps-compose/{operation_id}");
    #[derive(Serialize)]
    struct Binding<'a> {
        version: u32,
        repository: &'a Path,
        project: &'a str,
        operation: &'a str,
        output_ref: &'a str,
        base: &'a str,
        tree: &'a str,
        manifest: &'a Manifest,
        request: &'a Request,
        destination: &'a super::Destination,
        signing: &'a SigningPolicy,
    }
    let binding = digest(
        &serde_json::to_vec(&Binding {
            version: 1,
            repository: &source,
            project: &project,
            operation: &operation_id,
            output_ref: &output_ref,
            base: &base,
            tree: &tree,
            manifest: &evidence,
            request: &request,
            destination: &destination,
            signing: &signing,
        })
        .map_err(|_| Error::new("Could not seal the candidate identity"))?,
    );
    let preview = Preview {
        candidate_id: random_id()?,
        operation_id,
        output_ref,
        base,
        tree,
        binding,
        diff,
        destination,
        signing,
    };
    Ok(Candidate {
        preview,
        request,
        manifest: evidence,
        source,
        project,
        workspace,
    })
}

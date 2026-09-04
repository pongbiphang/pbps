//! First-run project creation (SPEC §14.2).
//!
//! `init` is deliberately separate from ordinary project discovery: it creates
//! the file discovery is looking for. More importantly, it stages a complete,
//! loadable project before making any of it visible. Onboarding is where a
//! network failure, an invalid path, or a loader mismatch is most likely; none
//! of those should leave a directory that merely looks initialized.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::Args;
use pbps_config::{Config, ConfigError, DialectName, Environment, Hooks, Project, Unmanaged};
use pbps_model::{IdsFile, Schema};

use crate::{context, db, declaration_file};

/// Arguments for `pbps init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Database dialect for the new project
    #[arg(long, default_value = "mssql", value_parser = ["mssql"])]
    dialect: String,

    /// Add this named environment to pbps.yml
    #[arg(long, conflicts_with = "from")]
    env: Option<String>,

    /// Adopt this environment now by reverse-generating its declarations
    #[arg(long, value_name = "ENVIRONMENT")]
    from: Option<String>,

    /// Environment variable holding the connection string (defaults to <ENV>_CONN)
    #[arg(long, value_name = "VARIABLE")]
    url_env: Option<String>,
}

/// Owns the staging tree until it has been installed.
///
/// Staging holds a complete project — pbps.yml, the identity file and every
/// declaration — inside the user's root, so a failure that leaves one behind
/// puts a second, invisible project beside theirs. Making that a `Drop` rather
/// than a line before each `return` is what keeps a later `?` from becoming the
/// one path that strands it: `preview` already was.
struct Staging(PathBuf);

impl Staging {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // `commit` removes the tree itself on both of its paths, so on the
        // ordinary run this finds nothing and the error is the right thing to
        // discard.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Keeps `schema/` in git when it holds no declarations yet.
const GITKEEP: &str = ".gitkeep";

/// What `commit` found where the declaration directory goes, so a rollback can
/// put back exactly what it displaced.
enum Displaced {
    Nothing,
    EmptyDirectory,
    /// The directory held only an empty `.gitkeep`, the file init writes
    /// itself. Nothing of the user's is in it, so putting it back is writing
    /// the same empty file again.
    GitKeep,
}

struct Prepared {
    schema: Schema,
    ids: IdsFile,
    config: Config,
    config_text: String,
    warnings: Vec<String>,
    unmanaged: Vec<pbps_mssql::introspect::UnmanagedModule>,
}

/// Creates a project at `root` without requiring one to exist already.
pub fn cmd_init(root: &Path, args: &InitArgs) -> anyhow::Result<()> {
    let root = absolute(root)?;
    let dialect = match args.dialect.as_str() {
        "mssql" => DialectName::Mssql,
        // clap currently prevents this, but keeping the match exhaustive at the
        // boundary makes a future second value an intentional implementation.
        other => bail!("unsupported dialect `{other}`"),
    };

    let environment = args.from.as_ref().or(args.env.as_ref());
    if args.url_env.is_some() && environment.is_none() {
        bail!("--url-env needs an environment; pass --env <name> or --from <name>");
    }
    if let Some(name) = environment {
        validate_environment_name(name)?;
    }
    let url_env = match environment {
        Some(name) => Some(match &args.url_env {
            Some(var) => var.clone(),
            None => default_url_env(name)?,
        }),
        None => None,
    };
    if let Some(var) = &url_env {
        validate_variable_name(var)?;
    }

    match Project::discover(&root) {
        Ok(project) => bail!(
            "`{}` is already inside the pbps project rooted at `{}`",
            root.display(),
            project.root.display()
        ),
        Err(ConfigError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    refuse_existing(&root)?;

    let mut environments = BTreeMap::new();
    if let (Some(name), Some(var)) = (environment, &url_env) {
        environments.insert(
            name.clone(),
            Environment {
                url_env: var.clone(),
                description: None,
            },
        );
    }
    let config = Config {
        dialect,
        schema_dir: PathBuf::from("schema"),
        ids_file: PathBuf::from("schema.ids.json"),
        environments,
        hooks: Hooks::default(),
        unmanaged: Unmanaged::Ignore,
        dev: None,
        // Left to the default: a new project has no reference data yet, and a
        // number written into every generated pbps.yml is one more line to
        // explain in the first hour.
        max_data_rows: None,
        policies: None,
    };
    let config_text = render_config(&config);

    let (schema, ids, warnings, unmanaged) = match (&args.from, &url_env) {
        (Some(_), Some(var)) => {
            let connection = std::env::var(var).with_context(|| {
                format!(
                    "cannot adopt the database because ${var} is not set.\nExport it, then run the same `pbps init --from ...` command again."
                )
            })?;
            let pulled = db::runtime()?.block_on(async {
                let mut conn = pbps_db::Conn::connect(&connection).await?;
                pbps_mssql::catalog::introspect(&mut conn).await
            })?;
            let ids = mint_ids(&pulled.schema)?;
            (
                pulled.schema,
                ids,
                pulled.warnings,
                pulled.unmanaged_modules,
            )
        }
        (None, _) => (
            Schema::default(),
            IdsFile::default(),
            Vec::new(),
            Vec::new(),
        ),
        (Some(_), None) => unreachable!("--from always resolves a url variable"),
    };

    let prepared = Prepared {
        schema,
        ids,
        config,
        config_text,
        warnings,
        unmanaged,
    };
    for warning in &prepared.warnings {
        eprintln!("warning: {warning}");
    }
    if !prepared.unmanaged.is_empty() {
        eprintln!(
            "note: {} object(s) in this database cannot be managed:",
            prepared.unmanaged.len()
        );
        for module in &prepared.unmanaged {
            eprintln!("  {} {} — {}", module.kind, module.name, module.why);
        }
        eprintln!("  They are left untouched and do not appear in any plan.");
    }
    let stage = Staging(stage_project(&root, &prepared)?);
    preview(&root, stage.path())?;
    commit(&root, stage.path())?;

    println!("Initialized pbps project at `{}`.", root.display());
    if !prepared.warnings.is_empty() {
        println!(
            "{} catalog item(s) could not be expressed; see the warnings above.",
            prepared.warnings.len()
        );
    }
    if !prepared.unmanaged.is_empty() {
        println!(
            "{} module(s) were inventoried as unmanaged; pbps will leave them untouched.",
            prepared.unmanaged.len()
        );
    }
    match environment {
        Some(name) => {
            println!("Next: `pbps validate`");
            if args.from.is_some() {
                println!(
                    "Then commit the generated files and initialize this database's ledger:\n  `pbps baseline --env {name} --reason initial-adoption`\nAfter editing a declaration, run `pbps plan --env {name} --out plan.json --sql plan.sql`."
                );
            } else {
                println!(
                    "To adopt the existing database first, run `pbps pull --env {name}`; otherwise add declarations under `schema/`."
                );
            }
        }
        None => println!(
            "Next: add declarations under `schema/`, then run `pbps validate` and `pbps plan`."
        ),
    }
    Ok(())
}

/// Resolves `path` to an absolute path with no `..` left in it.
///
/// `Project::discover` walks this path's ancestors, and a lexical `..` puts
/// directories that are not ancestors on that walk: `pbps --project ../sibling
/// init` run from inside a project would refuse because it "is already inside"
/// the very project it is escaping. `std::path::absolute` does not help — on
/// Unix it keeps `..` on purpose, because dropping it lexically names the wrong
/// directory whenever a component is a symlink. So the deepest existing prefix
/// is canonicalized, which resolves `..` against the real filesystem, and the
/// part that does not exist yet is appended to it.
fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("cannot determine the current directory")?
            .join(path)
    };

    let mut existing = joined.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return Ok(missing.iter().rev().fold(resolved, |mut out, name| {
                out.push(name);
                out
            }));
        }
        // `file_name` is None for a path ending in `..`, which can only happen
        // when its parent does not exist either. Nothing can be resolved then,
        // so hand back the joined path rather than invent a normalization.
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return Ok(joined);
        };
        missing.push(name.to_owned());
        existing = parent;
    }
}

fn refuse_existing(root: &Path) -> anyhow::Result<()> {
    let config = root.join(pbps_config::CONFIG_FILE);
    if config.exists() {
        bail!(
            "`{}` already exists; this project is already initialized",
            config.display()
        );
    }
    let ids = root.join("schema.ids.json");
    if ids.exists() {
        bail!(
            "`{}` already exists; init will not overwrite project data",
            ids.display()
        );
    }
    let schema = root.join("schema");
    if schema.is_dir() {
        // A lone, empty `.gitkeep` is the file init writes itself, so refusing
        // it would block the very layout init is about to produce. The
        // emptiness is the whole tolerance: a `.gitkeep` someone wrote into
        // holds their content, and init replaces this file with its own.
        for entry in std::fs::read_dir(&schema)? {
            let entry = entry?;
            if entry.file_name() == GITKEEP
                && entry.metadata().map(|m| m.len() == 0).unwrap_or(false)
            {
                continue;
            }
            bail!(
                "`{}` is not empty (it contains `{}`); init will not overwrite project data",
                schema.display(),
                entry.path().display()
            );
        }
    }
    if schema.exists() && !schema.is_dir() {
        bail!("`{}` exists but is not a directory", schema.display());
    }
    Ok(())
}

fn default_url_env(name: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(name.len() + 5);
    for ch in name.chars() {
        match ch {
            'a'..='z' => out.push(ch.to_ascii_uppercase()),
            'A'..='Z' | '0'..='9' | '_' => out.push(ch),
            '-' => out.push('_'),
            _ => bail!("environment name `{name}` cannot be converted to a variable name"),
        }
    }
    out.push_str("_CONN");
    Ok(out)
}

fn validate_environment_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some('a'..='z' | 'A'..='Z'))
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        bail!(
            "environment name `{name}` must start with a letter and contain only letters, digits, `_` or `-`"
        );
    }
    Ok(())
}

fn validate_variable_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!(
            "environment variable `{name}` must start with a letter or `_` and contain only letters, digits or `_`"
        );
    }
    Ok(())
}

fn render_config(config: &Config) -> String {
    let mut out = format!(
        "# Generated by pbps {}; config schema 1.\ndialect: {}\n",
        env!("CARGO_PKG_VERSION"),
        config.dialect
    );
    if !config.environments.is_empty() {
        out.push_str("environments:\n");
        for (name, environment) in &config.environments {
            out.push_str(&format!(
                "  {}:\n    url_env: {}\n",
                yaml_string(name),
                yaml_string(&environment.url_env)
            ));
        }
    }
    out
}

/// JSON string syntax is a valid YAML double-quoted scalar. Always quoting the
/// generated values is simpler and safer than maintaining a second copy of the
/// declaration renderer's null/bool/number detection here.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn mint_ids(schema: &Schema) -> anyhow::Result<IdsFile> {
    pbps_diff::resolve(schema, &IdsFile::default(), &[], &context())
        .map(|resolved| resolved.ids)
        .map_err(|blockers| {
            anyhow::anyhow!(
                "init could not mint identities for the pulled schema: {} blocker(s)",
                blockers.len()
            )
        })
}

fn stage_project(root: &Path, prepared: &Prepared) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("cannot create project directory `{}`", root.display()))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stage = root.join(format!(".pbps-init-{}-{nonce}", std::process::id()));
    let schema_dir = stage.join(&prepared.config.schema_dir);
    std::fs::create_dir_all(&schema_dir)
        .with_context(|| format!("cannot create staging directory `{}`", schema_dir.display()))?;

    let result = (|| -> anyhow::Result<()> {
        // git tracks files, not directories, so a project initialized with no
        // declarations would lose `schema/` on the first clone and every
        // command would then fail on a directory the user never deleted. The
        // loader only reads `.yml`/`.yaml`, so this file is invisible to it.
        std::fs::write(schema_dir.join(GITKEEP), "")
            .with_context(|| format!("cannot stage `{}`", schema_dir.join(GITKEEP).display()))?;

        // Before the first declaration is staged: see `refuse_folded_paths`.
        declaration_file::refuse_folded_paths(&declaration_file::paths_of(
            &schema_dir,
            &prepared.schema,
        )?)?;
        for (name, table) in &prepared.schema.tables {
            let path = declaration_file::path(&schema_dir, name, None)?;
            std::fs::write(&path, pbps_load::render(name, table, &[], None))
                .with_context(|| format!("cannot stage `{}`", path.display()))?;
        }
        for (name, module) in &prepared.schema.modules {
            let path = declaration_file::path(&schema_dir, name, Some(module.kind))?;
            std::fs::write(
                &path,
                pbps_load::render_module(name, module, &Default::default()),
            )
            .with_context(|| format!("cannot stage `{}`", path.display()))?;
        }
        for (name, role) in &prepared.schema.roles {
            let path = declaration_file::role_path(&schema_dir, name)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot stage `{}`", parent.display()))?;
            }
            std::fs::write(&path, pbps_load::render_role(name, role, &[]))
                .with_context(|| format!("cannot stage `{}`", path.display()))?;
        }
        std::fs::write(stage.join(pbps_config::CONFIG_FILE), &prepared.config_text)
            .context("cannot stage pbps.yml")?;
        std::fs::write(
            stage.join(&prepared.config.ids_file),
            format!("{}\n", serde_json::to_string_pretty(&prepared.ids)?),
        )
        .context("cannot stage schema.ids.json")?;

        // Validate from the serialized files, not the values that produced
        // them. This catches renderer/loader drift before the project appears.
        let project = Project::load(&stage.join(pbps_config::CONFIG_FILE))?;
        let loaded = pbps_load::load_schema_dir(&project.schema_dir()).map_err(|errors| {
            anyhow::anyhow!("the staged declarations have {} problem(s)", errors.len())
        })?;
        if loaded.schema != prepared.schema {
            bail!("the staged declarations do not round-trip to the pulled schema");
        }
        // The whole list, not the half this function used to enumerate: the
        // roles and rows a `pull --data` writes are checked by the checks that
        // own them, and a staged project that `pbps validate` would reject is
        // one this command must not leave behind (DECISIONS 141).
        let dialect = pbps_mssql::Mssql;
        let dialect_problems: Vec<String> = crate::declaration_problems(&loaded, &dialect)
            .into_iter()
            .map(|(_, problem)| problem)
            .collect();
        if !dialect_problems.is_empty() {
            bail!(
                "the staged declarations are not valid for mssql:\n  {}",
                dialect_problems.join("\n  ")
            );
        }
        let ids: IdsFile = serde_json::from_str(&std::fs::read_to_string(project.ids_file())?)?;
        ids.validate()?;
        if ids != prepared.ids {
            bail!("the staged identity file does not round-trip");
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(error);
    }
    Ok(stage)
}

fn preview(root: &Path, stage: &Path) -> anyhow::Result<()> {
    println!("Files to create:");
    let mut files = Vec::new();
    collect_files(stage, &mut files)?;
    files.sort();
    for path in files {
        let relative = path.strip_prefix(stage).unwrap_or(&path);
        println!("  {}", root.join(relative).display());
    }
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Commits the staged tree with pbps.yml last.
///
/// Discovery treats pbps.yml as the marker that a project exists. Moving that
/// marker last means another process can observe either no project or the whole
/// project, never a config pointing at files that have not arrived yet.
fn commit(root: &Path, stage: &Path) -> anyhow::Result<()> {
    // Introspection may take minutes. Anything that appeared while it was
    // running belongs to the user or another init and must win. This recheck
    // catches directories and declarations; the file installs below are also
    // atomic no-replace operations, closing the race after this check.
    if let Err(error) = refuse_existing(root) {
        let _ = std::fs::remove_dir_all(stage);
        return Err(error.context("the project changed while init was preparing its output"));
    }

    let final_schema = root.join("schema");
    let staged_schema = stage.join("schema");
    let final_ids = root.join("schema.ids.json");

    // schema.ids.json is installed first and released last: it is the exclusive
    // claim on this root. The recheck above cannot serialize two inits on its
    // own, because `rename` of a directory replaces an existing *empty* one on
    // Unix — both would believe they owned `schema`, and the loser's rollback
    // would delete the winner's declarations. An atomic no-replace link stops
    // the loser here, before it has touched anything.
    if let Err(error) = install_file_no_replace(stage.join("schema.ids.json"), &final_ids) {
        let _ = std::fs::remove_dir_all(stage);
        return Err(error);
    }

    let mut displaced = Displaced::Nothing;
    let mut schema_moved = false;
    let result = (|| -> anyhow::Result<()> {
        if final_schema.is_dir() {
            let keep = final_schema.join(GITKEEP);
            // `refuse_existing` has already established that an empty
            // `.gitkeep` is the only thing this directory may hold, so removing
            // it removes a file init writes itself. A failure here needs no
            // branch: `remove_dir` below then reports the real obstacle.
            displaced = if keep.is_file() {
                let _ = std::fs::remove_file(&keep);
                Displaced::GitKeep
            } else {
                Displaced::EmptyDirectory
            };
            if let Err(source) = std::fs::remove_dir(&final_schema) {
                // The directory may have changed after the initial emptiness
                // check. Never remove its contents: they belong to the user.
                return Err(anyhow::Error::new(source).context(format!(
                    "cannot replace the empty declaration directory `{}`",
                    final_schema.display()
                )));
            }
        }

        std::fs::rename(&staged_schema, &final_schema).with_context(|| {
            format!(
                "cannot install declarations at `{}`",
                final_schema.display()
            )
        })?;
        schema_moved = true;

        install_file_no_replace(
            stage.join(pbps_config::CONFIG_FILE),
            &root.join(pbps_config::CONFIG_FILE),
        )?;
        Ok(())
    })();

    if let Err(error) = result {
        if schema_moved {
            let _ = std::fs::remove_dir_all(&final_schema);
        }
        match &displaced {
            Displaced::Nothing => {}
            Displaced::EmptyDirectory => {
                let _ = std::fs::create_dir(&final_schema);
            }
            Displaced::GitKeep => {
                let _ = std::fs::create_dir(&final_schema);
                let _ = std::fs::write(final_schema.join(GITKEEP), "");
            }
        }
        // Released last, so no other init can reach the schema move while this
        // rollback is undoing it.
        let _ = std::fs::remove_file(&final_ids);
        let _ = std::fs::remove_dir_all(stage);
        return Err(error);
    }
    if let Err(error) = std::fs::remove_dir_all(stage) {
        eprintln!(
            "warning: project is complete, but staging directory `{}` could not be removed: {error}",
            stage.display()
        );
    }
    Ok(())
}

/// Installs one staged file without ever replacing an existing destination.
///
/// Staging lives under the project root, so source and destination are on one
/// filesystem. A hard link is therefore an atomic create-if-absent on Unix and
/// Windows; unlike `rename`, it fails when another process created the target
/// during introspection. The staging link is removed with the staging tree once
/// every destination is installed.
fn install_file_no_replace(source: impl AsRef<Path>, destination: &Path) -> anyhow::Result<()> {
    let source = source.as_ref();
    std::fs::hard_link(source, destination).map_err(|source_error| {
        // The kind is the only honest witness. Probing `destination.exists()`
        // is both racy and wrong for the other failures: a filesystem with no
        // hard links (exFAT, SMB without the unix extensions) or an EPERM would
        // have been reported as an existing file that is not there.
        let context = if source_error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "refusing to replace `{}`; it appeared while init was preparing its output",
                destination.display()
            )
        } else {
            format!(
                "cannot install `{}` at `{}`.\ninit installs files with a hard link so that it can never replace existing data; a filesystem that does not support one cannot be initialized in place.",
                source.display(),
                destination.display()
            )
        };
        anyhow::Error::new(source_error).context(context)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_names_have_deterministic_connection_variables() {
        assert_eq!(default_url_env("prod").unwrap(), "PROD_CONN");
        assert_eq!(default_url_env("qa-east").unwrap(), "QA_EAST_CONN");
    }

    #[test]
    fn unsafe_yaml_names_are_rejected() {
        assert!(validate_environment_name("prod: hacked").is_err());
        assert!(validate_environment_name("9prod").is_err());
        assert!(validate_variable_name("PROD-CONN").is_err());
    }

    #[test]
    fn generated_config_carries_tool_and_schema_versions() {
        let mut environments = BTreeMap::new();
        environments.insert(
            "null".to_owned(),
            Environment {
                url_env: "null".to_owned(),
                description: None,
            },
        );
        let config = Config {
            dialect: DialectName::Mssql,
            schema_dir: PathBuf::from("schema"),
            ids_file: PathBuf::from("schema.ids.json"),
            environments,
            hooks: Hooks::default(),
            unmanaged: Unmanaged::Ignore,
            dev: None,
            max_data_rows: None,
            policies: None,
        };
        let text = render_config(&config);
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains("config schema 1"), "{text}");
        let parsed = Config::parse(&text, Path::new("pbps.yml")).unwrap();
        assert_eq!(parsed.environments["null"].url_env, "null");
        assert!(text.contains("\"null\""), "{text}");
    }

    #[test]
    fn a_parent_component_is_resolved_before_the_project_is_discovered() {
        // `Project::discover` walks this path's ancestors, so a lexical `..`
        // would put the project being escaped from on that walk.
        let root = std::env::temp_dir().join(format!("pbps-init-abs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let real = root.canonicalize().unwrap();

        let resolved = absolute(&root.join("a/b/../newproj")).unwrap();
        assert_eq!(resolved, real.join("a/newproj"));
        assert!(
            !resolved.components().any(|c| c.as_os_str() == ".."),
            "{resolved:?}"
        );
        // A path that exists resolves to itself, and a genuinely nested one
        // still has the parent on its ancestor walk.
        assert_eq!(absolute(&root.join("a/b")).unwrap(), real.join("a/b"));
        assert_eq!(
            absolute(&root.join("a/b/deep/newproj")).unwrap(),
            real.join("a/b/deep/newproj")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staging_is_discarded_unless_it_was_installed() {
        let root = std::env::temp_dir().join(format!("pbps-init-staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stage = root.join(".pbps-init-x");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("pbps.yml"), "generated\n").unwrap();

        drop(Staging(stage.clone()));
        assert!(
            !stage.exists(),
            "a complete project must not be left in the user's root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_link_failure_that_is_not_a_collision_reports_its_own_cause() {
        let root = std::env::temp_dir().join(format!("pbps-init-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let destination = root.join("pbps.yml");
        std::fs::write(&destination, "user data\n").unwrap();

        // Source missing and destination present: probing the destination would
        // blame a collision for a failure that is nothing of the kind, which is
        // exactly what a filesystem without hard links would also be told.
        let error = install_file_no_replace(root.join("absent"), &destination).unwrap_err();
        assert!(
            !error.to_string().contains("refusing to replace"),
            "{error:#}"
        );
        assert!(error.to_string().contains("cannot install"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "user data\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_lone_gitkeep_does_not_look_like_project_data() {
        let root = std::env::temp_dir().join(format!("pbps-init-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("schema").join(GITKEEP), "").unwrap();
        refuse_existing(&root).expect("init writes this file itself");

        // A `.gitkeep` someone wrote into holds their content, and init would
        // replace it, so it is project data like any other file.
        std::fs::write(root.join("schema").join(GITKEEP), "mine\n").unwrap();
        let error = refuse_existing(&root).unwrap_err();
        assert!(error.to_string().contains("is not empty"), "{error:#}");

        std::fs::write(root.join("schema").join(GITKEEP), "").unwrap();
        std::fs::write(root.join("schema").join("dbo.t.yml"), "table: dbo.t\n").unwrap();
        let error = refuse_existing(&root).unwrap_err();
        assert!(error.to_string().contains("is not empty"), "{error:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rollback_puts_back_the_gitkeep_it_displaced() {
        let root = std::env::temp_dir().join(format!("pbps-init-keep-back-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stage = root.join(".pbps-init-test");
        std::fs::create_dir_all(stage.join("schema")).unwrap();
        std::fs::write(stage.join("schema.ids.json"), "generated ids\n").unwrap();
        // No staged pbps.yml, so the install fails after the schema move.
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("schema").join(GITKEEP), "").unwrap();

        commit(&root, &stage).unwrap_err();
        assert!(
            root.join("schema").join(GITKEEP).is_file(),
            "the rollback must restore the layout it displaced"
        );
        assert!(!root.join("schema.ids.json").exists(), "claim not released");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_destination_that_appeared_is_never_replaced() {
        let root =
            std::env::temp_dir().join(format!("pbps-init-no-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let staged = root.join("staged");
        let destination = root.join("pbps.yml");
        std::fs::write(&staged, "generated\n").unwrap();
        std::fs::write(&destination, "user data\n").unwrap();

        let error = install_file_no_replace(&staged, &destination).unwrap_err();
        assert!(
            error.to_string().contains("refusing to replace"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "user data\n"
        );
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "generated\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The recheck alone cannot serialize two inits: on Unix `rename` replaces
    /// an existing *empty* directory, so both could pass the recheck and both
    /// move `schema` — then the loser's rollback deletes the directory the
    /// winner had already installed, leaving pbps.yml and schema.ids.json with
    /// no `schema/` beside them.
    ///
    /// The staged directory is empty on purpose: that is what plain `pbps init`
    /// produces, and a populated one would make the second `rename` fail with
    /// ENOTEMPTY and hide the race.
    #[test]
    fn a_losing_init_never_removes_the_winners_declarations() {
        let root = std::env::temp_dir().join(format!("pbps-init-two-{}", std::process::id()));
        for round in 0..100 {
            let root = root.join(format!("round-{round}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            // The empty declaration directory init tolerates, which is exactly
            // what makes the directory move replace rather than fail.
            std::fs::create_dir(root.join("schema")).unwrap();

            let stages: Vec<PathBuf> = ["a", "b", "c", "d", "e", "f", "g", "h"]
                .iter()
                .map(|which| {
                    let stage = root.join(format!(".pbps-init-{which}"));
                    std::fs::create_dir_all(stage.join("schema")).unwrap();
                    std::fs::write(stage.join("schema.ids.json"), which).unwrap();
                    std::fs::write(stage.join("pbps.yml"), which).unwrap();
                    stage
                })
                .collect();

            let barrier = std::sync::Barrier::new(stages.len());
            let outcomes: Vec<bool> = std::thread::scope(|scope| {
                let handles: Vec<_> = stages
                    .iter()
                    .map(|stage| {
                        let (root, barrier) = (&root, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            commit(root, stage).is_ok()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            assert_eq!(
                outcomes.iter().filter(|ok| **ok).count(),
                1,
                "exactly one init may claim the root, got {outcomes:?}"
            );
            // The winner's project must be whole: the reported failure left
            // pbps.yml and schema.ids.json behind with no `schema/`.
            let winner = std::fs::read_to_string(root.join("pbps.yml")).unwrap_or_else(|e| {
                panic!("round {round}: an init succeeded but left no config: {e}")
            });
            assert_eq!(
                std::fs::read_to_string(root.join("schema.ids.json")).ok(),
                Some(winner),
                "round {round}: the identity file does not belong to the init that won"
            );
            assert!(
                root.join("schema").is_dir(),
                "round {round}: the loser deleted the directory the winner installed"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_commit_recheck_discards_staging_not_new_user_data() {
        let root =
            std::env::temp_dir().join(format!("pbps-init-commit-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stage = root.join(".pbps-init-test");
        std::fs::create_dir_all(stage.join("schema")).unwrap();
        std::fs::write(stage.join("schema.ids.json"), "generated ids\n").unwrap();
        std::fs::write(stage.join("pbps.yml"), "generated config\n").unwrap();
        // Simulates a file created while a long catalog read was in progress.
        std::fs::write(root.join("pbps.yml"), "user config\n").unwrap();

        let error = commit(&root, &stage).unwrap_err();
        assert!(error.to_string().contains("project changed"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(root.join("pbps.yml")).unwrap(),
            "user config\n"
        );
        assert!(!stage.exists(), "staging must be discarded on the race");
        let _ = std::fs::remove_dir_all(&root);
    }
}

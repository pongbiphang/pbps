//! `pbps resolver externals`: draft a resolver's externals file from the
//! target, for review (#1778, SPEC §9.3.2).
//!
//! What the draft holds is `pbps-pg`'s ([`pbps_pg::resolver::draft`]); this
//! file reads the project, guards the destination and prints. The draft is a
//! starting point a reviewer edits, so it never replaces a file the project
//! already uses: the externals file a resolver entry names is reviewed
//! history, and a regenerated draft over it would silently drop what the
//! operator wrote by hand.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use pbps_config::Project;

use crate::{db, paths};

pub fn cmd_externals(
    project: &Project,
    target: &db::Target,
    schemas: &[String],
    out: Option<&Path>,
    force: bool,
) -> anyhow::Result<()> {
    if target.driver() != pbps_db::Driver::Postgres {
        bail!(
            "`pbps resolver externals` reads a PostgreSQL target; SQL Server has no binding \
             adapter yet (#619, #620)"
        );
    }
    // Before connecting: where the draft goes is a fact about the command
    // line and the project.
    if let Some(out) = out {
        refuse_destination(project, out, force)?;
    }
    let dialect = crate::dialect(project)?;
    let loaded = crate::load(project, dialect.as_ref())?;
    let draft = db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        pbps_pg::resolver::draft::draft(&mut conn, &loaded.schema, schemas)
            .await
            .context("cannot draft the externals file")
    })?;
    match out {
        Some(out) => {
            write(out, &draft.sql, force)?;
            eprintln!("wrote {}", out.display());
        }
        None => print!("{}", draft.sql),
    }
    for line in &draft.listed {
        eprintln!("listed: {line}");
    }
    Ok(())
}

/// Refuses a destination the project already uses, whatever `force` says,
/// and an existing file unless `force`.
fn refuse_destination(project: &Project, out: &Path, force: bool) -> anyhow::Result<()> {
    // A link's destination is what gets written, and a dangling one cannot
    // be resolved to compare with what the project reads.
    if std::fs::symlink_metadata(out).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "`{}` is a symbolic link; write the draft to a regular file or a new path",
            out.display()
        );
    }
    for (name, profile) in &project.config.resolvers {
        let (pbps_config::resolver::ResolverProfile::Docker { externals, .. }
        | pbps_config::resolver::ResolverProfile::Server { externals, .. }) = profile;
        if let Some(path) = externals
            && paths::same_file(out, &project.root.join(path))?
        {
            bail!(
                "`{}` is the externals file resolver `{name}` names; a draft never replaces it, \
                 since it is reviewed and may hold what was written by hand. Write the draft \
                 elsewhere and compare",
                out.display()
            );
        }
    }
    // Every file under the declarations' directory is one the loader may
    // read, including one that does not exist yet.
    let schema_dir = paths::absolute(&project.schema_dir())?;
    if paths::absolute(out)?.starts_with(&schema_dir) {
        bail!(
            "`{}` is under the declarations' directory `{}`, which the project reads; write \
             the draft elsewhere",
            out.display(),
            schema_dir.display()
        );
    }
    let mut inputs = vec![project.config_file(), project.ids_file()];
    // A listing that failed is not an empty directory (the loader reports
    // it); what could be listed is still protected.
    if let Ok(files) = pbps_load::schema_files(&project.schema_dir()) {
        inputs.extend(files);
    }
    for environment in project.config.environments.values() {
        if let Some(path) = &environment.fingerprint_key_file {
            inputs.push(project.root.join(path));
        }
    }
    for input in &inputs {
        if paths::same_file(out, input)? {
            bail!(
                "`{}` is the project's `{}`; write the draft elsewhere",
                out.display(),
                input.display()
            );
        }
    }
    if !force && std::fs::symlink_metadata(out).is_ok() {
        bail!(
            "`{}` exists; pass --force to replace it, or choose another --out",
            out.display()
        );
    }
    Ok(())
}

fn write(out: &Path, sql: &str, force: bool) -> anyhow::Result<()> {
    let mut file = if force {
        std::fs::File::create(out)
    } else {
        // Checked above, and again here: a file that appeared in between is
        // still not replaced.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(out)
    }
    .with_context(|| format!("cannot write `{}`", out.display()))?;
    file.write_all(sql.as_bytes())
        .with_context(|| format!("cannot write `{}`", out.display()))
}

//! Safe, deterministic filenames for generated declarations.
//!
//! Declaration filenames carry no meaning to the loader, but generated names
//! still need three properties: ordinary identifiers should stay readable,
//! different database names must not collide, and no legal quoted identifier
//! may become a path component. In particular, SQL Server accepts `/` and `..`
//! inside quoted identifiers; joining either spelling directly would let a pull
//! or init write outside `schema/`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::bail;
use pbps_model::{ModuleKind, ObjectName};
use sha2::{Digest as _, Sha256};

/// Leaves the common case readable and percent-encodes every byte that could
/// make the component ambiguous or meaningful to a filesystem.
fn component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            out.push(char::from(*byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Windows resolves these stems as devices even with an extension appended, so
/// `CON.customer.yml` cannot be created there. They are legal quoted SQL Server
/// schema names, and only the leading stem is interpreted, so the escape is
/// applied to the schema component alone.
///
/// The first byte is percent-encoded rather than prefixed: `component` emits
/// `%` only for bytes outside its safe alphabet, and the escaped byte is
/// alphanumeric, so no other identifier can encode to the same string. The
/// escape is unconditional — declarations are written into git and must resolve
/// to the same filename on every platform that checks the repository out.
fn escape_reserved_stem(encoded: String) -> String {
    const RESERVED: [&str; 24] = [
        "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9",
    ];
    if !RESERVED.iter().any(|r| encoded.eq_ignore_ascii_case(r)) {
        return encoded;
    }
    let (first, rest) = encoded.split_at(1);
    format!("%{:02X}{rest}", first.as_bytes()[0])
}

/// The file a role is written to: `roles/<name>.yml`, with the same encoding
/// as an object name's components and the same hashed fallback.
///
/// A directory of its own, not a suffix. `<name>.role.yml` was the first
/// cut, and a table `app_reader.role` produces exactly `app_reader.role.yml`,
/// so `pull` wrote the role over the table without a word. No table file is
/// ever written under `roles/` (a table's file is `<schema>.<name>.yml` at
/// the top), so the two can no longer name one path. The loader reads
/// every `.yml` under the schema directory and tells a role by its content,
/// so a hand-written role file elsewhere still loads.
pub fn role_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let readable = format!("{}.yml", escape_reserved_stem(component(name)));
    let file = if readable.len() <= 240 {
        readable
    } else {
        let mut hasher = Sha256::new();
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(b"role");
        format!("~pbps-{:x}.yml", hasher.finalize())
    };
    let roles = dir.join("roles");
    let path = roles.join(&file);
    if path.parent() != Some(roles.as_path()) {
        bail!("refusing to write `{file}` outside `{}`", roles.display());
    }
    Ok(path)
}

fn filename(name: &ObjectName, kind: Option<ModuleKind>) -> String {
    filename_of(&name.schema, &name.name, kind)
}

/// The file a module is written to, under its whole identity.
///
/// The whole identity, because anything less lets `pull` write one
/// declaration over another (ADR-0009 §1, DECISIONS 197). A routine carries
/// its argument types — `app.f(integer)` and `app.f(text)` are two
/// declarations — and a trigger carries its table, because `audit` on
/// `app.orders` and `audit` on `app.customers` are two more. `component`
/// percent-encodes the parentheses, commas and the separating dot, so the
/// name stays one path component.
pub fn module_path(
    directory: &Path,
    id: &pbps_model::ModuleId,
    kind: ModuleKind,
) -> anyhow::Result<PathBuf> {
    let name = id.object_name();
    let stem = match id {
        pbps_model::ModuleId::Named(n) => n.name.clone(),
        pbps_model::ModuleId::Routine(r) => {
            let spelled: Vec<String> = r.args.iter().map(ToString::to_string).collect();
            format!("{}({})", r.name.name, spelled.join(","))
        }
        // The table's own name only: its schema is `name.schema`, which the
        // filename already carries, and repeating it would make
        // `app.app.orders.audit`.
        pbps_model::ModuleId::Trigger { on, name } => format!("{}.{}", on.name, name),
    };
    let path = directory.join(filename_of(&name.schema, &stem, Some(kind)));
    if path.parent() != Some(directory) {
        bail!(
            "generated declaration path for `{id}` escaped `{}`",
            directory.display()
        );
    }
    Ok(path)
}

fn filename_of(schema: &str, name: &str, kind: Option<ModuleKind>) -> String {
    let kind_suffix = kind.map(|k| format!(".{}", k.as_str())).unwrap_or_default();
    let readable = format!(
        "{}.{}{}.yml",
        escape_reserved_stem(component(schema)),
        component(name),
        kind_suffix
    );
    // SQL Server identifiers can be long Unicode strings. Percent-encoding all
    // their bytes may exceed a filesystem's 255-byte filename limit; in that
    // rare case a full SHA-256 keeps the name deterministic and collision-safe.
    if readable.len() <= 240 {
        return readable;
    }

    let mut hasher = Sha256::new();
    hasher.update(schema.as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(kind.map(ModuleKind::as_str).unwrap_or("table").as_bytes());
    // `~` is deliberately outside `component`'s safe alphabet (a literal one
    // becomes `%7E`), so no readable schema/object pair can ever occupy this
    // hashed namespace.
    format!("~pbps-{hash:x}{kind_suffix}.yml", hash = hasher.finalize())
}

/// Returns a declaration path whose parent is exactly `directory`.
pub fn path(
    directory: &Path,
    name: &ObjectName,
    kind: Option<ModuleKind>,
) -> anyhow::Result<PathBuf> {
    let path = directory.join(filename(name, kind));
    if path.parent() != Some(directory) {
        // This should be unreachable because `filename` emits one encoded
        // component. Keep the check at the write boundary: a later refactor of
        // the encoding must fail closed instead of reopening path traversal.
        bail!(
            "generated declaration path for `{name}` escaped `{}`",
            directory.display()
        );
    }
    Ok(path)
}

/// Every file a schema's declarations are written to, each under the name a
/// human calls the thing.
pub fn paths_of(
    directory: &Path,
    schema: &pbps_model::Schema,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for name in schema.tables.keys() {
        out.push((name.to_string(), path(directory, &name.clone(), None)?));
    }
    for (id, module) in &schema.modules {
        out.push((
            format!("{id} ({})", module.kind.as_str()),
            module_path(directory, id, module.kind)?,
        ));
    }
    for name in schema.roles.keys() {
        out.push((format!("role {name}"), role_path(directory, name)?));
    }
    Ok(out)
}

/// Refuses two declarations whose files differ only in case, before either
/// is written.
///
/// A case-sensitive database holds `Reader` beside `reader`, and their
/// encoded filenames differ only in case; on a case-insensitive filesystem
/// the second `pull` wrote over the first, with the identity file still
/// naming both — declarations that cannot round-trip, and nothing said. The
/// encoding cannot fix this by itself: a declaration is written into git and
/// has to resolve to the same file on every platform that checks the
/// repository out, so the refusal is unconditional rather than a property of
/// the filesystem underneath (DECISIONS 135).
pub fn refuse_folded_paths(paths: &[(String, PathBuf)]) -> anyhow::Result<()> {
    let mut by_folded: std::collections::BTreeMap<String, Vec<&str>> =
        std::collections::BTreeMap::new();
    for (label, path) in paths {
        by_folded
            .entry(path.to_string_lossy().to_lowercase())
            .or_default()
            .push(label);
    }
    let clashing: Vec<String> = by_folded
        .iter()
        .filter(|(_, labels)| labels.len() > 1)
        .map(|(folded, labels)| format!("{} -> `{folded}`", labels.join(", ")))
        .collect();
    if !clashing.is_empty() {
        bail!(
            "{} declaration file name(s) differ only in case, and a filesystem that \
             ignores case would keep one of each:\n  {}\n\
             Rename one side in the database, or declare only one of them.",
            clashing.len(),
            clashing.join("\n  ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trigger's table is half of its identity (ADR-0009 §1), so two
    /// triggers named `audit` on different tables in one schema are two
    /// declarations. One filename for both would have `pull` write the second
    /// over the first, and the next plan would drop the trigger whose file
    /// vanished — the identity this phase added, undone on the way to disk.
    #[test]
    fn same_named_triggers_on_different_tables_get_different_files() {
        let dir = Path::new("/tmp/schema");
        let orders: pbps_model::ModuleId = "app.orders.audit".parse().unwrap();
        let customers: pbps_model::ModuleId = "app.customers.audit".parse().unwrap();
        let a = module_path(dir, &orders, ModuleKind::Trigger).unwrap();
        let b = module_path(dir, &customers, ModuleKind::Trigger).unwrap();
        assert_ne!(a, b, "one file for two triggers");
        // And the table is what distinguishes them, not a hash nobody can read
        // back to the declaration it came from.
        assert!(a.to_string_lossy().contains("orders"), "{}", a.display());
        assert!(b.to_string_lossy().contains("customers"), "{}", b.display());
    }

    /// A case-sensitive database holds `Reader` beside `reader`; the files
    /// they encode to differ only in case, and one filesystem in two keeps
    /// only one of them.
    #[test]
    fn two_declarations_whose_files_differ_only_in_case_are_refused() {
        let dir = Path::new("/tmp/schema");
        let mut schema = pbps_model::Schema::default();
        for name in ["dbo.Customer", "dbo.customer", "dbo.order"] {
            schema
                .tables
                .insert(name.parse().unwrap(), pbps_model::Table::default());
        }
        schema
            .roles
            .insert("Reader".to_owned(), pbps_model::Role::default());
        schema
            .roles
            .insert("reader".to_owned(), pbps_model::Role::default());

        let paths = paths_of(dir, &schema).unwrap();
        let err = refuse_folded_paths(&paths).unwrap_err().to_string();
        assert!(err.contains("2 declaration file name(s)"), "{err}");
        assert!(err.contains("dbo.Customer, dbo.customer"), "{err}");
        assert!(err.contains("role Reader, role reader"), "{err}");
        // The one that collides with nothing is not named.
        assert!(!err.contains("dbo.order"), "{err}");

        // And the ordinary case says nothing: a role and a table of one name
        // live in different directories, and always did.
        let mut plain = pbps_model::Schema::default();
        plain
            .tables
            .insert("dbo.reader".parse().unwrap(), pbps_model::Table::default());
        plain
            .roles
            .insert("reader".to_owned(), pbps_model::Role::default());
        refuse_folded_paths(&paths_of(dir, &plain).unwrap()).expect("no collision");
    }

    /// `app_reader.role` is a legal table name whose file used to be the
    /// role `app_reader`'s file; `pull` then wrote one over the other.
    #[test]
    fn a_role_file_cannot_share_a_path_with_any_table_file() {
        let dir = Path::new("schema");
        let role = role_path(dir, "app_reader").unwrap();
        assert_eq!(role, dir.join("roles").join("app_reader.yml"));
        let table = path(dir, &"app_reader.role".parse().unwrap(), None).unwrap();
        assert_ne!(role, table);
        assert_eq!(table, dir.join("app_reader.role.yml"));
        // A role named after a table's file still lands beside the roles.
        let role = role_path(dir, "dbo.customer").unwrap();
        assert!(role.starts_with(dir.join("roles")), "{}", role.display());
    }

    #[test]
    fn ordinary_names_stay_readable() {
        let dir = Path::new("schema");
        assert_eq!(
            path(dir, &ObjectName::new("dbo", "customer"), None).unwrap(),
            dir.join("dbo.customer.yml")
        );
    }

    #[test]
    fn path_components_can_never_escape_the_declaration_directory() {
        let dir = Path::new("stage/schema");
        let name = ObjectName::new("../../existing", "../victim");
        let generated = path(dir, &name, Some(ModuleKind::View)).unwrap();
        assert_eq!(generated.parent(), Some(dir));
        assert_eq!(
            generated.file_name().unwrap().to_string_lossy(),
            "%2E%2E%2F%2E%2E%2Fexisting.%2E%2E%2Fvictim.view.yml"
        );
    }

    #[test]
    fn separators_inside_identifiers_do_not_create_filename_collisions() {
        let dir = Path::new("schema");
        let a = path(dir, &ObjectName::new("a.b", "c"), None).unwrap();
        let b = path(dir, &ObjectName::new("a", "b.c"), None).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn long_identifiers_fit_in_one_filesystem_component() {
        let dir = Path::new("schema");
        let name = ObjectName::new("資料".repeat(128), "表".repeat(128));
        let generated = path(dir, &name, None).unwrap();
        assert!(generated.file_name().unwrap().len() <= 240);
        assert_eq!(generated.parent(), Some(dir));
    }

    #[test]
    fn windows_device_stems_never_lead_a_generated_filename() {
        let dir = Path::new("schema");
        for reserved in ["CON", "con", "PRN", "AUX", "nul", "COM1", "lpt9"] {
            let generated = path(dir, &ObjectName::new(reserved, "customer"), None).unwrap();
            let stem = generated.file_name().unwrap().to_string_lossy().to_string();
            let leading = stem.split('.').next().unwrap().to_string();
            assert!(
                !leading.eq_ignore_ascii_case(reserved),
                "`{reserved}` still leads `{stem}`"
            );
            assert!(leading.starts_with('%'), "{stem}");
        }

        // The escape must not swallow names that merely start with a device
        // name, and must not collide with any other identifier: `%43` can only
        // come from this escape, because `C` is inside `component`'s alphabet.
        assert_eq!(
            path(dir, &ObjectName::new("CONTROL", "customer"), None).unwrap(),
            dir.join("CONTROL.customer.yml")
        );
        assert_eq!(
            path(dir, &ObjectName::new("CON", "customer"), None).unwrap(),
            dir.join("%43ON.customer.yml")
        );
        // Only the leading stem is interpreted by Windows, so an object named
        // after a device stays readable.
        assert_eq!(
            path(dir, &ObjectName::new("dbo", "CON"), None).unwrap(),
            dir.join("dbo.CON.yml")
        );
    }

    #[test]
    fn hashed_names_cannot_collide_with_the_readable_namespace() {
        let long = ObjectName::new("資料".repeat(128), "表".repeat(128));
        let hashed = filename(&long, Some(ModuleKind::View));
        assert!(hashed.starts_with("~pbps-"), "{hashed}");

        // Recreate the old collision shape: a table whose schema and name make
        // the hashed module filename when joined with dots. The leading `~` is
        // percent-encoded on the readable path, so the two stay distinct.
        let stem = hashed
            .strip_prefix('~')
            .unwrap()
            .strip_suffix(".view.yml")
            .unwrap();
        let readable = filename(&ObjectName::new(format!("~{stem}"), "view"), None);
        assert_ne!(hashed, readable);
        assert!(readable.starts_with("%7E"), "{readable}");
    }
}

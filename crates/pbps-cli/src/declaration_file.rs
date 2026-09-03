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

/// The file a role is written to: `<name>.role.yml`, with the same encoding
/// as an object name's components and the same hashed fallback.
///
/// A role has no schema, so the `.role` suffix is what keeps it apart from a
/// table called the same thing — `app_reader.yml` would otherwise be read as
/// a table file whose name has no schema, which the loader refuses.
pub fn role_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let readable = format!("{}.role.yml", escape_reserved_stem(component(name)));
    let file = if readable.len() <= 240 {
        readable
    } else {
        let mut hasher = Sha256::new();
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(b"role");
        format!("~pbps-{:x}.role.yml", hasher.finalize())
    };
    let path = dir.join(&file);
    if path.parent() != Some(dir) {
        bail!("refusing to write `{file}` outside `{}`", dir.display());
    }
    Ok(path)
}

fn filename(name: &ObjectName, kind: Option<ModuleKind>) -> String {
    let kind_suffix = kind.map(|k| format!(".{}", k.as_str())).unwrap_or_default();
    let readable = format!(
        "{}.{}{}.yml",
        escape_reserved_stem(component(&name.schema)),
        component(&name.name),
        kind_suffix
    );
    // SQL Server identifiers can be long Unicode strings. Percent-encoding all
    // their bytes may exceed a filesystem's 255-byte filename limit; in that
    // rare case a full SHA-256 keeps the name deterministic and collision-safe.
    if readable.len() <= 240 {
        return readable;
    }

    let mut hasher = Sha256::new();
    hasher.update(name.schema.as_bytes());
    hasher.update([0]);
    hasher.update(name.name.as_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

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

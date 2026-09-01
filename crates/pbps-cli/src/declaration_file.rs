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

fn filename(name: &ObjectName, kind: Option<ModuleKind>) -> String {
    let kind_suffix = kind.map(|k| format!(".{}", k.as_str())).unwrap_or_default();
    let readable = format!(
        "{}.{}{}.yml",
        component(&name.schema),
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
    format!("pbps-{hash:x}{kind_suffix}.yml", hash = hasher.finalize())
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
}

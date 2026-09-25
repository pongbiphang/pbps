//! `pbps key generate --out` under a hostile umask (DEC-952.1, PR #955).
//!
//! A process-wide umask cannot be set inside a unit test without disturbing its
//! neighbours, so the shipped binary runs in a shell that sets it.

#[cfg(unix)]
#[test]
fn a_key_written_under_a_restrictive_umask_is_still_readable_by_its_owner() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::env::temp_dir().join(format!("pbps-key-umask-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("prod.key");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "umask 0777 && exec \"{}\" key generate --out \"{}\"",
            env!("CARGO_BIN_EXE_pbps"),
            path.display()
        ))
        .status()
        .unwrap();
    assert!(status.success());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(std::fs::read_to_string(&path).unwrap().trim().len() >= 43);
    std::fs::remove_dir_all(&dir).unwrap();
}

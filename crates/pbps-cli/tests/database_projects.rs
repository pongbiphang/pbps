use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pbps-cli-a1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn legacy_commands_refuse_named_projects_before_connections_or_writes() {
    let dir = Dir::new();
    let config = dir.0.join("pbps.yml");
    let text = "dialect: mssql\ndatabases: [alpha, beta]\n";
    std::fs::write(&config, text).unwrap();
    // No declarations, identities or credentials exist: refusal belongs to
    // the command's project contract, before the old single-DB pipeline.
    for args in [vec!["validate"], vec!["plan"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir.0)
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("does not yet support instance-wide"),
            "{args:?}: {message}"
        );
        assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }
    std::fs::write(&config, "dialect: mssql\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&dir.0)
        .arg("validate")
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("does not yet support instance-wide")
    );
}

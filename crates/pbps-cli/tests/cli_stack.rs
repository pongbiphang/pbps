//! Exercise the real startup frames, including command-tree construction.

#[test]
fn commands_start_with_the_windows_main_stack() {
    for (flag, expected) in [("--version", 0), ("--not-a-pbps-option", 2)] {
        #[cfg(unix)]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            // Windows already gives the binary's main thread a 1 MiB stack.
            // Limit the child too on Unix; a small test thread alone omits the
            // real main/run frames and did not reproduce this regression.
            command.args([
                "-c",
                "ulimit -s 1024 && exec \"$@\"",
                "pbps-stack-test",
                env!("CARGO_BIN_EXE_pbps"),
            ]);
            command
        };
        #[cfg(windows)]
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"));
        let output = command.arg(flag).output().unwrap();
        assert_eq!(output.status.code(), Some(expected), "{flag}: {output:?}");
    }
}

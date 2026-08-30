# PongBiphang Schema (`pbps`)

Declarative database schema version control. You maintain one description of what
the schema should look like; the tool works out the rest.

- Design specification: [docs/SPEC.md](docs/SPEC.md)
- Decision records: [docs/ADR-0001-yaml-crate.md](docs/ADR-0001-yaml-crate.md)

**Phase 0 and Phase 1 are complete.** The available commands are `plan` (with
`--check` / `--since` / `--base` / `--out`), `validate`, `fmt` (with `--check`),
`rename`, `rename-table`, `drop` and `drop-table`. SQL emission and database
access arrive in Phase 2.

## Development

Linux is the primary development and release target; Windows coverage comes from
the CI matrix.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

### On Windows, develop inside WSL2

Do not use `x86_64-pc-windows-gnu`. Rustup's self-contained mingw ships no GNU
assembler, so crates that use raw-dylib — `windows-sys` among them — fail to
build, and both `clap` and `miette` pull `windows-sys` in, which means no CLI
with coloured output escapes it. `gnullvm` needs an external llvm-mingw just the
same.

There are two workable paths on Windows:

1. **WSL2** (what this project uses): `sudo apt install build-essential
   pkg-config`, then install stable via rustup. Keep the project inside the WSL
   filesystem (`~/pbps`) rather than under `/mnt/c` — that is a 9p filesystem,
   and cargo's many small reads and writes get noticeably slower on it.
2. **MSVC**: install the VC++ workload of the Visual Studio Build Tools.

### Line endings

Everything in version control is LF (see `.gitattributes`). This tool writes
files itself (`pbps fmt`), and letting the platform decide line endings would
break the guarantee that the tool owns the file format on Windows, as well as
manufacturing phantom git diffs.

## Licence

MIT OR Apache-2.0

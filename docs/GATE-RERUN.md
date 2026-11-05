# Completing an independent gate failure

DEC-2651.1 permits one targeted attempt for an unregistered failure. Preserve
the failed output. Fetch `origin/master`, identify the defining test source,
and review every module and fixture the check uses. If the PR diff touches any
of them, fix the failure and run a new full gate instead.

Record isolated passes with the driver from the PR, pointing it at clean
worktrees for `origin/master` and the PR head. Use the same environment as the
gate. Run cargo/test commands through the project's memory guard. These proofs
execute only the named test, or the whole named Python check, and record the
commit, tree, input digest and validated result. Keep outputs outside both
worktrees. For example:

```sh
python3 /path/to/pr/scripts/live-shards.py proof --worktree /path/to/master \
  --output /evidence/master.json --name module::property \
  test -p pbps-cli --test flow -- --ignored
python3 /path/to/pr/scripts/live-shards.py proof --worktree /path/to/pr \
  --output /evidence/head.json --name module::property \
  test -p pbps-cli --test flow -- --ignored
```

For a Python check replace `--name ... test ...` with
`--python scripts/live_test_runners_test.py`. Python checks are always whole
scripts. To record a resumable non-live Python gate, use
`python3 scripts/live-shards.py --suite python --output /gate/python`; this
derives its checks from CI's lint/quick jobs. A local full-gate wrapper can use
this command in place of its Python loop. It does not run the other gate steps.

Open or join an issue for every failed name. Record the proofs and complete
scope review in the PR, then write a JSON evidence file:

```json
{
  "version": 1,
  "pr": "https://github.com/pongbiphang/pbps/pull/123#issuecomment-456",
  "names": [{
    "cargo": ["test", "-p", "pbps-cli", "--test", "flow", "--"],
    "test": "module::property",
    "source": "crates/pbps-cli/tests/support/module.rs",
    "paths": ["crates/pbps-cli/tests/support/module.rs", "crates/pbps-cli/tests/fixtures/example"],
    "scope_review": "The defining module and all external helper modules and fixtures were reviewed and listed.",
    "issue": 789,
    "master_pass": "master.json",
    "head_pass": "head.json"
  }]
}
```

Proof paths are relative to the evidence file or absolute. `paths` contains
tracked repository paths, including the defining file and every external
module/fixture it uses; directory paths cover their descendants. Scope paths must be literal,
repo-relative POSIX spellings from the tracked tree: no root `.`, dot or parent
components, absolute paths, empty strings, trailing `/` or glob characters.
Duplicate JSON fields are refused. The driver
also protects a Rust file's same-named module directory (`mod.rs` protects its
parent directory) and shared gate/build inputs. It checks the defining Rust
function exists. Macro-generated names with no verifiable defining function
are conservatively refused. The operator remains responsible for reviewing
the complete external dependency scope; a file list alone cannot establish
independence.

For Python use `"cargo": ["python", "scripts/check.py"]`,
`"test": "scripts/check.py"`, and `"source": "scripts/check.py"`.
List its implementation modules, child runners and fixtures in `paths` too.

```sh
python3 scripts/live-shards.py --suite pg --resume /gate/failed \
  --rerun-unregistered /evidence/names.json --output /gate/completed
```

Use `--suite python` for Python receipts. The suite selection must match the
failed output. Evidence must name exactly the unregistered failed checks;
registered failures use the existing register path. Successful names are
carried and missing obligations still run. Retain both summaries, the head
tree, evidence and attempt history in the PR. A failed or interrupted targeted
attempt blocks another attempt, including a resume of the original output.
The allowance is stored under the common Git directory's
`pbps-targeted-reruns/<tree>/`; preserve it across worktrees and gate attempts.
Follow up with an ordinary register PR so recurring failures stay visible.
Python register entries use the same whole-check `cargo`/`test` selector as
the evidence file, with the ordinary issue and expiry fields.

Resume inputs also pin Python startup flags, the executable/search path,
Cargo/Rust/Python/Node and Git configuration environment, locale, and the UTC
expiry day. Build jobs and the target directory are scheduling/storage settings. Keep that context identical for isolated proofs
and resumption; a different expiry day requires fresh evidence.

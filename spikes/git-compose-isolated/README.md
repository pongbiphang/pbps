# Isolated compose feasibility measurements

This experiment supports ADR-0017 and DECISIONS 527 (#744). It uses real Git
and the ordinary CLI in disposable repositories with a local bare remote.
It is a design probe, not a production compose, browser or recovery test.

Run on Linux with Python 3.9+ and a CLI built from this checkout:

```sh
cargo build -p pbps-cli
python3 spikes/git-compose-isolated/measure.py --pbps target/debug/pbps
python3 spikes/git-compose-isolated/measure.py --pbps target/debug/pbps --revert-ref-type-guard
python3 spikes/git-compose-isolated/measure.py --pbps target/debug/pbps --revert-candidate-freeze
python3 spikes/git-compose-isolated/measure.py --pbps target/debug/pbps --revert-forced-text
```

When using `CARGO_TARGET_DIR`, pass the resulting binary's path instead.
The first run must pass. The second restores expected-zero single-shot ref
creation and must fail dangling-symbolic-ref and unborn-checked-out-branch
preservation. The third restores rebuilding from live source bytes after
preview and must fail the reviewed-tree assertion. The failures are intentional
controls, not production test failures. `observed.txt` retains the actual runs;
rerunning never overwrites it.
The fourth run restores Git's attribute-selected binary diff and must fail the
reviewed-lines assertion for declarations and ids marked `-diff` (#745).

| Question | Boundary exercised |
| --- | --- |
| Is the reviewed output separate from source work? | Raw base blobs plus the captured declaration, real CLI rename/validate, private Git index, commit tree and parent; source HEAD/ref/index/files and modes remain unchanged. |
| Does unrelated staged/untracked work enter the commit? | Only declaration and ids paths change from the selected base; an unrelated staged edit stays excluded. |
| What if the user edits after preview? | The same captured tree and diff are committed. The later source edit remains in the original checkout. |
| Can Git attributes hide changed lines? | Forced-text diff preserves declaration and ids lines even with `-diff`; the reverted control displays only a binary summary and fails. |
| Is expected-absent ref creation sufficient? | Direct collisions refuse; a prepared transaction plus live type/checked-out checks preserves a dangling symbolic ref and an unborn checked-out destination. The reverted control loses both protections. |
| Does a narrow refspec exclude unreviewed ancestry? | No. Pushing a child also transfers its unpublished parent. Requiring a remotely advertised base is a separate guard. |
| What if signing fails? | A failing local signing helper causes explicit `commit-tree -S` to fail; no output ref exists and source state is unchanged. |
| What if push fails? | The local result remains; retry sends the exact same commit. |
| What if acknowledgment is lost? | A wrapper lets real Git push, discards the success and exits unsuccessfully; querying the remote identifies the same commit without creating another. This is caller-evidence loss, not a real network interruption. |
| How does editing continue? | An explicitly created separate worktree of the result validates with its reviewed declarations and renamed ids, while the source remains unchanged. |

The probe excludes global/system Git configuration and inherited `GIT_*`
overrides, disables hooks/fsmonitor, and contacts no network. It uses a fixed
operation branch only inside its own temporary repository. It removes only
its disposable temporary directory. It does not modify the repository that
contains this script or the supplied CLI executable.

The fixture intentionally contains only known regular files. It does not
implement production path containment, stable input enumeration, complete
manifests, guarded transport/signing policy, browser generation checks,
persistent records, lock retirement or process/power-loss recovery. A normal
temporary-directory cleanup here is not evidence for any of those properties.
Signing success, a remote server, hostile filesystem races, other Git backends,
Windows and macOS remain unqualified. #745–#748 carry those implementation and
qualification obligations; the read-only UI is not enabled for writes by this
experiment.

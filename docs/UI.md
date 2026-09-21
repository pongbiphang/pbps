# The local UI

Run `pbps ui` in a project, or `pbps --project path/to/project ui`, then open the
complete URL printed in the terminal. Keep that process running while browsing;
Ctrl-C stops it. Each launch uses a new port and a new token in the URL fragment.

The page provides five views:

| View | CLI read |
|---|---|
| Environments | `status --format json` |
| Drift | `verify --env <name> --format json` |
| Saved plan | `explain --plan <path> --format json` |
| Timeline | `state list --env <name> --format json` |
| Schema & ERD | `docs --format html` |

Environment names come from `pbps.yml`. Set their `url_env` variables in the
terminal before starting the viewer, exactly as for the CLI. The page accepts
an environment name; it has no connection-string field. Saved plan paths refer
to files on the machine running pbps, relative to the project or absolute.

Refresh runs the selected command again. Reports and their findings are shown
together: an unreachable environment is not an empty history, and an unread
lock is not a free lock. The timeline shows the CLI's requested limit and
preserves unreadable entries with their diagnoses. The ERD is the Mermaid
source already produced by `docs`, beside its tables and documentation, in a
script-free sandboxed frame. No assets are downloaded.

The viewer invokes ordinary CLI reads with `--no-input`, including the
project's existing `on_drift` hook when `verify` finds drift. It adds no polling
or scheduler. It never runs `plan` or `apply`. An explanation can show an
approval command as text; it cannot execute it. Approval and deployment stay in
the ordinary CLI/CI workflow.

## Composing intent

The page records a rename, a drop or a role change the way you would in a
shell, and in the same order. **Edit the declaration in your editor first**: an
intent command does not write a declaration, it resolves the edited ones
against the ids file and rewrites the ids file alone. Then pick the kind of
change, type its arguments, and read the diff.

What you send is the *intent* — the kind of change and its arguments, one of
six commands — never a file and never a command name. The UI writes the
recorded tip out to a private directory, lays your working tree's declarations
over it, runs the CLI's own intent command there, and builds the commit from
what that produced. `pbps validate` is run on the tree the commit will hold,
not on the checkout, so an editor saving mid-compose cannot pass a check the
commit then fails.

The diff you are shown is a preview that has already undone itself: nothing is
placed, no lock is held while you read it, and pressing **Commit and push**
runs the whole protocol again from the beginning — so a file saved between the
two is caught rather than overwritten.

**Your git hooks do not run** for a commit or a push made here, and the page
says so beside the commit. The commit is built with plumbing under the index
lock and the branch moves through a prepared `update-ref` transaction; a hook
that can publish would make every check the UI runs afterwards a check too
late. This tool keeps policy in files and in CI (SPEC §14.3, ADR-0008), and a
rule that must hold for every commit belongs where a rebase cannot skip it
either.

The compose refuses rather than guesses. An uncommitted `pbps.yml`, a
declaration `git` has been told to leave alone, an ignored declaration, a
symbolic-linked declaration, a project whose inputs lie outside its directory,
a staged change of your own on a path it would commit, a branch that is behind
its remote, a remote with more than one push URL, and a `HEAD` that points at a
chain of symbolic refs are each refused with the thing named, and your checkout
is left as it was.

If a compose is interrupted — the machine stops, the process is killed — it
leaves a record under `<git-dir>/pbps-ui/composing/`. The UI reads it the next
time it starts and either finishes what was interrupted or rolls it back, and
says which; it never decides by looking at the refs alone. A file it displaced
is kept under `<git-dir>/pbps-ui/previous/` and is never deleted, because an
editor may still hold it open.

Composing runs on Linux only for now. On macOS and Windows the page says so and
shows the commands to run by hand, which is also what a machine without `git`
gets (DECISIONS 523).

If the page says the launch token is missing, reopen the complete printed URL.
A different Host, port, Origin or token is refused. An incompatible CLI response
is shown as a read failure, with no partial report. Stop and restart the viewer
after replacing the pbps executable.

The server and page are designed for one user on loopback. There is no `--host`
option. [ADR-0015](ADR-0015-local-ui-implementation.md) records the process,
credential and browser boundaries; [DECISIONS 457](DECISIONS.md) records the HTTP
dependency and the initial size measurement; DECISIONS 523–526 record the
composing step.

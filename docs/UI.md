# Local read views

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
or scheduler. It never runs `plan` or `apply`. Its only writes are the compose
workflow below, which records intent as a reviewed Git commit on a new branch.
An explanation can show an approval command as text; it cannot execute it.
Approval and deployment stay in the ordinary CLI/CI workflow.

If the page says the launch token is missing, reopen the complete printed URL.
A different Host, port, Origin or token is refused. An incompatible CLI response
is shown as a read failure, with no partial report. Stop and restart the viewer
after replacing the pbps executable.

The server and page are designed for one user on loopback. There is no `--host`
option. [ADR-0015](ADR-0015-local-ui-implementation.md) records the process,
credential and browser boundaries; [DECISIONS 457](decisions/compose-and-ui.md#decision-457) records the HTTP
dependency and the initial size measurement.


## Compose workflow

**Compose change** records a rename, a drop reason, or `strategy:` annotations
already edited in the declarations. It follows
[ADR-0017](ADR-0017-isolated-compose.md) (#494):

1. **Preview / Refresh** captures the edited declarations and resolves the
   intent with the ordinary CLI in a private snapshot. It then shows the exact
   diff, parent, tree, destination and signing policy. Later editor changes are
   excluded until you refresh.
2. **Confirm this candidate** commits exactly that reviewed tree on a new
   branch `pbps-compose/<operation>` and pushes it to the reviewed destination.
   The current checkout keeps its declaration edits, ids, staged work and
   branch. Nothing is reset or copied back.
3. The result shows the commit, the local branch and the remote outcome. A
   failed or uncertain push keeps the local commit. **Reconcile** re-reads the
   saved result. **Republish** explicitly authorizes recreating an absent
   branch with the same commit, after you have diagnosed the remote.
4. A delivered result links to a new merge request when the destination is
   `github.com` or `gitlab.com` over HTTPS or SSH without a custom port.
   Elsewhere the page names the branch to open a request for. Continue from the
   result in a separate checkout; the page prints the `git worktree add` and
   `pbps ui` commands.

5. **Clean up private resources** retires a delivered operation's snapshot
   and base root. The exact-commit root stays for its receipt. **Forget this
   receipt** asks for a second confirmation, then retires the receipt and that
   root; pushed branches are never cleanup targets. **Find private compose
   resources** lists every operation's private evidence and state. It can
   retire what an operation still owns, such as an expired preview left by a
   closed viewer. An unresolved, foreign or unreadable record is refused and
   preserved (#855).

The page sends only the intent fields and opaque handles. Every tree, manifest,
destination and commit stays on the server side. Compose actions are fixed
`POST /api/compose/<action>` requests. They require the launch token, the
page's own Origin and a JSON body of at most 64 KiB; every other route stays
read-only. HTTP and SSH destinations must use ordinary branches, as stated
before confirmation ([DECISIONS 534](decisions/compose-and-ui.md#decision-534)).

Compose is qualified on Linux with Git's files ref backend. On other platforms
the viewer refuses compose actions, so use the CLI there (#471).

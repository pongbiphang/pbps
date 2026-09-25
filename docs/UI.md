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
project's existing `on_drift` hook when `verify` finds drift. It adds no
scheduler. Its writes are the compose workflow below, which records intent as a
reviewed Git commit on a new branch, and the plan and apply workflow after it,
which runs the same two commands a terminal would. Approval stays where the
organization keeps it: the viewer runs `apply` only with the checksum a person
types, and stores no approval.

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
   The request targets the reviewed base branch, not the host's default
   branch (#851). Elsewhere the page names the branch to open a request for. Continue from the
   result in a separate checkout; the page prints the `git worktree add` and
   `pbps ui` commands.

5. **Clean up private resources** retires a delivered operation's snapshot
   and base root. The exact-commit root stays for its receipt. **Forget this
   receipt** asks for a second confirmation, then retires the receipt and that
   root; pushed branches are never cleanup targets. **Find private compose
   resources** lists every operation's private evidence and state. It can
   retire what an operation still owns, such as an expired preview left by a
   closed viewer. An unresolved, foreign or unreadable record is refused and
   preserved (#855). If another viewer retires this workflow's own preview,
   the workflow is released the next time it is listed, refreshed or
   confirmed. Confirming it is refused and nothing is published (#867).

The saved receipts, not the page, decide whether a new preview may start
(#854), so a restarted viewer keeps the rules. The page lists them when it
opens. A preview from a base that already has an unresolved result is
refused until that result is reconciled or retried. A base that already has
a delivered result needs **Start an alternative** first. Receipts from other
bases never block, so a result that cannot resolve does not close compose.
Confirmation checks the same rules again under the publisher's lock. So if
another viewer publishes from that base after this preview was shown,
confirming it is refused, nothing is published, and the preview is retired
(#874). The page shows the reason and lists the saved results, so the result
from that base and its **Start an alternative** are in view.

The page sends only the intent fields and opaque handles. Every tree, manifest,
destination and commit stays on the server side. Compose actions are fixed
`POST /api/compose/<action>` requests. They require the launch token, the
page's own Origin and a JSON body of at most 64 KiB; every other route stays
read-only. HTTP and SSH destinations must use ordinary branches, as stated
before confirmation ([DECISIONS 534](decisions/compose-and-ui.md#decision-534)).

Compose is qualified on Linux with Git's files ref backend. On other platforms
the viewer refuses compose actions, so use the CLI there (#471).

## Plan and apply

**Plan & apply** runs the two deployment commands of SPEC §7.3 (#1025, #64
step 5). Each is one `pbps` child with `--no-input`, started from typed fields:

| Form | Command |
|---|---|
| Write plan | `plan --env=<name> --out=<new file>` |
| Apply plan | `apply --env=<name> --plan=<file> --checksum=<typed> [--allow=<classes>] [--staged] [--resume]` |

- **The environment is a name.** The child reads its connection string from
  the `url_env` variable, as the CLI does. No field holds a connection string.
- **The plan file must be new.** The viewer refuses a path that already names
  anything, so a plan someone approved is never replaced by a new one
  (DEC-1025.2). Read the new plan with **Read the plan**, which opens the Saved
  plan view on it.
- **The checksum is typed, never filled in.** The field starts empty, and
  nothing the viewer reads fills it. Enter the SHA-256 your deployment gate
  approved; `apply` refuses a plan whose checksum does not match. Allowed risk
  classes are typed the same way, and an empty field allows none (DEC-1025.1).
- **A run outlives the page.** Closing the tab does not stop a plan or apply.
  The page shows each environment's latest run, and while one is running it
  asks for the outcome every two seconds. A second run against the same
  environment is refused until the first ends; the ledger's lock still decides
  between runs from different places (DEC-1025.3).
- **The outcome is the CLI's own.** The page shows the exit code and what the
  command printed, as the command printed it. For the recorded result, open the
  environment's **Timeline**.
- **Ctrl-C reaches the run.** Stopping the viewer from its terminal also
  interrupts a plan or apply it started, exactly as Ctrl-C would interrupt
  `pbps apply` run there. A staged apply that stopped part-way continues with
  `--staged --resume`.

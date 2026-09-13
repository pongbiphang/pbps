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
or scheduler. It never runs `plan`, `apply`, intent commands, file edits or git
writes. An explanation can show an approval command as text; it cannot execute
it. Approval and deployment stay in the ordinary CLI/CI workflow.

If the page says the launch token is missing, reopen the complete printed URL.
A different Host, port, Origin or token is refused. An incompatible CLI response
is shown as a read failure, with no partial report. Stop and restart the viewer
after replacing the pbps executable.

The server and page are designed for one user on loopback. There is no `--host`
option. [ADR-0015](ADR-0015-local-ui-implementation.md) records the process,
credential and browser boundaries; [DECISIONS 457](DECISIONS.md) records the HTTP
dependency and the initial size measurement.

# Namespace-scoped process observation

Issue #740 provides the observation primitive for the runtime redesign tracked
by #737 and #740–#743. Issue #741 uses it for whole-container privilege checks,
supplied-server engine discovery and pre-engine membership. Issue #742 enforces
the separately documented launch/workload privilege boundary. Issue #743 uses
the view for socket-holder observations, separately qualifying the native
service/backend relation and the verified connection.

## Contract

`NamespaceProcfs` borrows a qualified `ProcessLease`, opens `/proc` through
that process's held root, and retains the procfs directory and PID namespace
handles. It verifies the filesystem type, the namespace of that view's PID 1,
and the exact mount recorded in the anchor's mount table. Hidden-process and
unknown mount options are refused. Checking only the filesystem type would
also admit a host procfs or a substituted process directory.

Every ordinary path below the held procfs directory uses `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`. Bind mounts from the
same procfs are mount crossings too. The final `1/ns/pid` kernel link is opened
explicitly after qualifying its containing directory and link, then compared
with the anchor's namespace handle.

Task numbers belong to this procfs view. `NamespaceTaskId` pairs the number
with the namespace identity; the observation retains the namespace handle.
A number from this view must never be passed to host `/proc`. Group identity
is represented by its leader's coordinate, separately from the observed task's
coordinate. A coordinate is not a live identity: each `TaskObservation` holds
the opened task directory and start time and brackets status reads through
that directory. Directory identity comparisons apply within the same procfs
instance, not across arbitrary views of one process.

Production consumers capture a `ProcessLease` through each held task entry.
They consume entries as they are enumerated, so descriptor use does not grow
with the container's task count. The optional `observe` collector retains its
returned entries and requires enough caller descriptor capacity; it is not the
source for production qualification. Engine discovery retains at most the two
candidates needed to distinguish a unique engine from an ambiguous selection.
The lease records whether it came from the observer or a retained namespace
view; only the former can supply an observer PID. Parent executable lookups
use that same view. Cross-view identity compares the innermost task number,
start time and retained namespace identities, bracketed by live checks of both
leases. A departed held task cannot be replaced by a same-tick PID reuse.
The fixed lifetime guard's privilege exception uses this identity, never a
bare PID comparison. Other threads of its group do not inherit the exception.

Engine discovery counts group leaders, whereas credentials are checked for
every task, including workers with a different UID from their leader. The
container consumers refuse visible child PID namespaces whose identity differs
from the admitted namespace. Supplied-server cgroup subtree limits and foreign
network/mount/IPC sharer accounting remain independent checks; the latter still
needs the observer's wider view. Matching credentials does not establish engine
origin, and neither the census nor an exception claims to contain a provisioning
administrator (DECISIONS 529).

The observation enumerates the namespace view and each visible group's task
directory. It does not follow parent-child edges or enumerate unrelated host
processes when the selected namespace is private. Child PID namespaces visible
from this view are included: consumers requiring exact namespace equality must
check that separate relation. Likewise, PID membership does not establish
cgroup membership, engine ancestry, or who provisioned a process.

An entry absent when opened may have exited since enumeration. A held task is
discarded only on a proven exit through its held inode. Permission failures and
malformed metadata are errors. A dead leader is not a dead group: if live
threads remain, their task entries are inspected, or an unreadable group is
refused. An empty observation with a live anchor is refused. Replaced views,
unsupported/hidden views, unreadable metadata and lost anchors have distinct
error classes; this does not widen the older native diagnostic API (#646).

This is a sequence of observations, **not an atomic snapshot or continuous
containment**. A task can appear after the directory position it would occupy
has passed. A caller cannot derive a permanent exact membership count from a
successful scan. Privilege and resource restrictions between reads must be
enforced by the verified launch boundary, not inferred from polling.

## Measurements before implementation

Measurements used WSL2 Linux `6.6.87.2`, rootful Podman `4.9.3`, and a separate
rootful Docker `29.8.1` daemon on the same Ubuntu instance. The latter had its
own socket and storage and did not change the existing runtime configuration.
These are component measurements, not a claim that a Podman-native peer is
accepted by the Docker daemon authenticator (#686), nor full resolver admission.

| Case | Result |
| --- | --- |
| Container `/proc` from a root observer | PID 1 corresponds to the selected namespace; the host PID and proc directory device/inode are different |
| Runtime `exec` sleeper | Visible in the namespace with PPid 0, absent from init's `children` |
| Parent exits leaving a live grandchild | The grandchild remains visible in the namespace |
| Two private namespaces each containing PID 1 | Same local number, different namespace identities |
| Held process directory after container removal | Reading `stat` returns ESRCH; no numeric-path recapture is needed |
| Forced PID reuse in a private test namespace | The old held task still reports exit and does not compare equal to the new task at the same namespace-local number |
| Leader calls `pthread_exit`, worker survives | On this kernel the task directory still lists both threads, while the leader's fd directory is empty; the worker must be observed separately |
| Another process directory bound over a numeric proc entry | `openat2` with `NO_XDEV` refuses with EXDEV, even though both entries are procfs |

The synchronized counterexample posted on #730 used the enumeration decision
logic extracted from PR #735 head `a79a3046450929159d886a9ad1a947731cc79a34`,
with minimal support types and a scheduling hook. A parent exits after its
matching PPid was accepted and before its own children are read. Its zombie
entry remains readable with an empty children list, and its living grandchild
has moved to the already visited subreaper. All ten runs returned an accepted
list omitting that grandchild. Moving the exit before enumeration included the
grandchild in all ten controls. This was an enumeration experiment, not a full
CLI or engine test.

A second component experiment gave a persistent sibling and that grandchild
the same socket. The extracted old walk found only the sibling in all ten
runs; a flat namespace enumeration found both in all ten. This demonstrates
the two-holders-to-one omission and a way to avoid that particular omission.
It does not prove uniqueness against arbitrary concurrent descriptor transfers
or supply the final target-connection contract.

## Launch and target implications for the dependent stages

Both engine bootstrap premises were tested before adding the primitive:

| Engine / recipe | Measured result |
| --- | --- |
| PostgreSQL 18, existing root bootstrap with all capabilities removed | Fails creating the owned data directory |
| PostgreSQL 18, root bootstrap with CHOWN, DAC_OVERRIDE, FOWNER, SETUID, SETGID and SETPCAP | Initializes, drops to uid/gid 999 with all capability sets zero, executes create/insert/select |
| PostgreSQL 18, runtime-created storage owned by 999, direct uid/gid 999, no capabilities | Initializes and executes the same SQL; no-new-privileges and zero capability sets remain in effect |
| SQL Server 2025, direct uid 10001, no capabilities | The engine executable is refused with EPERM |
| SQL Server 2025, direct uid 10001 and NET_BIND_SERVICE bounding ceiling | Starts and answers SQL; observed live tasks meet the uid, no-new-privileges and capability ceiling |

The measured image IDs were PostgreSQL
`a6638641707cdf047e5d5c2781f437e2e809323cab22c70b280be8389fbb7878` and SQL Server
`03f9c5d3599eb86eba05fa68becaed8d86b1048600ce4960d4782e383eaf66c3`.
Podman 4.9.3 rejected the documented Docker tmpfs `uid=999` option before
startup. Therefore the engine recipe comparisons above were run on Docker;
the Podman namespace measurements do not certify that recipe's portability.

Stage #741 preserves independent cgroup and foreign network/mount/IPC
accounting. Stage #742 checks the actual post-drop workload separately from
the privileged deadline guard and requires that guard's effective termination
authority. The updated Docker supplied-PostgreSQL recipe starts directly under
uid/gid 999 with no capabilities. A restructured ownership-only bootstrap for
the Podman component needs CHOWN/SETUID/SETGID/SETPCAP, dropping before initdb;
the earlier six-capability measurement above applies to the old root shell
that also prepared the password and data directory. No repeated census was
removed: inheritance bounds dropped descendants, not arbitrary administrator
runtime-exec entrants. See [the launch contract](RESOLVER-RUNTIME.md#startup-and-continuity).

For #743, the native target uses its selected service's procfs view, which
can be host-wide. Matching socket holders are grouped by TGID after inspecting
each task's descriptor table. Incidental non-holders need no executable lease,
so an unrelated user program or kernel thread is not qualified as an engine.
Required unreadable descriptor tables still refuse. Native connection binding
then requires a positive relation between the observed backend and selected
service, rather than accepting another service in the same namespace.

The complete preimplementation report on #743 demonstrates a stable PID list
and held identities with two holders at every instant, yet an observed count
of one. Four repeated scans can all produce that count. The approved contract
therefore trusts environment provisioning against deliberate FD handoff; it
preserves observation of stable/reparented extra holders and all independent
identity and containment checks, without promising exhaustive membership.
An empty result gets only one fresh pass for late visibility, never retries of
ambiguity or unreadable evidence. See [the target contract](RESOLVER-RUNTIME.md#target-identity-and-socket-observation-743)
and DECISIONS 533. The acknowledged kernel fixtures run via
`scripts/live-resolver-sockets.py` in addition to the observer fixture below.

## Repeatable fixture

Build the CLI test binary, then run the fixture as root on a disposable native
Linux host with the pinned PostgreSQL image already loaded:

```sh
sudo python3 scripts/live-resolver-namespace.py --test-binary /absolute/path/to/pbps_cli-test-binary
```

An explicit `--docker-socket` and `--runtime-binary` select a separate Docker
daemon. `--runtime podman` exercises the component against rootful Podman.
`--image` permits a preloaded local reference; the fixture prints its actual
image ID. The fixture mounts only its own native measurement helper read-only;
it tests process observation, not admission of this helper-bearing container as
a scratch engine. Mount substitution runs in a fresh mount/PID namespace.
Created containers are uniquely named and removed on exit. CI runs the Docker
fixture alongside the existing resolver tests.

Regression sensitivity was checked by temporarily restoring a children-based
membership source, permitting process-entry mount crossings, and ignoring a
changed procfs mount ID. Each mutation failed its corresponding fixture
assertion; restoring the guards passed again.

Kernel references: [PID namespaces](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html),
[thread directories](https://man7.org/linux/man-pages/man5/proc_pid_task.5.html),
[children enumeration](https://man7.org/linux/man-pages/man5/proc_tid_children.5.html),
[openat2 resolution](https://man7.org/linux/man-pages/man2/openat2.2.html), and
[cgroup v2 membership](https://docs.kernel.org/admin-guide/cgroup-v2.html).

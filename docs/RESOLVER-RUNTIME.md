# Resolver runtime boundary

This describes the internal runtime layer for #608 and #609. Named selection is exposed
by the CLI (#606); binding planning is still gated on the later #595 steps.
Acquisition and a live candidate are not environment compatibility, deployment
authorization, retained-source permission or sealed binding evidence.

The staged replacement for descendant-based process observation is documented
in [namespace-scoped observation](RESOLVER-NAMESPACE.md). Its primitive and
fixtures are available under #740. Whole-container task qualification and
engine discovery use that view under #741. The launch boundary is implemented
under #742; #743 observes socket holders through the same namespace view and
separately binds the selected target service to its connected backend.

## Supported initial profile

`linux-amd64-v1` uses a direct local Docker Unix API and a native Linux target
observer on the same kernel. The API's actual Unix peer must be a live,
root-installed Docker daemon, reached through a protected root-owned socket
path. A root-owned proxy is insufficient. The target adapter accepts direct
loopback verified TLS whose actual server socket belongs to the selected
engine service or an observed descendant. A configured PID selects that
service and its qualified procfs view; it does not assert identity. A shared
host PID namespace entails a host-wide observation, including unrelated
process descriptor tables; a private namespace keeps this view small. Database names, credentials and aliases do not establish
instance separation.

Systemd socket activation (`dockerd -H fd://`) is supported with its protected
`/run/docker.pid`. Listener credentials name systemd in this case, so that PID
file supplies only a candidate: kernel Unix socket diagnostics must bind the
actual accepted peer inode to a descriptor held by the root-installed dockerd
process. The client socket's kernel cookie, the peer descriptor and process
lease are checked throughout ordinary API traffic and attach streams. Passing
a protected PID file or plausible Docker replies through a proxy cannot qualify
its socket. Missing or unreadable socket diagnostics refuse admission.

The initial fixed layouts are the tested PostgreSQL 18 and Linux SQL Server
images used by the fixture scripts. The selected image must be explicitly
trusted. Missing images obey `never`/`if_missing`; registry credentials stay
with Docker's configured credential helpers. Acquisition records Docker's
immutable content ID, registry digests and platform separately. It never treats
an image digest as proof of running mounts or loaded native code.

Unsupported or unreadable process, channel, platform and runtime premises
refuse qualification. Remote targets, arbitrary proxies, emulated runtimes,
rootless Docker and other layouts need their own measured profiles. The
observer needs permission to read the selected live processes' proc metadata,
executable and namespace handles, sockets, mounts and cgroup-v2 limits. This
is a read-only runtime observer, not a production agent or a target-side SQL
write probe. Native target verification does not invoke Docker; the factory's
Docker dependency belongs to scratch provisioning.

Contained file lookup retries only the kernel's transient `EAGAIN` resolution
race, for at most eight attempts with the same held root, path and confinement
flags. This also covers the contained parent of a namespace magic link.
Exhaustion and other errors remain refusals; there is no less confined retry.

The fixed workload and forwarder request a soft and hard `RLIMIT_NOFILE` of
1024. Admission and rechecks read effective limits through the held guard and
existing waiter/task observations: both limits must be finite, ordered and no
higher than 1024. A lower ceiling is supported; missing, malformed or unreadable
limits refuse. Checking only the guard would miss an already-created child
whose hard limit remained higher when its parent's was lowered. These checks
retain the process-continuity and observation limits below; they do not establish
a complete lifetime process census.
The shared guard check also covers the control and analysis forwarders of a
supplied server, whose root guards do not have a Docker-factory execution lease.
The supplied engine itself retains its separate operator resource contract.

The Docker workload and control execution leases verify every requested
optional `/proc` mask through the held process root. Only a leaf's actual
`ENOENT` beneath the identified procfs parent counts as absence. Existing
objects must match their visible mount IDs: a directory is an empty read-only
tmpfs, and a file is the same null device bound from the run's `/dev/null`.
Missing masks, different objects, writable or nonempty directories, symlinks
and unreadable state refuse. Kernels without an optional interface remain
supported. Docker's `MaskedPaths` report alone cannot establish these facts.

The three runtime files `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`
must be empty or contain the complete fixed bytes of the measured profile
(#630). The measured Docker API with `NetworkDisabled=true` leaves three
empty files; CLI `--network none` alone produces the generated layout below.
The workload requests hostname `pbps-resolver`, DNS `127.0.0.1`, search `.` and
option `ndots:0`. Docker then produces a fixed hostname, its standard loopback
hosts file and resolver contents with fixed generated comments. Every byte is
qualified: unknown layouts, extra search domains, addresses or comments refuse.
The generated resolver file may name either `/etc/resolv.conf` or
`/run/systemd/resolve/resolv.conf` in Docker's fixed source-path comment. Both
complete layouts are measured; arbitrary paths and additional comments still
refuse. The latter is selected when the host uses the systemd-resolved stub.
A file must be a readable regular file on an actual read-only mount; missing,
oversized, symlinked and unreadable files are not empty answers. A nonempty
truncated file does not match the complete generated layout.
The verifier resolves each file below the held process root.

Docker's `container:` network mode reuses the workload's generated file paths
and forbids separate forwarder DNS/hostname overrides. With no generated paths,
the empty image files remain in each separate mount view. Qualification checks
both actual views. The source-free factory gate checks before
engine initialization; execution and guard checks retain the requirement during
the run. Supplied-server admission and its control/analysis forwarders use the
same content requirement. Operators must use the complete documented fixture
recipe; an arbitrary host-derived file is not qualified by being read-only.
Image acquisition remains a separate explicitly trusted phase, and the fixed
private connection needs no external name resolution or workload egress.

The workload and each forwarder also qualify the actual kernel UTS hostname
and NIS domain name (#804, DEC-804.1). Empty files and empty Docker
`Config.Domainname` do not prove empty kernel values. The hostname must be
exactly `pbps-resolver`; the complete NIS value must be empty, `(none)` or
`localdomain`. These fixed generic literals carry no operator-specific input.
Each process lease retains its UTS namespace handle and identity; existing task
observations require membership in that runtime's qualified UTS view.

The observer enters only the held UTS namespace on a fresh short-lived thread,
reads `uname`, and joins the thread before returning. It requires permission to
join that namespace; unavailable evidence refuses qualification. The ordinary
observer thread never changes namespace, and no target state is written.
Reading `/proc/PID/root/proc/sys/kernel/hostname` from outside that namespace
would instead return the observer's name, even through a pre-opened procfs file.
The same check runs before bootstrap release, supplied admission and continued
analysis, independently for the workload and both supplied forwarders. Replacing
a UTS namespace or changing its names invalidates retained qualification;
restoring a name cannot revive a discarded analysis. Trusted provisioning still
owns excluding deliberate changes restored between observations.

The host kernel, its administrators, the selected daemon and explicitly trusted
image installation form the provisioning trust boundary. SQL privileges in
scratch grant no authority over those external controls. This profile does not
claim protection from a compromised kernel or provisioning administrator.

## Target identity and socket observation (#743)

`observed_socket_holders` visits the pinned namespace procfs and each visible
thread group's tasks, independently of parent-child lists. It reads descriptor
tables through held task entries and captures protected executable/process
leases only for matching groups. Threads sharing a table count once, while a
worker with an unshared table is still inspected. Group order is stable within
the retained namespace view. More than three observed groups refuses without
retaining an unbounded set of leases; the fixed forwarder permits three and an
engine backend permits one.

The native adapter separately follows the observed holder's parent chain to
the selected service, with held identities and bracketed parent reads. Merely
sharing a namespace or the engine executable name does not establish that
relation. The authenticated connection identity, actual endpoint pair/socket
inode, retained service/backend processes and engine-owned identity query are
checked together. A new connection cannot reuse a lease; exit, exec replacement,
changed endpoints, canceled checks and dropped target witnesses invalidate it.

An empty observation receives one fresh pass for a backend that became visible
after the first PID enumeration. A permission error, replaced view or observed
ambiguity is never retried as absence. This bounded readiness handling does
not prove completeness: acknowledged fixtures keep two holders alive throughout
a scan while it observes one, and repeat that result across four passes with
unchanged process identities and socket cookies. Forks can also add holders
after enumeration. Excluding deliberate endpoint sharing by a cooperating
engine/host is a responsibility of the trusted provisioning environment, not
an exhaustive guarantee supplied by pbps. Known alias/proxy negatives remain
required tests; they do not certify all possible cooperating proxies.

The same observation is used by run-owned private channels and supplied-server
binding/revalidation. Runtime-enforced launch privileges, namespaces, cgroups,
foreign-sharer checks, session counters and scratch/target separation remain
independent premises. No target is frozen or reconfigured. This runtime work
does not deliver the later binding-planning or source-handling stages of #595.

## Dedicated scratch servers (#609)

A supplied server is not provisioned by pbps, so it cannot be asked for exact
controls; it is asked to already be a **known layout**: a container, started
by the operator from the documented recipe under a Docker-API runtime pbps
can reach as root, named by the endpoint together with the externally
enforced profile it is claimed to meet. The name lives with the credentials
rather than in `pbps.yml`, so nothing in git claims a server meets a profile
pbps has not measured. An unimplemented name is refused by name, per engine.

This is deliberately not a proof that an arbitrary container is contained.
That question has no bottom — every round of review of an earlier shape of
this profile found one more arrangement of the kernel's tables that hid
something, because the tables of a container someone else assembled can be
arranged in any number of ways. Equality with a layout is decidable: the
daemon's record is read, the kernel is measured against the rows the profile
names, and a row it does not name is refused as that row (DECISIONS 514).

Admission connects through a **Docker-API daemon** (`dockerd`), the same peer-authenticated local channel the Docker profile uses; the record and mount rules are measured against both Docker's and Podman's container layouts so they are not over-fit to one runtime's exact output, but a Podman-native daemon is not yet an admittable peer (#686).

`linux-dedicated-v1` requires, of a container on the same native Linux kernel:

| Premise | What is measured |
| --- | --- |
| Record | The daemon's record of the container: running, not privileged, `NetworkMode: none`, a read-only root, a private PID, IPC, UTS and user namespace — Podman's default `shareable` IPC namespace is refused, since another container can join it — no binds, devices, ports, links or volumes, only tmpfs mounts, a memory and PID limit. Its id, init PID, start time and image are pinned, and re-read on every check: a restarted or replaced container is a different runtime |
| Separation | The container's init and engine service are neither the target's service process nor in any of its PID, mount or network namespaces, and the engine's instance identity is not the target's. Decided **before** the record and containment measurements, so an alias of the target refuses as the target |
| Network | The container's network namespace holds only a loopback device — a real one, by link type and flag — with no IPv4 or IPv6 route and no address but `::1` |
| Anchors | PID 1 seen through the container's `/proc` is in its own PID namespace, and its `/sys` shows only that loopback device: a host procfs or sysfs bound in keeps the type and not these |
| Mounts | Every row of the init's mount table, uncollapsed, is one the profile names: the read-only image root; `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/mqueue` and `/sys/fs/cgroup` with their kinds and flags; the read-only `/proc` files on the same procfs; the masks Docker lays as empty tmpfs and Podman as binds of `/dev/null`; the tmpfs `/tmp`, `/dev/shm`, `/run` and `/var/tmp`; the runtime's `/etc` files bound read-only from an ordinary filesystem with the complete private contents described above; and the engine's storage as a fresh tmpfs. Two rows at one target are two mounts stacked, which no runtime lays out. Both runtimes' layouts were measured and are pinned by unit tests |
| Privileges | Every task in the container's PID namespace — not only the ones the service started — at the profile's uid **and** group, with no-new-privileges, a seccomp filter and the capability ceiling, still in the container's network, mount, IPC and UTS namespaces, judged as found rather than against an earlier listing. That a seccomp filter is *loaded* is measured (`Seccomp: 2`); its BPF contents cannot be read from `/proc`, so attesting the exact policy — to exclude a non-IP channel such as `AF_VSOCK` that the loopback network checks do not contain — is the operator's provisioning responsibility, a documented limit (#684) |
| Resources | cgroup-v2 memory, swap, CPU and PID bounds on the init's cgroup that exist and are within the profile's ceilings; `max` is not a bound. Every task must be in that cgroup or below it |
| Accounting | Nothing shares the container's mount or IPC namespace that is not in its PID namespace, and nothing shares its network namespace but those tasks and this run's own forwarders. A container joined with `--network container:` is in no process listing and is caught here. The forwarder exception is by PID namespace, not by an exact task set: a forwarder's `bash` reaps and respawns its `cat` pipes, so a captured task list races a legitimate child, and joining that namespace needs `--pid container:` on the same root daemon, whose socket also lists the container id — so what excludes it is not the name but that root daemon access is provisioning-administrator access, the boundary this profile does not claim to hold against. Narrowing the exception to the forwarder's exact tasks is not pursued, for that reason (#681) |
| Lifetime | A bounded run deadline, which the forwarders' own root guards share: past it the next check refuses and the caller's exit path removes the resources. Not a watchdog: a caller that never returns leaves the resources for a human, a documented limit (#641) |

The Docker PostgreSQL recipe starts directly as `999:999` with `--cap-drop ALL`.
The runtime creates its private storage with that ownership, so neither
`initdb` nor the engine needs a root ownership bootstrap. SQL Server starts as
uid/gid 10001 with only `NET_BIND_SERVICE` in its bounding ceiling: the tested
executable carries that file capability and an empty bounding set makes its
exec fail. Both recipes, including their complete containment options, live
in [`live-resolver-server.py`](../scripts/live-resolver-server.py).

For the measured Podman 4.9 component, which rejects Docker's tmpfs `uid`
option, the alternative is a fresh root-owned tmpfs, `chown` of its root to
the final engine identity, then `setpriv` **before** password-file preparation
or engine initialization. This bootstrap needs only `CHOWN`, `SETUID`,
`SETGID`, `SETPCAP`, and SQL Server's `NET_BIND_SERVICE`; it clears groups and
drops to the final capability ceiling. Both engines answered SQL under this
alternative. It does not make a Podman-native daemon admissible (#686), nor
does an operator's claim replace supplied-server qualification (531).

## Analysis scope qualification (#610 PostgreSQL, #611 SQL Server)

Once a run holds a scratch database, `ScratchRun::qualify` establishes that it
is a place the target's declarations can be compiled as the target's own
deployer would (ADR-0016 cases 5, 14, 16, 21, 23). The lifecycle is one; what a
scope is belongs to each engine, behind `resolver::scope`. PostgreSQL first:

| Premise | What is measured |
| --- | --- |
| Compatibility | Under `pg-analysis-scope-v1`: equal patch-level version; encoding and locale, including the provider's *actual* collation version; every target extension installable on the resolver, with the native libraries its C functions name; the effective settings the dialect does not pin; and the deployer's effective schema visibility. Each fact is match, mismatch or unknown, and an unknown fact refuses like a provisioning failure rather than reading as a match |
| Executables | The engine image and each loaded or required native library by content, not version: the running image through `/proc/<pid>/exe`, a loaded library through `/proc/<pid>/map_files`, an unloaded required library as a disk candidate; a library replaced under the running process is caught by inode identity, so a same-version build with different content or a parser-hook library is a mismatch |
| Deployer | The planning connection's `current_user`, reproduced on scratch with run-local `pbps_role_<n>_<token>` roles created NOLOGIN with the measured attributes and memberships, reachable only under `SET ROLE`; the reproduction is verified by re-reading it as the mapped deployer, and compiling as the setup administrator instead is refused |
| Stability | The target's facts and its deployer's authorization are read in one catalog snapshot, so the sealed scope never holds half of a change committed between them. The scope is sealed to the target and scratch connections, so a reopened session cannot inherit it, and requalified on every check, so an extension, setting, collation or authorization change between checks invalidates the run |

The scope carries a deployment-authorization fingerprint later steps seal
(#614) and apply rechecks against its own session (#616). What it does not
prove is named: a target that is not on this kernel is unqualifiable
(executable content is unreadable); code loaded dynamically inside a routine
body, or by an operator's `LD_PRELOAD`, is outside this proof domain; and a
target whose recorded collation version has drifted from its provider's is
recorded as a limitation, not a resolver mismatch.

Required library aliases are correlated with a mapped file only when its inode,
measured file size and content digest agree with the candidate (#712). Equal
inodes on different filesystems do not establish this association, and equal
sizes alone are insufficient. Each mapping is observed once for both correlation
and reporting; legitimate symlink spellings remain supported on native and
overlay filesystems.

Both engine rules require loaded-content evidence for a library already mapped
into the backend. If `map_files` cannot supply that content, a digest obtained
from the disk path remains `Unknown` on that side, even when the digests match
or a measured build mapping names them (#698). A required library not yet
loaded may still qualify through its disk candidate; it can match the same
content already loaded on the other side.

SQL Server is qualified under its own rules, measured on 2025 (17.0) for Linux.
Qualification is not a binding adapter: SQL Server binding stays unimplemented
(#619, #620).

| Premise | What is measured |
| --- | --- |
| Compatibility | Under `mssql-analysis-scope-v1`: equal product version, level and update; the same operating system; server and database collation, compatibility level, containment and the database-level ANSI options, which the scratch database is created to match; the session's effective statement settings — language, date format, first day of the week and the SET options a statement persists or an indexed expression requires — which are the login's and the driver's, not the server's defaults; `QUOTED_IDENTIFIER` and `ANSI_NULLS` on, the only module settings this tool manages; user CLR assemblies by content, in both directions. A hosted or unknown product family (`EngineEdition` outside 2–4) is refused by name and never mapped to a boxed build; an edition difference inside the boxed family verifies with a named limitation, because binding is the same and capability stays the target's to check |
| Executables | As for PostgreSQL, plus the engine's own packages: SQL Server for Linux maps its binaries out of `.sfp` files, so the ELF at `/proc/<pid>/exe` is only the loader and the mapped packages are the engine, hashed like any loaded library |
| Deployer | The planning connection's database user, read as itself: `fn_my_permissions` on the database and each in-scope schema, `IS_ROLEMEMBER`, its default schema and language, the users it may impersonate, and the grant rows it can see. Reproduced with run-local users `WITHOUT LOGIN` and roles named `pbps_principal_<n>_<token>`, database-scoped, reachable only under `EXECUTE AS USER`; `dbo`, `public` and the fixed `db_` roles keep their identity; grants and DENYs are replayed under their own grantors. A `dbo` deployer — which a member of `sysadmin` is everywhere — is the run login itself, which owns its scratch database. The reproduction is verified against the target as read, and only then are the plan's grants run, as the reproduced deployer, so one it could not make is the engine's refusal |
| Stability | Catalog views are not a snapshot under any isolation level, so the target's facts and authorization are read twice and must agree. The rest is PostgreSQL's: sealed to both connections, requalified on every check against what was sealed |

The scratch database is created the way the target's is: collation, containment,
compatibility level and the database-level ANSI options. A **partially
contained** target therefore needs a scratch server whose
`contained database authentication` option is 1; a fresh server has it at 0 and
refuses the `CREATE DATABASE` (error 12824, measured). The run asks before it
creates anything and refuses by the option's name. It does not set the option:
that is configuration of a server pbps did not provision, and it would outlive
the run.

What it does not prove is named here too: server-level permissions of the
deployment login beyond `dbo` are outside a database scope; a change made and
undone between the two bracketing reads is not seen by them, only a lasting one
by the next check; and an assembly the target has is a mismatch rather than
something the resolver reproduces.

The engine is reached the way the Docker profile reaches its own: a
run-owned **forwarder**, launched from the supplied container's image into
its network namespace with its own PID and mount namespaces, piping exactly
one TCP session to the engine's loopback port through an authenticated
attach stream. The operator supplies no relay, no socket directory and no
process id; PostgreSQL listens on loopback with no Unix socket, SQL Server on
its port. Each session is bound to the kernel: the one established pair to
the engine's port whose client end the forwarder's processes hold, and whose
server end is observed in one engine thread group — on PostgreSQL the backend the
engine itself reports for the session. The forwarder's tasks are checked
against the fixed program at the fixed privileges, as in the Docker profile.

Exclusivity combines kernel observations with engine evidence under the
trusted provisioning boundary. In the fixed loopback layout, both ends of
an ordinary TCP session appear in the namespace's table: a listener is the
engine's, a row with no inode is a connection already gone, and any other
observed row must be one end of a session this run opened or a refusal. These
reads are sequential observations, not an atomic socket/holder inventory. Any Unix socket at all is a refusal — the recipe
gives the engine no Unix listener. What that cannot see is a session that
opened and closed between two reads, so each engine also supplies a
**cumulative** session counter, and only this run's own sessions may have
moved it:

- PostgreSQL sums `pg_stat_database.sessions` with `max(stats_reset)` as the
  counter's origin. Measured on PostgreSQL 18: autovacuum workers, launched
  parallel workers and this run's own DDL leave it unchanged, while a client
  session that has already disconnected is still counted — with one exception
  measured on the same engine: a `walsender` moves it by nothing at all, so a
  replication connection is caught by the session list and the kernel census
  while it is open and by neither once it has closed (#651). A database
  created, used and dropped entirely between two checks is invisible the same
  way — its row and its share of the sum are gone before either is sampled
  (#682). A sum has a way
  back down that a single counter does not: the row is per database, and
  dropping one takes its share away. The rows the total was summed over
  therefore come back with it, and the set may only grow.
- SQL Server reads the General Statistics `Logins/sec` counter, which is
  cumulative despite its name, with `sqlserver_start_time` as its origin.
  `@@CONNECTIONS` is unusable here: it also counts the engine's internal
  connections, and rose by eighteen during one `CREATE DATABASE`.
- A dedicated SQL Server runs with **customer feedback off**
  (`[telemetry] customerfeedback = false` in `mssql.conf`). With it on, the
  engine's own telemetry client logs in over loopback a few minutes after
  startup — measured on 17.0: `SQLServerCEIP`, as `NT AUTHORITY\SYSTEM` from
  `127.0.0.1`, five and a half minutes in — and that is a socket in the
  engine's namespace, a session in its list and a login on its counter that
  this run did not open. All three signals refuse it, correctly by their own
  terms, and a run that straddles the moment ends as not exclusive (#611).
  With the setting off no such session appears (measured on Developer
  edition). This is the operator's premise, like the container's layout; the
  profile does not try to tell the engine's own client from somebody else's.

Neither counter is readable without the privilege to see other sessions, and
an unreadable counter refuses rather than reporting an idle server; so does a
refusal the adapter raises for a login without that privilege, which is a
credential problem and never an intrusion. The privileged reads belong to the
control session, which connects with the operator's administrative
credentials; a run-owned login sees only its own row and supplies its own
session key, which is all the census needs from it. The first inventory is
the baseline every later total is measured against, so a session this run
did not open is refused there rather than absorbed into it. Every check reads
the engine's list between two kernel censuses and closes with one more read
of the counter, so what holds over the whole check is the session premise and
not an instant inside it; the kernel premises hold at the instants they are
measured, and the run's bounded lifetime limits how far apart those are.

Run-owned resources are one uniquely named database and one login, created
only after every gate above and removed on every exit path, and a removal
that cannot be confirmed reports those two generated names and any forwarder
container whose removal was not confirmed either. The cleanup capability is
not a live handle: it is the credentials, the daemon path and the container's
pinned identity, from which a fresh session can be opened to remove exactly
those names. Every forwarder the run opened is removed with the run and its
removal confirmed before success is reported, including one whose session
ended early; a forwarder that cannot be confirmed gone is a recovery name too. A check that refuses ends the analysis and not the cleanup; an
await cancelled mid-exchange leaves a protocol stream in no known state, so
the session it was on is dropped, the analysis is over, and cleanup opens a
fresh session to the pinned container — and reports the names if that
container is gone. No pre-existing logging, audit policy, grant or database is
altered to make a server qualify.

## Startup and continuity

1. Acquire the selected image on the actual API connection. Another connection
   cannot reuse that acquisition's provenance for startup.
2. Reserve an empty private runtime with a root lifetime guard and a fixed,
   unprivileged bootstrap waiter. The runtime prepares storage for the final
   workload identity; `setpriv` drops the UID, GID, supplementary groups and
   capability sets before the waiter acknowledges its probe. Remove inherited
   image environment variables before execution and disable inherited
   healthchecks. No database is created.
3. Qualify the actual target, namespace separation, empty external network,
   effective mounts and resource limits. Retain these process and kernel
   handles while releasing the fixed bootstrap command.
4. Await the engine's source-free readiness signal. Listening alone does not
   establish PostgreSQL readiness. A failed handshake discards the run; it
   never reconnects a partly qualified database session.
5. Open a fixed forwarder in separate PID/mount namespaces, sharing only this
   run's network namespace. Bind its one database connection to the actual
   backend and forwarder socket owners, authenticated daemon streams and target
   connection identity. Then recheck the measured runtime premises.

The lifecycle API exposes source-free identity, continuity and cleanup only.
Engine build/native-library compatibility and deployment context belong to
#610/#611. Binding adapters must finish that qualification and the applicable
source-handling gates before transferring declarations or consuming results.

Process, namespace, socket and connection handles remain live capabilities;
they are not serializable booleans. Kernel and engine identity checks surround
continuity reads. Cancellation takes the complete session state before awaiting
I/O, so a later restoration cannot revive it. Any lost control, replacement or
unreadable premise discards the connection and requests owned cleanup. Cleanup
may establish a separately authenticated connection only to remove the exact
owned resource; it cannot resume analysis. A fresh analysis starts from fresh
resources and qualification.

The launch authority and final workload authority are deliberately separate:

| Role | UID / GID | Capabilities |
| --- | --- | --- |
| Root bootstrap and deadline guard | 0 / 0, only its own supplementary group allowed | `SETUID`, `SETGID`, `SETPCAP`, `KILL`; add `NET_BIND_SERVICE` only for the SQL Server workload |
| PostgreSQL waiter and workload | 999 / 999, supplementary groups cleared | All five capability sets empty |
| Run-owned SQL Server waiter and workload | 10001 / 0, supplementary groups cleared | Each capability set at most `NET_BIND_SERVICE` |
| Fixed forwarder | 65534 / 65534, supplementary groups cleared | All five capability sets empty |

The guard must retain **effective `CAP_KILL`**, not merely stay below a maximum
mask. Without it the measured root `timeout` could not terminate its differently
owned child. Admission checks this before releasing bootstrap and during later
channel checks. The source-free waiter must have the exact final UID/GID and
empty supplementary group set; each engine's launch command and observer use
the same workload identity. The guard's exception remains bound to its retained
process identity, never inherited by another task at PID 1's numeric coordinate.

Once applied, no-new-privileges and seccomp restrictions survive fork, clone
and exec; ordinary workload code cannot undo the privilege drop. This was
measured with a worker thread, fork child and exec child under both workload
ceilings and the forwarder policy, including failed attempts to regain root,
another group, `SYS_ADMIN` or a user namespace. The workload's connect syscall
remained denied while the forwarder's remained allowed. See the kernel's
[no-new-privileges](https://docs.kernel.org/userspace-api/no_new_privs.html) and
[seccomp inheritance](https://docs.kernel.org/userspace-api/seccomp_filter.html)
contracts. No-new-privileges alone is not a sandbox: it does not remove already
held capabilities or constrain a provisioning administrator's runtime-exec
entry point. Namespace task observations therefore remain, alongside mount,
network, cgroup, endpoint and exclusivity checks. The factory and owned
forwarders also require the fixed effective-policy
probes below (#633). Source handling (#617) and other delivery gates are
not discharged by these launch measurements, and the contents of a supplied
engine's policy stay the operator's provisioning responsibility (#684).

Scratch also retains a weak witness to the target's live binding and its actual
socket. Dropping or cancelling the target invalidates scratch's next identity
or continuity check; the scratch handle cannot keep that discarded connection
alive merely by retaining its opaque identifier.

Reading a cached identity revalidates its synchronous native leases: the target's
socket/backend/service binding, and the candidate's workload/control containment
and private channel. Failure discards the affected capability permanently, even
without a preceding asynchronous continuity check; owned scratch cleanup follows
the same drop path. This does not replace the engine re-read in that check.

## Effective seccomp checks (#633)

The factory checks actual prohibited-call behavior before engine initialization.
A `Seccomp: 2` status and a Docker report matching the requested JSON are still
necessary observations, but neither substitutes for this gate. Native target
separation and containment qualification precede the probe; a separate fixed
acknowledgement and a recheck of the retained native leases precede the
engine-start command. Canceling either exchange removes the owned runtime.
The fixed forwarder runs its own role-specific probe before accepting its
private protocol channel, including forwarders used with supplied servers.

The probe issues fixed harmless calls through x86-64, i386 `int 0x80`, and x32:
workload `connect`, process inspection/descriptor theft, `unshare`, io_uring,
`AF_VSOCK`, and all three send entry points with `MSG_FASTOPEN`. It also checks
i386's multiplexed `socketcall(SYS_CONNECT)`. The intended derivative returns
kernel `EPERM` before validating those calls' deliberately invalid operands.
The forwarder omits the outbound-connect probes because that role must open
the fixed private connection. Unknown results, missing interpreter support,
unavailable ABI/memfd support, incomplete output and a failed probe refuse
startup before declarations or scratch DDL. Ordinary compilation and real
socket/SQL loopback negatives separately exercise the initialized engine.

Safe Rust constructs a small fixed ELF with no dynamic loader, writable segment
or compiler dependency. The supported engine images' existing Perl writes it
to an anonymous memfd, seals it against writes/growth/shrinkage, and executes it
with a close-on-exec descriptor. No extra image, host helper installation,
persisted executable, writable executable mount or declaration-derived code
is introduced. The file disappears when the probe exits. The bootstrap and its
engine descendants inherit the same restrictions through fork/clone/exec;
filters cannot be relaxed by those unprivileged descendants. Existing native
process/session continuity checks remain in force. This does not add a global
or continuous task census.

This is a fixed behavioral qualification under DECISIONS 533's trusted
provisioning premise, not equivalence checking of arbitrary BPF or resistance
to an administrator deliberately providing probe-specific rules. It does not
certify the filter of an already running supplied engine; that stays the
operator's provisioning responsibility (#684).
The alternative kernel-read route was measured: `PTRACE_SECCOMP_GET_FILTER`
requires a privileged unfiltered tracer and a stopped tracee, and equivalent
Docker policies can differ in architecture-dispatch instruction ordering.
The runtime therefore does not trace or stop target/scratch tasks to obtain a
policy hash. DECISIONS 542 records the selected boundary.

## External containment

| Boundary | Enforced premise |
| --- | --- |
| Network | Only a private loopback device, no external routes, no published ports or host network; workload `connect`, compat `socketcall` and TCP Fast Open are denied |
| Control | Fixed loopback destination and program, separate PID/mount namespaces, one authenticated protocol session, no reconnect, bounded Docker framing and aggregate traffic |
| Filesystem | Read-only image root, limited private tmpfs storage, no host binds/runtime socket/devices, masked host proc/sys interfaces, no executable private storage |
| Privileges | Root deadline guard; engine-specific unprivileged UID and bounded capabilities; no-new-privileges; deny-by-default seccomp derivative |
| Resources | cgroup-v2 CPU/memory/swap/PID bounds, tmpfs bounds, file-descriptor limits and an independent root deadline |
| Cleanup | Locally generated ownership token plus immutable container ID; exact absence confirmation, including automatic-removal races; no label-wide deletion |

The control process alone can initiate the private database connection. The
workload cannot open a second session, acquire the forwarder's descriptors or
reach an external writer. Namespace isolation and the fixed bootstrap exclude
another session from the run's scratch scope, including a change-and-restore
sequence between fingerprint reads. These are runtime premises; image labels,
initial/final SQL hashes or the existence of one live socket do not replace
them. Native-library/parser-hook and engine-specific analysis requirements
remain separate qualification gates.

## Verification

`scripts/live-resolver.py <pg|mssql>` requires the explicit preloaded fixture
images and runs the Docker ownership, gate, actual kernel pairing, filesystem,
network and lifetime tests. Disposable internal-network listeners receive real
DNS and metadata-shaped traffic, and a private volume holds a synthetic host
file and mock runtime socket. The protected recipe reaches none; removing each
corresponding fixture boundary produces the expected effects. No real host data
or Docker socket enters those sentinels. The pairing inspector has a private PID namespace
and receives only read-only proc directories from its owned workload/control
trees. It receives no host PID namespace, runtime socket or external network.
The root-control inspector separately measures its one owned root guard and
read-only cgroup controls, with and without permission to join the owned UTS
namespace. The containerized inspectors need `SYS_ADMIN` for the supported UTS
read; the missing-permission case must refuse. These test access paths are not
production adapters.

`scripts/live-resolver-server.py <pg|mssql>` builds a disposable TLS target and
a supplied scratch server this script — not pbps — starts from the documented
recipe, plus an alias endpoint naming the target's own container and, for one
test, a second supplied server on an ordinary bridge network. It compiles a
table and a view in the run's own scratch database, and covers same-instance
aliases, an unimplemented profile name, a session the run did not open that
closes again before the next check, a session present at admission, a
statistics row removed to pay for an intruder, a privileged process the engine
did not start inside its namespaces, a container joined to its network
namespace, target replacement and loss, and a server stopped under a live run
so that cleanup cannot be confirmed. It prints the daemon's record and the
kernel's tables of the supplied server once at startup, which is where the
unit tests' pinned layouts come from.

`scripts/live-resolver-launch.py <pg|mssql> --test-binary <absolute-path>` runs
as root on the disposable native host. It checks the real dropped waiter and
refuses wrong UID/GID, supplementary groups, a larger capability bounding set,
missing no-new-privileges and a guard without effective termination authority.
No start line is sent, so every refusal precedes engine initialization. Each
owned container is removed before the test asserts its result. The new group
and guard regressions failed on the previous implementation and passed after
the checks were added; CI runs them for both engines.

The launch runner also starts actual connect-permissive, Fast-Open-permissive,
i386 socketcall-permissive and Docker-default filters. Their mode-only native
checks still pass, and a simulated Docker report containing the expected policy
passes the recipe check; the behavioral gate must refuse them. Missing probe
support also refuses. The control fixture separately permits process inspection
or Fast Open and requires refusal. The ordinary reserved-session test cancels
both before and after the new policy acknowledgement; the private-channel test
retains real-engine DDL, SQL-initiated extra-connection refusal, real-socket
Fast Open refusal and terminal control-loss cleanup.

The same runner also mutates only explicitly marked owned workload/control
mount namespaces to test omitted masks, replacement devices, nonempty or
writable masks, and unreadable directories while Docker's report remains
unchanged. It drops the observer's DAC bypass capabilities for the unreadable
case. Unchanged masks and genuinely absent optional interfaces must still pass;
all helper failures follow owned cleanup before the test reports a failure.
The launch runner also injects host information into actual files on owned waiters
and verifies refusal without ever sending the engine-start line. Its SQL-read
fixture also runs as root: qualifying the guard and changing the owned mount
namespace require native host privileges. These fixtures change a generated
file's already-bound inode or add a
read-only overmount in the owned container when the file belongs to its image;
the shared image layers and Docker's reported recipe remain unchanged. Both real engines read all synthetic bytes
through SQL, including comment-only and alternate-layout injections; workload,
control and shared forwarder-guard checks must reject them. Ordinary fixed files,
contained DDL and confirmed cleanup remain positive controls.

`scripts/live-resolver-target.py <pg|mssql>` creates disposable TLS targets and
confines each inspector to its owned target's PID/network namespaces. It checks
aliases, database/login changes, backend-child refusal, canceled bindings and
intermediate proxies. A TLS-terminating proxy deliberately alters a managed-only
query on a plaintext backend connection: the real client accepts the TLS hop
and engine result, while native qualification refuses the proxy. The database
transport fixtures also reject corrupted and replayed TLS application records.

The UTS regressions run on both real engines with fixed generated files and
Docker API `NetworkDisabled=true` empty image files. Synthetic hostname and NIS
markers refuse before initialization/admission; in-place changes to each
workload and forwarder discard live analysis while Docker's recorded recipe is
unchanged. Ordinary table/view DDL and owned cleanup still succeed. PostgreSQL
can read both names through `pg_read_file`; SQL Server's `MachineName` property
exposes the hostname, while the measured procfs `OPENROWSET` reads fail with
error 12703 and are not counted as successful reads. A separate process fixture
changes only its UTS namespace while retaining its PID and executable; the
lease must fail without moving the observer's namespace. Actual removed-check
runs pin the negative cases.

The dedicated CI resolver matrix runs both profiles separately. On its
disposable native Linux runner, `live-resolver-target.py --native-host` also
exercises the complete public factory using a prebuilt test binary with root
process-inspection access, with the target sharing the host PID namespace.
`scripts/live-resolver-sockets.py` runs acknowledged reparenting, shared and
unshared thread-table, service-mismatch, bounded empty-result and descriptor
handoff cases in a disposable PID/mount namespace. All required CI jobs, including this matrix, must
pass on the current PR head before merge. A component-only test run cannot
stand in for this complete factory check.

The fixed bootstrap's environment removal follows Moby's key-only override
semantics ([Moby environment implementation](https://github.com/moby/moby/blob/v28.3.3/container/env.go)).
The seccomp derivative retains its Apache-2.0 attribution in the adjacent
`NOTICE`; its additional network restrictions include the implicit connection
performed by [TCP Fast Open](https://man7.org/linux/man-pages/man2/send.2.html).
PostgreSQL readiness uses the engine's
[versioned PID-file status](https://github.com/postgres/postgres/blob/REL_18_6/src/include/utils/pidfile.h).

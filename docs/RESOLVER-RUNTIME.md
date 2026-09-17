# Resolver runtime boundary

This describes the internal runtime layer for #608 and #609. Named selection is exposed
by the CLI (#606); binding planning is still gated on the later #595 steps.
Acquisition and a live candidate are not environment compatibility, deployment
authorization, retained-source permission or sealed binding evidence.

## Supported initial profile

`linux-amd64-v1` uses a direct local Docker Unix API and a native Linux target
observer on the same kernel. The API's actual Unix peer must be a live,
root-installed Docker daemon, reached through a protected root-owned socket
path. A root-owned proxy is insufficient. The target adapter accepts direct
loopback verified TLS whose actual server socket belongs to the selected
engine service's process tree. A configured PID bounds inspection; it does
not assert identity. Database names, credentials and aliases do not establish
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

The host kernel, its administrators, the selected daemon and explicitly trusted
image installation form the provisioning trust boundary. SQL privileges in
scratch grant no authority over those external controls. This profile does not
claim protection from a compromised kernel or provisioning administrator.

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

`linux-dedicated-v1` requires, of a container on the same native Linux kernel:

| Premise | What is measured |
| --- | --- |
| Record | The daemon's record of the container: running, not privileged, `NetworkMode: none`, a read-only root, a private PID, IPC, UTS and user namespace — Podman's default `shareable` IPC namespace is refused, since another container can join it — no binds, devices, ports, links or volumes, only tmpfs mounts, a memory and PID limit. Its id, init PID, start time and image are pinned, and re-read on every check: a restarted or replaced container is a different runtime |
| Separation | The container's init and engine service are neither the target's service process nor in any of its PID, mount or network namespaces, and the engine's instance identity is not the target's. Decided **before** the record and containment measurements, so an alias of the target refuses as the target |
| Network | The container's network namespace holds only a loopback device — a real one, by link type and flag — with no IPv4 or IPv6 route and no address but `::1` |
| Anchors | PID 1 seen through the container's `/proc` is in its own PID namespace, and its `/sys` shows only that loopback device: a host procfs or sysfs bound in keeps the type and not these |
| Mounts | Every row of the init's mount table, uncollapsed, is one the profile names: the read-only image root; `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/mqueue` and `/sys/fs/cgroup` with their kinds and flags; the read-only `/proc` files on the same procfs; the masks Docker lays as empty tmpfs and Podman as binds of `/dev/null`; the tmpfs `/tmp`, `/dev/shm`, `/run` and `/var/tmp`; the runtime's `/etc` files bound read-only from an ordinary filesystem; and the engine's storage as a fresh tmpfs. Two rows at one target are two mounts stacked, which no runtime lays out. Both runtimes' layouts were measured and are pinned by unit tests |
| Privileges | Every task in the container's PID namespace — not only the ones the service started — at the profile's uid **and** group, with no-new-privileges, a seccomp filter and the capability ceiling, still in the container's network and mount namespaces, judged as found rather than against an earlier listing |
| Resources | cgroup-v2 memory, swap, CPU and PID bounds on the init's cgroup that exist and are within the profile's ceilings; `max` is not a bound. Every task must be in that cgroup or below it |
| Accounting | Nothing shares the container's mount or IPC namespace that is not in its PID namespace, and nothing shares its network namespace but those tasks and this run's own forwarders' processes — the forwarder's guard and its descendants, not its PID namespace. A container joined with `--network container:` is in no process listing and is caught here |
| Lifetime | A bounded run deadline, which the forwarders' own root guards share: past it the next check refuses and the caller's exit path removes the resources. Not a watchdog — see #641 |

The engine is reached the way the Docker profile reaches its own: a
run-owned **forwarder**, launched from the supplied container's image into
its network namespace with its own PID and mount namespaces, piping exactly
one TCP session to the engine's loopback port through an authenticated
attach stream. The operator supplies no relay, no socket directory and no
process id; PostgreSQL listens on loopback with no Unix socket, SQL Server on
its port. Each session is bound to the kernel: the one established pair to
the engine's port whose client end the forwarder's processes hold, and whose
server end exactly one engine process holds — on PostgreSQL the backend the
engine itself reports for the session. The forwarder's tasks are checked
against the fixed program at the fixed privileges, as in the Docker profile.

Exclusivity is decided by the kernel and confirmed by the engine. Because the
namespace has no route out, every session to the engine is a TCP connection
whose both ends are in that namespace's own table, so the census is complete
for what reaches the engine: a listener is the engine's, a row with no inode
is a connection already gone, and any other row is one end of a session this
run opened or a refusal. Any Unix socket at all is a refusal — the recipe
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
  while it is open and by neither once it has closed (#651). A sum has a way
  back down that a single counter does not: the row is per database, and
  dropping one takes its share away. The rows the total was summed over
  therefore come back with it, and the set may only grow.
- SQL Server reads the General Statistics `Logins/sec` counter, which is
  cumulative despite its name, with `sqlserver_start_time` as its origin.
  `@@CONNECTIONS` is unusable here: it also counts the engine's internal
  connections, and rose by eighteen during one `CREATE DATABASE`.

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
   unprivileged bootstrap waiter. Remove inherited image environment variables
   before execution and disable inherited healthchecks. No database is created.
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

Scratch also retains a weak witness to the target's live binding and its actual
socket. Dropping or cancelling the target invalidates scratch's next identity
or continuity check; the scratch handle cannot keep that discarded connection
alive merely by retaining its opaque identifier.

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
read-only cgroup controls. These test access paths are not production adapters.

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

`scripts/live-resolver-target.py <pg|mssql>` creates disposable TLS targets and
confines each inspector to its owned target's PID/network namespaces. It checks
aliases, database/login changes, backend-child refusal, canceled bindings and
intermediate proxies. A TLS-terminating proxy deliberately alters a managed-only
query on a plaintext backend connection: the real client accepts the TLS hop
and engine result, while native qualification refuses the proxy. The database
transport fixtures also reject corrupted and replayed TLS application records.

The dedicated CI resolver matrix runs both profiles separately. On its
disposable native Linux runner, `live-resolver-target.py --native-host` also
exercises the complete public factory using a prebuilt test binary with root
process-inspection access. All required CI jobs, including this matrix, must
pass on the current PR head before merge. A component-only test run cannot
stand in for this complete factory check.

The fixed bootstrap's environment removal follows Moby's key-only override
semantics ([Moby environment implementation](https://github.com/moby/moby/blob/v28.3.3/container/env.go)).
The seccomp derivative retains its Apache-2.0 attribution in the adjacent
`NOTICE`; its additional network restrictions include the implicit connection
performed by [TCP Fast Open](https://man7.org/linux/man-pages/man2/send.2.html).
PostgreSQL readiness uses the engine's
[versioned PID-file status](https://github.com/postgres/postgres/blob/REL_18_6/src/include/utils/pidfile.h).

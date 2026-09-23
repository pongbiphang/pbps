# Connections and drivers

The connection seam, the two drivers, TLS, and how database errors are reported.
Part of the [decision record](../DECISIONS.md), which says how to add an entry
here.

<a id="decision-21"></a>

21. **`pbps.yml` names the env var, never the connection string** (`url_env:`),
    and nothing prints one: `db::redact` reduces it to server/database and
    degrades to a placeholder rather than echoing what it could not parse.

<a id="decision-193"></a>

193. **`DbError` reports the server's error code as text.**
    `server_error_number() -> Option<u32>` was the one place `pbps-db` held a
    T-SQL shape under a neutral name: PostgreSQL's SQLSTATE is five characters
    that may be letters (`42P01`), so a `u32` could never carry the second
    engine's answer, and every caller comparing against it would have been
    written against the first engine's. It is `server_error_code() ->
    Option<String>` now, and the single caller compares against `"208"`. Owned
    rather than borrowed because this driver hands back a number and the error
    holds no string for a `&str` to borrow from (ADR-0014 §1). Landed ahead of
    the PostgreSQL crate, with 194 and 195, so that crate's diff carries only
    what is new.

<a id="decision-225"></a>

225. **`Conn` becomes an enum over two drivers — not a trait object, not a type
    parameter.** ADR-0014 ruled out deciding *how* while there was one real
    driver ("the useful abstraction is the one drawn from two implementations
    that both exist"); this is the second, so the deferral ends. Measured on
    `4abb917`, the enum leaves all 22 `pbps-mssql` functions that take
    `conn: &mut Conn` untouched. A trait object needs `async fn` in a
    dyn-compatible trait, which Rust has not got — so hand-rolled boxed futures
    or a new dependency, to abstract over exactly two implementations that both
    live in this workspace. Generics spread a type parameter across those 22
    functions and everything calling them, and the dialect is a runtime value
    out of `pbps.yml`, so the dispatch would only move to the CLI. The enum is
    also what `Param` chose in this same file, for this same reason, before
    there was a second driver.

    The cost is the `FromColumn` blanket impl: two of them, one per driver,
    overlap and coherence refuses them. The closed set that replaces it is
    `&str`, `i32`, `i64`, `i16`, `u8` and `bool` — **from the compiler, not
    from reading the source.** Counted by eye it looked like three, because
    `bool`, `i16` and `u8` reach the seam through `get(&row, "max_length")`
    with the type inferred from the struct field and never spelled at the call
    site. The set is not engine-neutral either: `u8` is SQL Server's `tinyint`,
    and PostgreSQL's arm refuses it rather than inventing a conversion.

    ADR-0007 decision 5's "exactly one file" becomes **one file per driver**:
    `pbps-db::mssql` names `tiberius`, `pbps-db::postgres` names
    `tokio_postgres`, and nothing else in the workspace names either.
    `DbError::Driver` stops carrying one driver's error type and carries text
    plus an optional code, so the seam's own error names no driver.

<a id="decision-228"></a>

228. **One rustls crypto provider in the tree, and the connector names it
    anyway.** Asking for `ring` while `tiberius-ng` resolves `rustls` with its
    own default compiled **both** providers in. rustls then cannot determine a
    process-level provider and **panics** — not errs — the first time a
    `ClientConfig::builder()` runs, which is inside a connection, where the
    seam has no way to report it. Nothing caught this: it builds, it lints, and
    `cargo deny` is green; the PostgreSQL live suite hit it on its first
    connection to a real server, which is the argument for that suite existing
    before there is a dialect to test.

    So the PostgreSQL TLS stack takes `aws-lc-rs`, the one already in the tree,
    and `pbps-db::postgres` builds its config with `builder_with_provider`
    rather than the process default — the same shape `tiberius` uses on the
    other side of the seam. One provider makes the ambiguity impossible; naming
    it makes a future second provider unable to change which one this connector
    uses, or to reintroduce the panic.
    uses, or to reintroduce the panic.

<a id="decision-229"></a>

229. **The connection seam dials one TCP endpoint, and refuses every string
    that means anything else.** `Conn::connect` opens the socket itself, which
    is what keeps `Connect` and `ConnectTimeout` two errors instead of one
    (ADR-0014 §3) — so it has to choose a host and a port, and libpq strings can
    name things that choice cannot honour: a Unix socket path, a `hostaddr` the
    driver would dial while `host` is what TLS checks, or several hosts to try
    in turn. Each of those is now a `BadConnectionString` that says which.

    It was a silent fallback to `localhost:5432`, with a comment claiming the
    connection would then "fail to connect saying so". It would not: a machine
    configured with a Unix socket is exactly the machine with a server on
    `localhost:5432`, so the fallback **succeeded**, against a different
    endpoint under a different authentication method. Supporting these properly
    is a feature and belongs to whoever needs it; guessing is not the smaller
    version of it.

<a id="decision-231"></a>

231. **`target_session_attrs` is reproduced at the seam, not refused and not
    dropped.** `Config::connect` runs a `SHOW transaction_read_only` probe
    *after* the handshake; this seam calls `connect_raw` — which is what keeps
    the three connection failures three (ADR-0014 §3) — and inherits none of
    it. Dropped silently, a string saying "never a writable primary" would have
    got one and run DDL on it.

    Reproduced rather than refused, unlike `hostaddr` and multiple hosts in 229,
    and the difference is which failure each choice risks: refusing
    `target_session_attrs=read-write` would refuse a string that works, and
    refusing a valid input is the one thing this project's review rules put
    first. The probe is fifteen lines and needs nothing new.

    It gets its own `DbError::WrongSession`, because it is the one connection
    failure that is not about *reaching* a server — `cannot reach {addr}` would
    be false, and the fix is a different server rather than an open port. That
    does not make ADR-0014's three into four: those three are how a socket can
    fail, and this is a server that answered.

    What the seam still drops is in issue #113: `keepalives`, `tcp_user_timeout`
    and `connect_timeout` are applied by the driver's own `connect_socket` and
    by nothing here. Left there rather than fixed with this one, because
    honouring them needs a new dependency and refusing them refuses strings that
    work — a choice, not a bug fix.
    work — a choice, not a bug fix.

<a id="decision-232"></a>

232. **Opening the socket is one function, shared by both drivers, and it gives
    every resolved address a chance inside one budget.** `TcpStream::connect(host)`
    resolves the name and tries the addresses **in turn**, returning the last
    error, so a timeout wrapped around it bounds the *whole loop*: one address
    that drops packets spends the entire budget and a healthy second address is
    never tried. A dual-stack endpoint whose IPv6 address is black-holed is the
    ordinary case of that, and reporting a server that is up as unreachable
    refuses work — the failure this project's review rules put first.

    `open_socket` resolves first and tries each address itself. The budget is
    divided as it is spent — each attempt gets what is left over how many
    addresses are left — so the total is still `CONNECT_TIMEOUT` however many
    there are, an address that refuses at once hands its share to the rest, and
    the last one gets the remainder. Fixed shares would make a slow-but-
    answering server fail behind a dead one, and a full budget each would make
    `CONNECT_TIMEOUT` mean *N* times what it says.

    A refusal from some address outranks the clock. `Connect` names something
    with a fix the reader can act on, and only when no address answered at all
    is this the dropped-packets case that `ConnectTimeout` describes.

    In `pbps-db` itself rather than in either driver, because "there is a
    network" is this crate's (ARCHITECTURE) — and because the defect was on both
    sides of the seam. `pbps-db::mssql` had the same line, and fixing only the
    engine under review is how a shape becomes a second finding.

    The tests build the black hole out of a listening socket whose accept queue
    is full, and take the address list and the budget as parameters: resolution
    order is the operating system's, and a test that depends on it passes or
    fails by luck. What is *not* pinned by a failing test is the resolution half
    on its own — a hostname resolving to two addresses this test controls is not
    portable (`localhost` is one address on some machines and two on others), so
    the loop is pinned and the resolving is read.

<a id="decision-234"></a>

234. **A TLS stack is built only when the connection may use one, and ALPN is
    offered only for direct SSL.** Two halves of the same mistake: doing TLS
    work that the connection string has already ruled out, and not doing the
    TLS work it asks for.

    Building the stack reads the host's certificate store, and `tls()` refuses
    when that cannot be read — deliberately, because a trust store that failed
    to load is not an empty one. Under `sslmode=disable` that refused a
    connection over certificates it was never going to look at, on exactly the
    minimal image SPEC §11.3's single static binary is for. `connect` now
    branches, and the disabled path hands the driver `NoTls`.

    The other half is ALPN. **Measured on PostgreSQL 18.6**: a direct SSL
    connection that offers no ALPN is refused — `received direct SSL connection
    request without ALPN protocol negotiation extension` in the server log —
    while the TLS handshake itself *completes*, so the failure lands after it
    and reads as the connection dropping rather than as a protocol requirement.
    Neither `tokio-postgres` nor `tokio-postgres-rustls` sets it, so
    `sslnegotiation=direct` could not connect at all. Offered only for `Direct`,
    because the `SSLRequest` negotiation the default uses asks for none and
    libpq offers none there either.

    Both are pinned by unit tests over the parsed config rather than by the live
    suite. The suite's server has TLS off, as CI's does, and giving it a
    certificate this client trusts is its own piece of work; what is measured
    here was measured with `openssl s_client` against the same image and is
    written down above rather than asserted.

<a id="decision-417"></a>

417. **The connected seam is a `match` on the connection's driver in
    `pbps-cli`, and the answers it routes are `pbps-db`'s types.** Step 10 of
    Phase 5 (#85) had to remove `dialect()`'s refusal of `postgres`, and the
    refusal was the least of it: `pbps-cli` named `pbps_mssql` at every
    connected call site — the ledger, the catalog, the rename impact, the
    edition, `doctor`'s permission read — 206 times, so the CLI *was* the SQL
    Server implementation with a `Dialect` bolted on for the pure half. Two
    shapes were weighed for the seam.

    A trait in `pbps-dialect` was the obvious one and is not possible:
    `pbps-db` already depends on `pbps-dialect` for `TransactionFraming`
    (ADR-0014 §2), so a trait method taking a `Conn` is a dependency cycle.
    And it would have bound the pure half — `validate`, offline `plan`, which
    take only `pbps-dialect` — to the connection crate, for the benefit of the
    other half. The engines therefore keep their connected work as free
    `async fn`s over `pbps_db::Conn`, as ARCHITECTURE already said, and the
    CLI's `engine` module routes to them: one function per question, an
    exhaustive `match` on `Conn::driver()` in each, native `async fn` with no
    `dyn` and no new dependency. A third engine is a compile error in every
    one of those functions until it has an answer for each, which is the same
    completeness a trait gives, in the place the engine's author would look.

    **The types moved to `pbps-db`, beside the ledger's, for the reason the
    ledger's are there** ("one definition each, filled by whichever engine").
    `Pulled`, `Unexpressible`, `Limitation`, `UnmanagedModule`, `Catalogued`,
    `Misspelt`, `Spellings`, `RowsError`, `RenameTarget`, `Referrer`,
    `ImpactReport`, `ImpactError`, `GrantTargets`, `DataDemand` and the
    `doctor::Ask` were each defined twice, and the pairs had drifted:

    ```text
    Pulled          mssql +unmanaged_modules       pg +limitations (both now)
    Catalogued      pg +key_collation
    RenameTarget    mssql +Module
    ImpactReport    pg +carried
    Spellings       mssql Vec<RowConflict>         pg Vec<String>
    Limitation      mssql IntrospectionLimitation  pg Limitation
    ```

    Each is one struct now, with the union of fields, and every field one
    engine never fills says so on the field — `unmanaged_modules` is empty
    from PostgreSQL because that pull *leaves out* what it cannot hold and says
    so in `warnings`, `carried` is empty from SQL Server because text carries
    nothing, `Module` is never built by PostgreSQL's target builder and is
    refused by name by its `rename_impact`. They could not have gone to
    `pbps-dialect`: `RowsError` and `ImpactError` wrap a `DbError`, and the
    same cycle applies. Each engine re-exports them under its old paths, so
    its own code and tests did not move.

    What stayed with the engine is what only the engine can spell: `Held`,
    `Gap` and `Securable` (a gap's securable is rendered in that engine's
    `GRANT` spelling, `OBJECT::[dbo].[t]` against `TABLE "app"."t"`, so the
    seam hands the CLI the rendered text), the rename-target builder (only
    SQL Server asks the drop side of a module rename through the impact query;
    PostgreSQL answers it in `modules`), `truncate_reason` (UTF-16 units on
    one, characters on the other), and PostgreSQL's `key_collations`, which
    its `misspelt` now reads itself rather than trusting a caller to have
    asked — the collation is the one input to the spelling check that is the
    catalog's, and a caller that forgot it got the database default without a
    word (DECISIONS 148).

    **An engine-only question is answered by name on the other engine, never
    with an empty answer.** `capabilities` on PostgreSQL is a fact —
    `edition: None` means one edition, `supports_online: true` because every
    release the emitter targets builds an index `CONCURRENTLY` — and a read
    that failed is the `Err`, so `doctor` cannot confuse the two.
    `edition_verdict` refuses nothing there for the same reason. The four role
    questions (`names_alike`, `principals_holding`, `role_members`,
    `role_owned_securables`) *refuse* on PostgreSQL, citing 211: each exists
    to clear a `CREATE ROLE`, `RENAME ROLE` or `DROP ROLE` that dialect never
    plans, so an answer would be one to a question nobody asked, and "none"
    would be the empty answer this tool refuses everywhere else. The schema
    spelling question (142) got a PostgreSQL implementation because it *can*
    be asked there honestly: a quoted identifier is compared byte for byte, so
    `App` and `app` are two schemas and never two spellings, and the query
    answers presence — measured, the present name comes back as itself, the
    upper-cased one as absent.

    What the seam taught, recorded here because the third engine will read
    this before any code (SPEC §12): the `pbps_mssql` rename-target builder
    never received 407's table-name mapping, so on that engine a column rename
    on a table the same plan renames is asked about under a name the catalog
    does not have yet, and `OBJECT_ID` answers NULL — an empty impact report
    for a rename that has referrers. That mapping landed separately in 416
    and is preserved in the engine's `rename_targets` function here.
    PostgreSQL's pull keeps no
    unmanaged-module inventory, so an object it leaves out is invisible to
    `unmanaged: error` as well as to the plan; PostgreSQL's `doctor` does not
    yet ask about the rights a `data:` block or a `role:` grant needs, and the
    seam hands them over so that the day it does no caller changes; and the
    PostgreSQL-only connected checks initially left unwired — `roles::missing_roles`,
    `rename_evidence`, `drop_blockers`, `unsupported_permissions`,
    `modules::before_a_rebuild` — were reached only by that crate's live suite.
    Follow-ups track them; the review fixes in 419–420 wire role-rename
    evidence and module rebuild checks into the CLI. The first thing the seam
    taught that *was* this step is 418: the first end-to-end `bootstrap` on
    PostgreSQL was refused by the engine's own pull, and the CLI now says
    where each catalog read runs.

<a id="decision-434"></a>

434. **The PostgreSQL seam honours the socket-tuning parameters 231 left open,
    rather than refusing them.** `keepalives`, `keepalives_idle`,
    `keepalives_interval`, `keepalives_retries`, `tcp_user_timeout`,
    `connect_timeout` and `load_balance_hosts` were each neither applied nor
    refused — the shape PITFALLS calls "a
    comment that describes a check the code does not make", one level down: no
    comment even claimed a check here. Refusing them, the other honest option
    231 named, was rejected because every one of them is a string that
    connects and works today; refusing a valid input is the failure this
    project's review rules put first (issue #113).

    `DbError::ConnectTimeout`'s message was `"...did not answer within {}s...",
    CONNECT_TIMEOUT.as_secs()` — the constant, interpolated at every call site,
    not a field. The moment a connection string can set its own budget that
    becomes a false statement: an operator who asked for `connect_timeout=10`
    and waited 10 seconds would be told the seam waited 30. So the variant
    gained an `after: Duration` field carried per instance — required to keep
    the message honest once `connect_timeout` is a real input, not a
    convenience added for its test. That field is new to this branch, and so
    was a bug in it: a first draft reported `connect_any`'s own reduced
    sub-budget (`budget` minus whatever `open_socket`'s DNS resolution had
    already spent) rather than the original request, which — because
    `Duration::as_secs()` truncates rather than rounds — read a whole second
    short of the true budget even for a numeric address resolving in
    microseconds. The live suite caught it on this branch's first run
    (`29.999979671s` reported where the string asked for `30s`); the fix
    reports `budget`, the total the caller actually asked for, rather than the
    remainder one sub-step of it was left holding.

    `socket2` and `rand` are new direct dependencies of `pbps-db`, and cost
    nothing `cargo deny` had not already priced: both are already resolved in
    the tree at the versions named here — `socket2` through `tokio`'s own
    `net` feature, `rand` through `tokio-postgres`'s `postgres-protocol` — so
    this adds an edge to an audited node rather than a new one. `cargo deny
    check` stayed clean before and after, with no new duplicate-version
    warning. `socket2::SockRef::from(&tcp)` sets keepalive, and
    `tcp_user_timeout` where the platform honours it, on the socket
    `pbps_db::open_socket` already opened, after `open_socket` returns and
    before `connect_raw` takes it — the same place `set_nodelay` already
    runs, for the same reason: these are socket properties, not session ones.
    `rand::seq::SliceRandom` shuffles the resolved address list before
    `connect_any` tries any of it.

    `keepalives` and `keepalives_idle` are applied unconditionally once
    keepalive is on, because `Config`'s accessors cannot tell "the string set
    this" from "nobody said" for either of them — applying whatever the
    accessor returns is correct in both cases, the same value `Config::connect`
    would have used. `keepalives_interval` and `keepalives_retries` are
    applied only when `Some`, under the identical `#[cfg]` platform exclusions
    `tokio-postgres`'s own `keepalive.rs` uses — copied rather than invented,
    because a platform set either wider or narrower than the driver's stops
    being parity with it. `tcp_user_timeout` is the one exception to "honour":
    `TCP_USER_TIMEOUT` does not exist outside Linux, so a request for it
    elsewhere is refused by name (mirroring `tokio-postgres`'s own
    `#[cfg(target_os = "linux")]` for the same option) rather than silently
    dropped — the one parameter of the seven where "honour here, refuse there"
    is coherent, because the missing thing is the OS feature, not pbps's
    support for it. That refusal runs in `Conn::connect_as` on the parsed
    `Config`, beside `endpoint`'s own `hostaddr`/multi-host/Unix-socket
    refusals and before `open_socket` is ever called — not inside
    `apply_socket_options`, where a first draft of this fix put it. A review
    of this PR caught the difference: on a platform that cannot honour
    `tcp_user_timeout`, a connection string naming it and pointed at an
    unreachable endpoint would have dialled first and reported the network
    failure — `Connect` or a `ConnectTimeout` that spent the whole budget —
    instead of the named configuration refusal this paragraph promises. A
    string this build will not accept is refused without a network round
    trip, the same rule `endpoint`'s own refusals already follow, not only
    when the parameter happens to be one `open_socket` never touches.

    `connect_timeout` is a **ceiling**, not a default: `CONNECT_TIMEOUT`'s own
    doc comment calls it "short enough that a pipeline blocked by a firewall
    reports it while someone is still watching" — an operational bound pbps
    enforces for itself, stated unconditionally, not "the default when the
    string doesn't say". A request at or below 30s is honoured exactly, fed
    into `open_socket`/`connect_any` as the shared total budget — 232's own
    rule, "the budget is divided as it is spent... so the total is still
    `CONNECT_TIMEOUT` however many [addresses] there are", applied unchanged
    to a value that now comes from the string instead of the constant. Not
    `tokio-postgres`'s own per-address-attempt application (its own doc
    comment: "this timeout will apply to each address of each host
    separately"), which would let a dual-stack name's `connect_timeout=10`
    run up to 20s and quietly contradict the number in the string — 232
    already ruled that shape out for the constant, and nothing about the
    value coming from a string instead changes the reason. A request above
    30s is refused by name, naming the ceiling,
    rather than silently capped — silent capping is the same "neither applied
    nor refused" shape this decision exists to remove, just relocated instead
    of fixed. `connect_timeout=0` and a negative value are `tokio_postgres`'s
    own way of saying "unset" (its string parser only calls the setter for a
    value greater than zero), so both fall back to the ceiling exactly like
    saying nothing.

    `load_balance_hosts=random` reorders the same resolved-address list
    `tokio-postgres`'s own `connect_host` shuffles, before either tries an
    address — not a socket option, and not refused: unlike `hostaddr` and
    multiple hosts (229), a string naming it connects and works today, it
    just distributes nothing without this fix.

    The SQL Server side has no matching gap: `tiberius-ng`'s `Client::connect`
    takes an already-open stream and never opens a socket itself, and its
    ADO.NET parser has no keepalive, timeout or load-balancing key to read —
    checked in the driver source, not assumed. `pbps_db::open_socket`'s
    `mssql` caller passes `CONNECT_TIMEOUT` and `shuffle: false` unconditionally
    for that reason.

    Pinned by unit tests in `crates/pbps-db/src/postgres.rs` for the budget
    ceiling, the zero/negative fallback, `load_balance_hosts`, keepalive
    on/off, and the `tcp_user_timeout` refusal as a pure function of platform
    (so the refusal is exercised on every CI runner, not only the ones the
    real `#[cfg]` excludes); a fifth,
    `keepalive_settings_land_on_the_real_socket_not_only_the_parsed_config`,
    asserts the OS's own `SO_KEEPALIVE` and `TCP_KEEPIDLE` state through
    `SockRef`, not the parsed `Config`. `order_addresses` is pinned directly
    in `crates/pbps-db/src/lib.rs` for both `Disable` and a seeded `Random`.
    The ordering fix has its own two:
    `a_tcp_user_timeout_the_platform_cannot_honour_is_refused_before_any_socket_opens`
    calls `Conn::connect_as` with `is_linux: false` against a host that is
    never resolved (RFC 2606 `.invalid`) and a generous `tcp_user_timeout`,
    and asserts both the refusal's message and that it returns in well under
    a second — a platform this test can pin without needing to run on
    Windows for real, the same reason `tcp_user_timeout_disposition` itself
    takes `is_linux` as a parameter rather than a bare `#[cfg]`; its negative
    case beside it, naming no `tcp_user_timeout`, still reaches `open_socket`
    and fails as an ordinary `Connect` against a bound-then-dropped local
    port, so the ordering fix has not swallowed a genuine network failure
    into a configuration refusal.
    The live suite adds a smaller-`connect_timeout` test
    (`a_smaller_connect_timeout_gives_up_sooner_than_the_ceiling`) beside the
    existing 30-second black-hole test in `crates/pbps-pg/tests/live.rs`.

<a id="decision-454"></a>

454. **`From<tokio_postgres::Error> for DbError` reads the server's own
     sentence off `as_db_error()`, not `Display`, and only `message()` plus the
     object identifiers — never `detail()`, `hint()` or `where_()`.** Measured:
     `tokio_postgres` keeps a server-side failure's message in the error's
     *source* (`DbError`, the driver's own type of that name) and renders
     `Kind::Db` as the literal five-character string `db error` on `Display` —
     `e.code()` still answered the right SQLSTATE, so the bug passed every test
     that only checked the code. `message()` alone closes issue #167; the
     object identifiers (`schema()`, `table()`, `column()`, `datatype()`,
     `constraint()`) are folded in beside it under an `OBJECT:` label, because
     several diagnostics this workspace builds today reconstruct by hand
     exactly what these carry, and each is a name PostgreSQL itself declared as
     an identifier, never a value — `message()`'s own quoting already treats
     them the same way.

     A first draft of this fix folded `detail()`, `hint()` and `where_()` in
     too, each under its own label (`DETAIL:`, `HINT:`, `CONTEXT:`), on the
     reasoning that they are diagnostic text the server already composed.
     Ready-phase review of PR #464 caught what that reasoning missed: these
     three are not the server's *sentence* — they are one of the ways the
     server hands back **data**, and this function's return value reaches
     stderr (`crates/pbps-cli/src/main.rs`, which CI logs) and the deployment
     ledger's `reason` column (`failed_apply_snapshot` in
     `crates/pbps-cli/src/deploy.rs`), durably, in the audited history this
     project exists to keep trustworthy. **Measured** on 18.6, each of the
     three can carry values a deployer declared nowhere: `detail()` is *for*
     data (`"Key (email)=(alice@example.com) already exists."`,
     `"Failing row contains (null)."`); `hint()` is free text a user's own
     PL/pgSQL can set to anything (`RAISE EXCEPTION '...' USING HINT =
     format('the offending value was %s', v)`), and nothing server-side stops
     a data trigger this tool's own guard exists to police
     (`crates/pbps-pg/src/data_triggers.rs`) from doing exactly that; and
     `where_()` — `CONTEXT:` in `psql`'s own vocabulary — is not only the
     safe-looking call stack of PL/pgSQL functions and internally generated
     queries active when the error was raised, but also, for a statement that
     fails *inside* a function, that statement's own text with its literals in
     it (`CONTEXT:  SQL statement "INSERT INTO t VALUES ('secret')"`), and this
     seam holds no SQL grammar to tell that line apart from a bare call-stack
     frame (constraint 9; the same reason `data_triggers.rs` parses no SQL
     either). A field this crate cannot classify is not one it can partially
     trust, so all three are dropped unconditionally rather than filtered by
     shape.

     (An intermediate draft of this same fix folded the object identifiers
     under a `WHERE:` label, mislabeling object metadata as execution context
     and dropping the actual traceback `where_()` carries — round-1 review of
     PR #464 caught it, before the ready-phase review above found the larger
     problem with keeping `where_()` at all.) The live suite pins the
     redaction directly: each of `detail()`, `hint()` and `where_()` (in both
     its safe-looking and its literal-bearing shape) gets its own fixture
     proving the field is *absent* from the rendered message while the
     server's sentence and the object identifiers survive — not a test that
     merely stopped asserting presence, which a regression could pass by doing
     nothing. The fallback to `e.to_string()` is unchanged for a failure
     `as_db_error()` answers `None` for: those never reached the server, and
     the driver's own text for them (`"connection closed"`, and so on) was
     never `db error` to begin with.

     `pbps-pg`'s `schema_changed_underneath` and `the_engine_broke_a_tie` each
     wrap this seam's `DbError::Driver` in a sentence of their own for a
     SQLSTATE it recognizes (`XX000`, `40P01`) — written when the wrapped
     message was unconditionally `db error` and the wrapping was reconstructing
     by SQLSTATE alone what the server had already said. Both stay: what they
     add is domain framing an `as_db_error()` fix cannot supply on its own —
     that a `REPEATABLE READ` snapshot cannot see a concurrent `DROP`, that a
     deadlock here is a tie the engine already broke — not a restatement of
     the server's sentence, which the wrapped `{e}` now carries for the first
     time instead of `db error`. Only the doc comments explaining *why* they
     existed needed correcting, not the wrapping itself.

     The SQL Server side does not have this defect: measured on 17.0.4075.5,
     `tiberius::error::Error::Server`'s `Display` is `TokenError`'s own, which
     interpolates its `message` field directly — there is no `Kind::Db`
     standing in for the server's sentence the way `tokio_postgres::Error` has
     one. `crates/pbps-db/src/mssql.rs` carries this measurement as a comment
     and `crates/pbps-db/tests/live_mssql.rs` pins it as a regression guard,
     not a fix.

     The new live tests live in `crates/pbps-db/tests/`, not in `pbps-pg`'s or
     `pbps-mssql`'s own live suites: the seam's `From` impls are what changed,
     and `pbps-db` had no live suite of its own to reach them, so
     `scripts/live-tests-pg.sh` and `scripts/live-tests.sh` each gained one
     line invoking it.

<a id="decision-455"></a>

455. **What the operator's own terminal sees and what `pbps` writes down are
     deliberately different, from PR #464's apply failure path onward.**
     DECISIONS 454 redacts `detail()`, `hint()` and `where_()` at the seam
     because those three are optional enrichment this crate can drop without
     losing anything issue #167 asked for. `message()` cannot be dropped the
     same way — it is the sentence #167 exists to surface — and ready-phase
     review of PR #464 measured that `message()` is not always safe either: a
     failed type conversion names the value it could not convert
     (`invalid input syntax for type integer: "…"` on 18.6,
     `Conversion failed when converting the varchar value '…' to data type
     int.` on SQL Server 2025 — **measured on both engines**, since
     `crates/pbps-db/src/mssql.rs`'s `From<tiberius::error::Error>` builds the
     same `DbError::Driver` the PostgreSQL side does), and
     `crates/pbps-pg/src/emit.rs`'s own comment on `Held::as_stored` already
     said a retyped column's read-back re-parses a reference-data row's
     recorded text through its old type before its new one, and "the engine's
     conversion error names the value" when that parse fails.

     A first reflex here would be to redact `message()` at the seam too,
     matching 454's answer for the other three fields. That would silence
     issue #167's own deliverable: an operator running `apply` against a
     database they already hold credentials for would be back to a message
     with nothing in it, for every failure, not only the rare one that names a
     value. The fix instead is **where** the message goes, not what it says:
     `crates/pbps-cli/src/main.rs`'s stderr print (`{e:#}`) is unaffected — an
     operator already holding credentials to the target sees everything, the
     server's sentence included, which is what issue #167 asked for and
     nothing this decision takes back. What changes is the two places a
     failure's text is written **durably or outward** rather than shown once
     to whoever is already looking: `failed_apply_snapshot` and
     `record_failed_bootstrap` in `crates/pbps-cli/src/deploy.rs`, which write
     into the ledger's `reason` column — SPEC's own audit trail, read later by
     people who were not necessarily the one running that `apply` — and the
     `on_apply_attempt` hook, whose own doc comment calls its payload "stable
     input" a `pbps.yml` author wires to an arbitrary shell command, which can
     relay it anywhere.

     `crates/pbps-cli/src/engine.rs::ledger_safe_reason` is the one function
     both sinks now go through, in place of `error.to_string()`. It walks the
     failing `anyhow::Error`'s `.chain()` and rewrites only a `DbError::Driver`
     frame — the one shape a driver's own server-supplied sentence can reach
     this chain through — to its code and a fixed marker; every other
     frame, `DbError` or not, is this tool's own composed text (a malformed
     connection string, an unreachable host, a catalog row the introspection
     SQL got wrong, or a `.context()` sentence naming the statement that
     failed) and passes through unchanged. Object identifiers stay redacted
     the way 454 already decided; the code is not a value either, and is kept
     because it is the one thing a reader of a *redacted* ledger row can still
     act on.

     This is why `execute_statements` and `apply_staged_under_lock`'s
     per-statement loop changed from `anyhow::anyhow!("...{e}")` to
     `anyhow::Error::new(e).context(...)` / `.context(...)`: the macro's string
     interpolation bakes a driver error's `Display` into a new, sourceless
     string before `ledger_safe_reason` ever runs, which is indistinguishable
     from this tool's own text once it happens — there is no error left inside
     to downcast. Every other call site already reached `anyhow::Error`
     through a bare `?`, which keeps the source intact without needing this
     change; these two were the only ones written the other way, and are now
     the shape every future statement-execution failure should copy.

     The plan's own emitted SQL is not the same category and is not touched:
     `stmt.sql` — embedded in `execute_statements`' `.context()` sentence — is
     this tool's own generated text, deterministic from the checksum-pinned
     plan and the declarations already in git, never a value the server
     computed or a row nobody declared. A reference-data literal a deployer
     wrote in YAML belongs in the ledger the same way it already belongs in
     the plan; a value PostgreSQL or SQL Server hands back from its own
     catalog or an existing row does not, and only the latter is what this
     decision withholds.

     `crates/pbps-cli/src/engine.rs`'s four unit tests pin `ledger_safe_reason`
     directly: a `DbError::Driver` frame with a code redacts to it and
     nothing else; one with none is redacted without inventing a code; a
     non-`Driver` `DbError` frame passes through unchanged; and a
     `.context()`-wrapped driver frame keeps the context and redacts only the
     source. `crates/pbps-cli/tests/flow_pg.rs`'s
     `a_triggers_own_exception_keeps_the_row_value_off_the_ledger_but_not_off_the_operator`
     pins the same property end to end: a data trigger this tool's own guard
     already treats as approved (`crates/pbps-pg/src/data_triggers.rs`) reads
     an undeclared value from a side table and names it in its own
     `RAISE EXCEPTION`, and the resulting apply's ledger `reason` carries the
     code and this tool's own framing but not the value, while the
     operator's stderr carries all of it. A first draft of that fixture put
     the secret in the *declared* row instead of a side table, and its
     failure was itself a useful measurement: pbps's own emitted `INSERT`
     necessarily restates what was declared, so a secret placed there also
     appeared in `execute_statements`' own `.context()` framing — correctly,
     since that framing is the plan's own checksummed text, not a value this
     decision has any business withholding.

     Column retype was tried first, matching the shape ready-phase review
     named literally ("an apply encounters a conversion error"), and
     **measured** not to be reachable through this tool's own emitted DDL:
     `ALTER COLUMN ... TYPE` without an explicit `USING` — which is all pbps
     ever emits, ADR-0012 §5 refusing any retype no automatic cast covers —
     fails a real out-of-range or over-length row with a *generic* message on
     18.6 (`integer out of range`, `value too long for type character varying
     (10)`, `numeric field overflow`), none of which named the value; only a
     bare `CAST('text' AS type)` from an untyped literal does that, which is
     what `Held::as_stored`'s read-back predicate builds from a reference-data
     row's *recorded* text, not what the retyping `ALTER` itself runs against
     existing data. The trigger-exception shape is the same finding's own
     second-named case and reaches the identical `DbError::Driver` this crate
     cannot tell apart from the first, so pinning it is pinning the fix, not a
     different one.

     SPEC has no section describing what a failed ledger entry's `reason`
     holds — `pbps-model`'s own `StateKind::Failed` doc comment says only
     "`reason` records the failure," which stays true; nothing there needed
     correcting.

     A second ready-phase round on PR #464 found two more defects in this same
     function, both from treating `DbError::Driver` as if every caller who
     built one meant "the server said this." **`DbError::Refused(String)`**
     (a new variant, `crates/pbps-db/src/lib.rs`) is the fix for the first:
     several `pbps-pg` guards — an unsafe data trigger
     (`crates/pbps-pg/src/data_triggers.rs`), a catalog read outside the
     transaction it needs (`crates/pbps-pg/src/catalog.rs`), others in
     `drop_impact.rs`, `modules.rs` and `state.rs` — were building their own
     refusal text as a `DbError::Driver` with no code, because that was the
     only variant here with a free-text message and no server type behind it.
     `ledger_safe_reason` could not tell that shape apart from a genuine driver
     frame and redacted it the same way, hiding the one thing an operator
     reading the ledger later needs: which trigger or rule to fix. Rather than
     add a flag or a heuristic to `ledger_safe_reason` to tell the two apart,
     the type itself now cannot hold the ambiguity: a tool-composed refusal is
     `DbError::Refused`, never `Driver`, so it is no longer representable as
     the one shape this function redacts, and falls to the unredacted arm like
     any other of this tool's own text. Every call site that wraps an existing
     `Driver`'s rendered text into a new message of its own — the SQLSTATE- and
     deadlock-recognizing wraps in `catalog.rs` and `modules.rs`, and both
     engines' `migration_error` — is unchanged, because what it carries really
     did originate at the driver. `crates/pbps-cli/src/engine.rs`'s
     `a_refused_frame_names_its_own_rule_and_is_never_redacted` pins the unit
     shape; `crates/pbps-cli/tests/flow_pg.rs`'s
     `an_unapproved_triggers_own_refusal_keeps_naming_it_on_the_ledger` pins it
     live: a trigger installed after a plan is computed against a clean
     baseline is caught by `apply`'s own re-check, and the ledger's `reason`
     still names the trigger.

     The second defect is this decision's and this function's own wording, not
     a caller's: every marker above called the code a SQLSTATE regardless of
     which engine produced it, and **measured** on 17.0.4075.5,
     `tiberius::Error::code()` returns SQL Server's own numeric message number
     (`208`, `2627`, an ad hoc `THROW`'s `50000`, …), which is not a SQLSTATE —
     that word names PostgreSQL's own five-character scheme and nothing on the
     SQL Server side. `ledger_safe_reason`'s marker text is now engine-neutral
     ("the driver reported code …"), true of both without needing to know
     which one is asking. `crates/pbps-cli/src/engine.rs`'s
     `a_mssql_driver_frame_is_redacted_without_being_called_a_sqlstate` pins
     the unit shape; `crates/pbps-cli/tests/flow.rs`'s
     `a_triggers_own_throw_is_redacted_without_being_called_a_sqlstate` pins it
     live against a real SQL Server, the same way the PostgreSQL trigger test
     above pins the first fix: a trigger's `THROW` names a value nobody
     declared to this tool, the operator's stderr still gets the full
     sentence, and the ledger keeps `50000` without calling it something SQL
     Server never sent.

     A third ready-phase round, on the pushed fix for the first two defects,
     found `ledger_safe_reason` itself composing the bounded `reason` column
     in the wrong priority order. It maps `error.chain()` outermost-first,
     which put `execute_statements`' own `.context()` sentence — carrying
     `stmt.sql`, unbounded — ahead of the redacted driver marker it wraps.
     `truncate_reason` (both engines: `pbps_pg::state::REASON_CHARS` and
     `pbps_mssql::state::REASON_UTF16_UNITS`, each 1000) keeps only the
     *first* N units of what it is handed, so a `CREATE VIEW` or a large
     reference-data block put enough SQL ahead of the marker to push it past
     the cut entirely — a `Failed` row left with a fragment of the emitted
     statement and neither the server's message nor its code, the one thing
     that identifies why the server refused. The fix orders by diagnostic
     value per character rather than build order: `.chain().rev()` puts the
     redacted marker first, because `stmt.sql` is this tool's own generated
     text — deterministic from the checksum-pinned plan and the declarations
     already in git — and a partial copy of it in the ledger tells a reader
     nothing they cannot read better from the plan itself, while the marker
     exists nowhere else once `message()` is gone. This is a priority order,
     not a truncation workaround the next author could reorder away without
     noticing what it was protecting.

     `record_failed_bootstrap` and the `on_apply_attempt` hook were checked
     alongside `failed_apply_snapshot`, since a fix that lands on one sink and
     not the other two is a shape this repo's review keeps catching: both
     already compose their message through this same function (the hook's
     unbounded, since its payload is not a fixed-width column), so the
     reordering covers all three without a separate change at either.

     `crates/pbps-cli/src/engine.rs`'s
     `a_context_frame_longer_than_the_column_does_not_crowd_out_the_code`
     pins the boundary directly: a context frame built to outgrow both
     engines' column widths, run through each engine's own `truncate_reason`,
     with the code still present in the result for both.

     A fourth ready-phase round, on the pushed reordering fix, found a
     wrapper whose own `Display` interpolates `{source}`: `RowsError::Read`
     (`crates/pbps-db/src/catalog.rs`), for an apply whose managed-row read
     hits a data-bearing server error, rendered the driver's full message as
     part of *its own* text — before `error.chain()` ever reached the
     `DbError` separately to redact it. Redacting that later frame changed
     nothing; the leak already happened one frame up. Fixed by dropping
     `{source}` from `Read`'s format string: `#[source]` alone is enough for
     `.chain()` to keep walking into it, so nothing downstream needed to
     change to keep seeing it.

     Fixing that surfaced a second, independent defect this crate's own
     review missed: `Read`'s `#[source]` is `Box<DbError>` (boxed so the
     error is not larger than every `Ok` it travels beside), and **measured**,
     a boxed `#[source]` field downcasts through `error.chain()` to
     `Box<DbError>`, never to `DbError` — `thiserror` stores the trait object
     over the `Box` itself, so `downcast_ref::<DbError>()` on that frame fails
     even though `Box`'s own `Display` still forwards to what it holds.
     `ledger_safe_reason`'s match on that frame therefore fell to its
     catch-all, which called `frame.to_string()` and got the driver's raw
     message back regardless of the format-string fix above — the two defects
     compounded, and fixing only the one review found would still have leaked
     the message through the other. `ledger_safe_reason` now tries
     `downcast_ref::<DbError>()` and, failing that,
     `downcast_ref::<Box<DbError>>().map(AsRef::as_ref)`, so a boxed source
     redacts the same as a bare one.

     `crates/pbps-cli/src/engine.rs`'s
     `a_wrapper_that_names_its_source_does_not_repeat_the_drivers_text` pins
     both: a `RowsError::Read` built directly around a `DbError::Driver`
     carrying a value nobody declared, checked against `ledger_safe_reason`
     for the value's absence, the code's presence, and the wrapper's own
     (now source-free) text surviving. Reverted and watched fail for each
     defect independently — the format string alone, then the downcast alone
     — before both were restored together.

     A fifth ready-phase round found a third shape of the same family:
     `LedgerError::Db` and `ImpactError::Query` (`crates/pbps-db/src/
     ledger.rs`, `crates/pbps-db/src/impact.rs`) are `#[error(transparent)]`
     — not a boxed `#[source]` either, but thiserror's instruction to forward
     `Display` to the wrapped `DbError` *and* forward `source()` to the
     wrapped value's own `source()`, skipping the wrapped value itself.
     **Measured**: `error.chain()` on such a value is one frame long, and
     that frame downcasts to the wrapper (`LedgerError`), never to what it
     wraps — the opposite failure from the boxed case above (there a real
     second link downcast to the wrong type; here `.chain()` never produces
     a second link to downcast at all) — and `frame.to_string()` still
     renders the driver's raw message regardless, since `Display` forwards
     independently of whether `source()` does. Reachable through
     `crate::engine::record`, `latest` and `lock`, which return
     `Result<_, LedgerError>`. `ledger_safe_reason` now also tries
     `downcast_ref::<LedgerError>()` and `downcast_ref::<ImpactError>()`,
     unwrapping their transparent `DbError` directly rather than depending on
     `.chain()` to have produced it as its own link.

     `crates/pbps-cli/src/engine.rs`'s
     `a_transparent_wrapper_does_not_repeat_the_drivers_text` pins both
     wrappers directly, the same way the boxed-source test above pins
     `RowsError::Read`. Reverted and watched fail for the expected reason
     (the driver's raw message, unredacted) before being restored.

<a id="decision-456"></a>

456. **Database error context is a separate frame, never part of the driver's
     message (issue #488).** `DbError::Context` holds tool-authored text and a
     boxed `DbError` source. Its `Display` renders only that text; its source
     remains in the error chain, and `server_error_code()` delegates to it.
     A second copy of a server sentence in a wrapper would evade the
     per-frame redaction of DECISIONS 455. Flattening both texts into `Driver`
     instead loses the tool's remediation along with the server sentence.
     `DbError::context` keeps both facts separately without guessing whether
     a particular server error can contain row data.

     The PostgreSQL data-trigger lock denial, catalog-change and module
     deadlock diagnoses, and both engines' timeline-migration diagnoses use
     this frame. Raw driver messages originate only at the driver seam;
     standalone tool refusals remain `Refused`. The operator's full error
     chain retains the original message. Durable reasons and outbound hook
     diagnostics retain the context and redact the driver frame, with the
     code first so a long statement cannot crowd it out of the ledger.

     Unit regressions cover the wrapping sites, code propagation, nested
     context, boxed sources, transparent ledger/impact wrappers, absent codes
     and both engines' truncation. The PostgreSQL CLI regression
     `a_cascade_lock_denial_keeps_its_remedy_on_the_failed_ledger` plans a
     reference-data update with the reached table's lock privilege, revokes
     it while retaining TRIGGER and SELECT, then applies. Both ordinary and
     staged failures must record `42501`, the reached table and the required
     privileges, omit the raw server sentence, and leave the row unchanged.
     Restoring UPDATE lets the same saved plan complete.

<a id="decision-495"></a>

495. **Peer-verified TLS is a connection primitive, not resolver admission.**
     #607 introduces a separate constructor that yields an opaque connection
     only after the driver's verified TLS handshake succeeds. PostgreSQL
     requires explicit `sslmode=require`; SQL Server requires encryption and
     refuses certificate-validation bypasses. Both use native trust roots, and
     SQL Server also retains its explicit CA-file support. The existing
     ordinary constructors keep their defaults; #311 is separate.

     The connection owns an opaque random identity minted after authentication.
     It exposes neither reconnect nor mutable access to its inner connection,
     so replacement cannot retain the old identity. It is not serializable
     plan evidence or a server/cluster identity. Engine SQL remains in the
     engine crates. SQL Server's driver does not expose effective trust getters;
     inspect only the security keys using the same `connection-string` parser
     as the driver, including escaping and duplicate-key precedence. Avoid a
     second connection-string grammar or trust flags supplied by callers.

     Real PostgreSQL and SQL Server TLS fixtures demonstrate verified queries,
     wrong-name/untrusted-root refusal, replacement identities and rejection of
     encrypted replies altered by a controlled relay. A successful TLS hop
     cannot prove the protection of a proxy's backend hop or actual instance
     separation. Consequently #608/#609 must qualify those premises with the
     actual runtime provider, including local private channels and run binding,
     before scratch DDL or evidence acceptance. No admission capability or
     binding path is enabled by this primitive alone (ADR-0016 decisions 4–5).

<a id="decision-500"></a>

500. **A still-starting engine's refused login is retried; everything else it
     says is reported once (issue #638).** The private session waits behind the
     bootstrap's engine-owned readiness greeting, and on PostgreSQL that
     greeting is exact: it waits for `postmaster.pid` to say `ready`, and for
     that engine accepting a connection and authenticating it are the same
     moment. On SQL Server they are not. **Measured** on the pinned image:

     ```text
     in-container 127.0.0.1:1433 accepts                  4170ms
     "SQL Server is now ready for client connections"     4170ms   <- the greeting
     sa can actually log in                               4766ms
     ```

     The greeting is driven by grepping for that very line, so it is early by
     the same ~600ms as the port is, and the resolver logged in inside a window
     where the engine answers TDS and refuses `sa`. That is the intermittent
     `18456` that had been failing the `resolver (mssql)` job.

     **The probe cannot live in the engine container.** For this engine the
     only honest readiness signal is a successful login; a login needs
     `connect`; and the workload's seccomp policy denies `connect` by design —
     only the trusted forwarder may initiate a connection, which
     `native_and_compat_network_and_process_bypasses_are_not_allowed` pins.
     Nor is there a later log line to wait for instead: at the instant the
     login first worked the errorlog tail was msdb upgrade steps, whose
     presence and number depend on whether this is a first start. A probe put
     there anyway does not fail — it *spins*, and the measured cost was the
     whole 90-second budget rather than an error naming the cause.

     **So the retry is where the login already is**, and it retries the whole
     attempt rather than the login: the forwarder opens exactly one TCP session
     to the engine and then pipes it, so a refused login spends the channel and
     the next attempt needs a control container of its own. Three properties
     make that affordable rather than merely possible. The attempt closes its
     own container before the loop builds the next, and an attempt whose
     cleanup could not be confirmed is *terminal* — retrying past it would
     carry a recovery name into a session tracking only the container the next
     attempt made. The loop shares the one 90-second budget a single attempt
     used to have, as a deadline under `timeout_at`, rechecked after the pause
     as well as before it, because `timeout_at` bounds the attempt and not the
     wait in front of it. And the handles for each attempt are minted from one
     held back for exactly that, which re-verifies the daemon peer the way
     every other additional connection here does.

     **Narrow on purpose.** Only `18456` is retried. Every other refusal is
     reported on the first attempt, because a credential this resolver
     generated for a container it started is not going to become correct by
     being asked again, and a retry loop that tolerates every error is a
     timeout wearing a disguise.

<a id="decision-543"></a>

543. **A PostgreSQL connection string that names no `sslmode` is connected
     with verified TLS, not the driver's `prefer`.** (#311.) `prefer` sends
     the SSL request and, when a server answers "N", carries on in
     cleartext; anything that can answer on the port can then ask for a
     cleartext password and receive the deployment credential. The security
     review of #298 reproduced exactly that through `pbps doctor`. With
     `require`, this crate's rustls connector verifies the chain against the
     host's trust store and the host name (`postgres::tls`), so the unwritten
     default is the authenticated one.

     A written `sslmode` is honoured, `disable` and `prefer` included. Those
     are an operator's decision about a network they know — a local
     container, a socket inside one host — and refusing them would push
     people to a different tool rather than to TLS. What changes is that the
     insecure choice has to be spelled out where a reviewer can see it; the
     error for a server that refuses TLS under the default says to add
     `sslmode=disable`, and only on that failure, not on a refused socket or
     a timeout.

     "Names" is the driver's parser's answer, not a second parser here: the
     string is parsed again with `sslmode=disable` placed before its own
     keys, and the two agree exactly when the string names one. A scan
     written here would have to agree with the driver on quoting, escapes
     and the URL form, and every disagreement would be a silent `prefer`. A
     probe the driver cannot parse is read as unnamed, which falls on the
     verified side.

     The test and CI configurations name `sslmode=disable` for their
     TLS-less throwaway servers. `connect_verified` (the resolver's peer
     hop) is unchanged: it still refuses anything but an explicit `require`.

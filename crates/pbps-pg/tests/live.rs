//! What only a live PostgreSQL can answer (SPEC §11.5).
//!
//! The counterpart of `pbps-mssql/tests/live.rs`, and at this step it is
//! deliberately small: the crate holds a connected boundary and two lexical
//! rules, so those are what it measures. The dialect surface — the emitter,
//! introspection, the ledger — arrives one step at a time (issue #76), and each
//! step brings the tests that go with it.
//!
//! What is here is the half the unit tests cannot reach. The scanner's own
//! tests say what pbps *believes* `$tag$…$tag$` and `E'…'` mean; these say what
//! the engine *does*, and assert the two together. A normalizer that is
//! self-consistent and wrong about the engine is exactly the failure ADR-0011
//! Amendment 2 records, and it was found by running the engine.
//!
//! These tests are `#[ignore]`d because they need a server. Run them with
//! `scripts/live-tests-pg.sh`, or start one yourself and:
//!
//! ```text
//! export PBPS_TEST_PG_DB='host=localhost port=54320 user=postgres password=... dbname=pbps_test'
//! cargo test -p pbps-pg --test live -- --ignored
//! ```

use pbps_db::{Conn, DbError, Driver};
use pbps_dialect::{Dialect, TypeChangeRisk};
use pbps_model::ColumnType;
use pbps_pg::Postgres;

/// Opens a connection to the live server this suite runs against.
///
/// The driver is named here once, as it is on the SQL Server side: every test
/// in this file speaks to the container `scripts/live-tests-pg.sh` starts.
async fn connect() -> Conn {
    Conn::connect(Driver::Postgres, &conn_str())
        .await
        .expect("connect to the live server")
}

fn conn_str() -> String {
    std::env::var("PBPS_TEST_PG_DB")
        .expect("PBPS_TEST_PG_DB is not set; see scripts/live-tests-pg.sh")
}

/// The error from a connection that must not succeed.
///
/// A `match` rather than `expect_err`, because [`Conn`] has no `Debug` — a
/// connection is not a value to print — and this is the one place a test wants
/// the failing side of one.
async fn refusal(connection: &str) -> DbError {
    match Conn::connect(Driver::Postgres, connection).await {
        Ok(_) => panic!("`{connection}` connected, and this test needs it not to"),
        Err(error) => error,
    }
}

/// The first column of the first row, as text.
async fn text(conn: &mut Conn, sql: &str) -> String {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.first()
        .expect("one row")
        .try_get::<&str>("?column?")
        .or_else(|_| rows[0].try_get_at::<&str>(0))
        .expect("a text column")
        .expect("not null")
        .to_owned()
}

/// The first column of the first row, as a `bool`.
async fn truth(conn: &mut Conn, sql: &str) -> bool {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.first()
        .expect("one row")
        .try_get_at::<bool>(0)
        .expect("a boolean column")
        .expect("not null")
}

/// The first column of the first row, as an `int4`.
async fn number(conn: &mut Conn, sql: &str) -> i32 {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.first()
        .expect("one row")
        .try_get_at::<i32>(0)
        .expect("an integer column")
        .expect("not null")
}

/// The connection works at all, before anything else here means anything.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_live_server_answers_through_the_shared_connection_type() {
    let mut conn = connect().await;
    assert_eq!(number(&mut conn, "SELECT 1").await, 1);
}

/// The three connection failures stay three, because they need three different
/// fixes and the CLI's diagnostics say which (ADR-0014 §3). A first draft of
/// the spike folded the timeout into `BadConnectionString`, so a target that
/// silently dropped packets for thirty seconds would have been reported as a
/// typo.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_malformed_connection_string_is_not_reported_as_a_network_failure() {
    let error = refusal("port=not-a-number").await;
    assert!(
        matches!(error, DbError::BadConnectionString(_)),
        "{error:?}"
    );
}

/// A socket that answers with RST is a server that is not running, or a port
/// that is wrong — a different fix from a firewall, and a different message.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_refused_socket_names_the_address_it_could_not_reach() {
    // Port 1 on the loopback: privileged, and nothing in this suite binds it.
    let error = refusal("host=127.0.0.1 port=1 user=postgres").await;
    // The other variants are named rather than wildcarded: a category added
    // later has to be looked at here, because "not the one I asked for" is
    // exactly what this test is about.
    match &error {
        DbError::Connect { addr, .. } => assert_eq!(addr, "127.0.0.1:1"),
        DbError::BadConnectionString(_)
        | DbError::ConnectTimeout { .. }
        | DbError::Driver { .. }
        | DbError::WrongSession { .. }
        | DbError::BadRow(_) => panic!("a refused socket is not {error:?}"),
    }
}

/// A firewall that drops rather than refuses is the case an unbounded connect
/// waits out the OS retry for. It has to end in a bounded time and say so.
///
/// The black hole is built here rather than found on the network. A reserved
/// address is not one: `203.0.113.1` dropped the first SYN of a run and then
/// answered `EHOSTUNREACH` for every later one, because the kernel caches the
/// ICMP unreachable — the test passed once and failed on re-run, which is the
/// worst of both answers. A listening socket whose accept queue is full drops
/// the SYN every time instead, and needs no route and no privilege.
///
/// Linux-only for that reason: this is Linux's overflow behaviour with
/// `tcp_abort_on_overflow` at its default of 0. Windows sends an RST, which is
/// the *refused* category, so the test is absent there rather than asserting
/// something the platform does not do.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_dropped_connection_times_out_rather_than_reading_as_a_typo() {
    // Backlog 1, and nothing ever accepts: once the queue is full the kernel
    // stops answering SYNs altogether.
    let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
    socket
        .bind("127.0.0.1:0".parse().expect("an address"))
        .expect("bind");
    let listener = socket.listen(1).expect("listen");
    let addr = listener.local_addr().expect("the bound address");

    // Fill it. Each of these completes the handshake and is never accepted;
    // the one that finds the queue full is the one that hangs, so it is
    // bounded and dropped.
    let mut queued = Vec::new();
    for _ in 0..16 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        {
            Ok(Ok(stream)) => queued.push(stream),
            Ok(Err(e)) => panic!("the loopback refused a connection: {e}"),
            Err(_) => break,
        }
    }
    assert!(
        !queued.is_empty(),
        "the queue never accepted anything, so it was never full"
    );

    let started = std::time::Instant::now();
    let error = refusal(&format!(
        "host={} port={} user=postgres",
        addr.ip(),
        addr.port()
    ))
    .await;
    match &error {
        DbError::ConnectTimeout { addr: reported } => assert_eq!(*reported, addr.to_string()),
        DbError::BadConnectionString(_)
        | DbError::Connect { .. }
        | DbError::Driver { .. }
        | DbError::WrongSession { .. }
        | DbError::BadRow(_) => panic!("a dropped SYN is not {error:?}"),
    }
    let waited = started.elapsed();
    assert!(
        waited >= pbps_db::CONNECT_TIMEOUT && waited < pbps_db::CONNECT_TIMEOUT * 2,
        "the timeout has to bound the wait, not merely follow it: {waited:?}"
    );
}

/// ADR-0011 Amendment 2's table, measured against the engine and asserted
/// beside what the scanner does with the same text.
///
/// Both of the first two failures were **silent**: two definitions returning
/// different strings compared equal, so the change was never planned at all.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_engine_and_the_scanner_agree_on_what_is_data_in_a_definition() {
    let mut conn = connect().await;

    // A dollar-quoted string has no escapes, so its spacing is data.
    let two = text(&mut conn, "SELECT $tag$a  b$tag$").await;
    let one = text(&mut conn, "SELECT $tag$a b$tag$").await;
    assert_ne!(two, one, "the engine keeps the spacing inside $tag$…$tag$");
    assert_ne!(
        Postgres.normalize_definition("SELECT $tag$a  b$tag$"),
        Postgres.normalize_definition("SELECT $tag$a b$tag$"),
        "and so does the scanner"
    );

    // `\'` does not close an `E'…'` string: measured, it is the same
    // ten-character literal as the standard spelling `'it''s  here'`.
    assert_eq!(
        number(&mut conn, r"SELECT length(E'it\'s  here')").await,
        10
    );
    assert!(truth(&mut conn, r"SELECT E'it\'s  here' = 'it''s  here'").await);
    assert_ne!(
        Postgres.normalize_definition(r"SELECT E'it\'s  here'"),
        Postgres.normalize_definition(r"SELECT E'it\'s here'"),
    );

    // In a plain literal the backslash is only a backslash and the quote after
    // it *does* close, which is what makes the rule above specific to `E'…'`.
    // `standard_conforming_strings` is `on` by default and this is what it means.
    assert!(truth(&mut conn, r"SELECT 'a\' = chr(97) || chr(92)").await);

    // Block comments nest here as they do in T-SQL, so the scanner may leave
    // one only at the `*/` that matches its opener.
    assert_eq!(number(&mut conn, "SELECT /* a /* b */ c */ 1").await, 1);
    assert_ne!(
        Postgres.normalize_definition("SELECT /* outer /* inner */ it's here */ 'a  b'"),
        Postgres.normalize_definition("SELECT /* outer /* inner */ it's here */ 'a b'"),
    );

    // A `[` is a subscript, not a quote: a reindent inside one is not a change.
    assert_eq!(number(&mut conn, "SELECT (ARRAY[1, 2])[1  +  1]").await, 2);
    assert_eq!(
        Postgres.normalize_definition("SELECT a[1  +  2] FROM t"),
        Postgres.normalize_definition("SELECT a[1 + 2] FROM t"),
    );

    // And `a$b$c` is one identifier, which is why a `$` that continues a name
    // opens nothing.
    assert_eq!(number(&mut conn, "SELECT 1 AS a$b$c").await, 1);
    assert_eq!(
        Postgres.normalize_definition("SELECT a$b$c  ,  d FROM t"),
        "SELECT a$b$c , d FROM t"
    );
}

/// The measurement behind ADR-0011 Amendment 3: `serial` is a macro, and what
/// comes back is not what was written. That is why it is refused rather than
/// normalized — no normalization could make the two equal, and a schema that
/// differs from itself on every run is the permanent phantom change.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_serial_column_reads_back_as_an_integer_with_a_sequence_it_owns() {
    let mut conn = connect().await;
    let table = format!("pbps_serial_{}", std::process::id());
    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
    conn.execute(&format!(
        "CREATE TABLE {table} (small smallserial, plain serial, big bigserial)"
    ))
    .await
    .expect("create");

    let rows = conn
        .query(&format!(
            "SELECT column_name::text || ' ' || data_type::text AS c \
             FROM information_schema.columns WHERE table_name = '{table}' \
             ORDER BY ordinal_position"
        ))
        .await
        .expect("read the columns back");
    let read_back: Vec<String> = rows
        .iter()
        .map(|r| {
            r.try_get::<&str>("c")
                .expect("text")
                .expect("not null")
                .to_owned()
        })
        .collect();
    assert_eq!(
        read_back,
        ["small smallint", "plain integer", "big bigint"],
        "none of the three spellings survives the round trip"
    );

    let sequence = text(
        &mut conn,
        &format!("SELECT pg_get_serial_sequence('{table}', 'plain')"),
    )
    .await;
    assert!(sequence.ends_with("_plain_seq"), "{sequence}");

    // So the dialect refuses each of them, and names what it read back.
    for (declared, reads_back) in [
        ("smallserial", "smallint"),
        ("serial", "integer"),
        ("bigserial", "bigint"),
    ] {
        let ty: ColumnType = declared.parse().expect("a type parses");
        let message = Postgres
            .normalize_type(&ty)
            .expect_err("a macro is not a type")
            .to_string();
        assert!(message.contains(reads_back), "{message}");
    }

    conn.execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop");
}

/// The two identifier rules, measured rather than assumed. Both were review
/// findings on this PR and both are silent when wrong: a name the tool folds or
/// keeps differently from the engine is a declaration introspection can never
/// match, so the drift report never goes quiet and the `CREATE` makes an object
/// under a name nobody asked for.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_engine_and_the_dialect_agree_on_what_a_name_becomes() {
    let mut conn = connect().await;
    let pid = std::process::id();

    // Case folding is ASCII-only: the server downcases byte by byte and leaves
    // anything with the high bit set alone.
    let declared = format!("AÄfold{pid}");
    conn.execute(&format!("DROP TABLE IF EXISTS {declared}"))
        .await
        .expect("drop");
    conn.execute(&format!("CREATE TABLE {declared} (c int)"))
        .await
        .expect("create");
    let named = text(
        &mut conn,
        &format!(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n \
             ON n.oid = c.relnamespace WHERE n.nspname = 'public' \
             AND strpos(c.relname, 'fold{pid}') > 0"
        ),
    )
    .await;
    assert_eq!(
        named,
        Postgres.fold_ident(&declared),
        "the engine folded the name differently from the dialect"
    );
    conn.execute(&format!("DROP TABLE {declared}"))
        .await
        .expect("drop");

    // 63 bytes is the limit, and past it the server truncates rather than
    // refusing — with a NOTICE nothing here reads.
    let mut long = format!("t{pid}"); // distinct per run, inside the first 63
    while long.len() < 64 {
        long.push('b');
    }
    assert_eq!(long.len(), 64);
    conn.execute(&format!("CREATE TABLE \"{long}\" (c int)"))
        .await
        .expect("create");
    let kept = text(
        &mut conn,
        &format!(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n \
             ON n.oid = c.relnamespace WHERE n.nspname = 'public' \
             AND strpos(c.relname, 't{pid}b') = 1"
        ),
    )
    .await;
    assert_eq!(kept.len(), 63, "{kept}");
    assert_ne!(kept, long, "the server kept a name it in fact truncated");
    conn.execute(&format!("DROP TABLE \"{kept}\""))
        .await
        .expect("drop");

    // So the dialect refuses the name the server would silently rewrite, and
    // takes the one it would keep. Bytes, not characters: 32 `ä` is 64 bytes.
    assert!(Postgres.quote_ident(&long).is_err());
    assert!(Postgres.quote_ident(&long[..63]).is_ok());
    assert!(Postgres.quote_ident(&"ä".repeat(32)).is_err());
    assert!(Postgres.quote_ident(&"ä".repeat(31)).is_ok());
}

/// `target_session_attrs` is a constraint on *which server*, and this seam has
/// to enforce it itself: `Config::connect` runs the probe after the handshake,
/// and `connect_raw` — which is what keeps the three connection failures three
/// — does not. Dropped silently, a string saying "never a writable primary"
/// would have got one, and DDL would have run on it.
///
/// Both directions, on one server: the session's own
/// `default_transaction_read_only` is what `SHOW transaction_read_only`
/// reports, so the connection string can make the same server either kind.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_session_the_connection_string_excludes_is_refused() {
    let writable = conn_str();
    let read_only = format!("{writable} options='-c default_transaction_read_only=on'");

    for (connection, wanted) in [(&writable, "read-write"), (&read_only, "read-only")] {
        Conn::connect(
            Driver::Postgres,
            &format!("{connection} target_session_attrs={wanted}"),
        )
        .await
        .unwrap_or_else(|e| panic!("a {wanted} session satisfies target_session_attrs: {e}"));
    }

    for (connection, wanted, found) in [
        (&writable, "read-only", "read-write"),
        (&read_only, "read-write", "read-only"),
    ] {
        let error = refusal(&format!("{connection} target_session_attrs={wanted}")).await;
        match &error {
            DbError::WrongSession {
                wanted: asked,
                found: got,
                ..
            } => {
                assert_eq!(*asked, wanted);
                assert_eq!(*got, found);
            }
            DbError::BadConnectionString(_)
            | DbError::Connect { .. }
            | DbError::ConnectTimeout { .. }
            | DbError::Driver { .. }
            | DbError::BadRow(_) => panic!("a session mismatch is not {error:?}"),
        }
        // Reached, not unreachable: the message must not read as a network
        // failure, because the fix is a different server and not an open port.
        assert!(!error.to_string().contains("cannot reach"), "{error}");
    }
}

/// Every spelling the catalogue admits, declared against a real server and read
/// back — the test that would have caught `serial`, applied to the whole table.
///
/// The comparison is between **values**, not strings: `format_type` renders
/// `numeric(10,2)` and this model renders `numeric(10, 2)`, and it is the
/// parsed type that introspection will compare (ADR-0011 Amendment 3). What the
/// contract promises is that the two are the same type, and a rendering that
/// differs by a space is not a second type.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn every_spelling_the_catalogue_admits_reads_back_as_the_dialect_says() {
    let mut conn = connect().await;
    let table = format!("pbps_types_{}", std::process::id());

    // Each declared spelling, and nothing else: the dialect's answer is not
    // consulted until after the engine has given its own.
    let declared = [
        "int",
        "int4",
        "integer",
        "int2",
        "smallint",
        "int8",
        "bigint",
        "decimal(10,2)",
        "dec(10,2)",
        "numeric(10,2)",
        "numeric(10)",
        "numeric",
        "decimal",
        "real",
        "float4",
        "float",
        "float8",
        "double precision",
        "float(1)",
        "float(24)",
        "float(25)",
        "float(53)",
        "bool",
        "boolean",
        "char(5)",
        "character(5)",
        "char",
        "character",
        "varchar",
        "varchar(9)",
        "character varying",
        "character varying(9)",
        "text",
        "date",
        "bytea",
        "time",
        "timetz",
        "time with time zone",
        "timestamp",
        "timestamptz",
        "timestamp with time zone",
        "timestamp without time zone",
        "interval",
        "interval(3)",
        "json",
        "jsonb",
        "uuid",
    ];

    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
    let columns: Vec<String> = declared
        .iter()
        .enumerate()
        .map(|(i, spelling)| format!("c{i} {spelling}"))
        .collect();
    conn.execute(&format!("CREATE TABLE {table} ({})", columns.join(", ")))
        .await
        .expect("create one column per declared spelling");

    for (i, spelling) in declared.iter().enumerate() {
        let read_back = text(
            &mut conn,
            &format!(
                "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
                 WHERE a.attrelid = '{table}'::regclass AND a.attname = 'c{i}'"
            ),
        )
        .await;
        let engine: ColumnType = read_back
            .parse()
            .unwrap_or_else(|e| panic!("the engine's own spelling `{read_back}` must parse: {e}"));
        let normalized = Postgres
            .normalize_type(&spelling.parse().expect("a type parses"))
            .unwrap_or_else(|e| panic!("`{spelling}` should normalize: {e}"));
        assert_eq!(
            normalized, engine,
            "declared `{spelling}`: the dialect says `{normalized}` and the engine says \
             `{read_back}`, so every run would report a change that is not there"
        );
        // And the contract's other half, against the engine's own spelling
        // rather than against this crate's idea of it.
        let again = Postgres.normalize_type(&engine).expect("idempotent");
        assert_eq!(again, engine, "normalizing the read-back form moved it");
    }

    conn.execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop");
}

/// `Incompatible` has to mean exactly "this engine refuses the conversion", and
/// the only way to know is to ask it. Twenty types, four hundred ordered pairs,
/// on an **empty** table: with no rows the only thing that can fail is the
/// conversion itself, which is the question the class answers.
///
/// A pair called incompatible that the engine accepts blocks a plan that would
/// have worked; a pair called narrowing that the engine refuses fails half way
/// through an apply, after the changes before it have run.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_engine_and_the_dialect_agree_on_which_type_changes_are_impossible() {
    let mut conn = connect().await;
    let table = format!("pbps_risk_{}", std::process::id());
    let types = [
        "smallint",
        "integer",
        "bigint",
        "numeric(10,2)",
        "real",
        "double precision",
        "boolean",
        "character(5)",
        "character varying(9)",
        "text",
        "bytea",
        "date",
        "time without time zone",
        "time with time zone",
        "timestamp without time zone",
        "timestamp with time zone",
        "interval",
        "uuid",
        "json",
        "jsonb",
    ];

    for from in types {
        for to in types {
            conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
                .await
                .expect("drop");
            conn.execute(&format!("CREATE TABLE {table} (c {from})"))
                .await
                .unwrap_or_else(|e| panic!("create `{from}`: {e}"));
            let accepted = conn
                .execute(&format!("ALTER TABLE {table} ALTER COLUMN c TYPE {to}"))
                .await
                .is_ok();

            let normalize = |s: &str| {
                Postgres
                    .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
                    .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
            };
            let judged = Postgres.type_change_risk(&normalize(from), &normalize(to));
            assert_eq!(
                judged != TypeChangeRisk::Incompatible,
                accepted,
                "`{from}` -> `{to}`: the dialect says {judged:?} and the engine {} it",
                if accepted { "accepts" } else { "refuses" }
            );
        }
    }

    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
}

/// ADR-0012 §6's first trap, measured here so that introspection (Phase 5 step
/// 3) does not walk into it: **a dropped column does not leave**. The catalog
/// keeps the slot with a placeholder name, `attnum` keeps the hole, and the
/// type is not readable at all.
///
/// This is CLAUDE.md's rule wearing its worst disguise: absent, empty and
/// unreadable are three different things, and this row is the third dressed as
/// the first. A reader that does not filter `attisdropped` reports a phantom
/// column whose type cannot be parsed.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_dropped_column_keeps_its_slot_and_its_type_is_not_readable() {
    let mut conn = connect().await;
    let table = format!("pbps_dropped_{}", std::process::id());
    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
    conn.execute(&format!(
        "CREATE TABLE {table} (keep integer, gone text, later integer)"
    ))
    .await
    .expect("create");
    conn.execute(&format!("ALTER TABLE {table} DROP COLUMN gone"))
        .await
        .expect("drop the column");

    let rows = conn
        .query(&format!(
            "SELECT a.attnum::text || ' ' || a.attname || ' ' || a.attisdropped::text AS c \
             FROM pg_attribute a WHERE a.attrelid = '{table}'::regclass AND a.attnum > 0 \
             ORDER BY a.attnum"
        ))
        .await
        .expect("read the columns back");
    let slots: Vec<String> = rows
        .iter()
        .map(|r| {
            r.try_get::<&str>("c")
                .expect("text")
                .expect("not null")
                .to_owned()
        })
        .collect();
    assert_eq!(
        slots.len(),
        3,
        "the dropped column keeps its slot: {slots:?}"
    );
    assert!(slots[1].contains("true"), "{slots:?}");
    assert!(slots[1].contains("pg.dropped.2"), "{slots:?}");
    // And `attnum` is not a position: the third column is still numbered 3.
    assert!(slots[2].starts_with("3 later"), "{slots:?}");

    // The type of the slot is not merely uninteresting — it cannot be read.
    // `format_type` answers `-`, which is no type this model can parse, so a
    // reader that got this far would fail on the parse rather than on the flag
    // it should have checked.
    let ty = text(
        &mut conn,
        &format!(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = '{table}'::regclass AND a.attisdropped"
        ),
    )
    .await;
    assert_eq!(ty, "-");
    assert!(ty.parse::<ColumnType>().is_err(), "`{ty}` must not parse");

    conn.execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop");
}

/// An argument the engine changes **without saying so**. Measured:
/// `interval(7)` is stored as `interval(6)`, with no error and no notice worth
/// the name — the identifier-truncation shape again, one layer down.
///
/// So the dialect refuses it, and this test asserts both halves together: what
/// the engine does with the declaration, and that the dialect will not let it
/// get that far.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_interval_precision_the_engine_would_quietly_reduce_is_refused() {
    let mut conn = connect().await;
    let table = format!("pbps_interval_{}", std::process::id());
    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
    conn.execute(&format!("CREATE TABLE {table} (c interval(7))"))
        .await
        .expect("the engine accepts a precision it will not keep");
    let read_back = text(
        &mut conn,
        &format!(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = '{table}'::regclass AND a.attname = 'c'"
        ),
    )
    .await;
    assert_eq!(read_back, "interval(6)", "the engine reduced it silently");

    let refusal = Postgres
        .normalize_type(&"interval(7)".parse::<ColumnType>().expect("parses"))
        .expect_err("the dialect refuses what the engine would quietly change")
        .to_string();
    assert!(refusal.contains("between 0 and 6"), "{refusal}");

    conn.execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop");
}

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

/// What `Safe` promises, against real rows: the statement runs, and the value
/// that comes out is the value that went in.
///
/// The matrix above uses empty tables, which is the right shape for
/// `Incompatible` and blind to everything else — a classification can be wrong
/// about a boundary value without a single pair changing category. Each row
/// here carries the value that finds the boundary, and the assertion is one
/// implication: **if the dialect says `Safe`, nothing may fail and nothing may
/// change.** Narrowing is not asserted the other way round; that a change *can*
/// lose is not a promise that this row does.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_change_the_dialect_calls_safe_neither_fails_nor_alters_a_value() {
    let mut conn = connect().await;
    let table = format!("pbps_safe_{}", std::process::id());

    // (from, to, a value at the edge of `from`).
    let cases = [
        // Ten digits either side, and one of them stops at 2147483647.
        ("numeric(10,0)", "integer", "9999999999"),
        ("numeric(9,0)", "integer", "999999999"),
        ("numeric(5,0)", "smallint", "99999"),
        ("numeric(4,0)", "smallint", "9999"),
        ("numeric(19,0)", "bigint", "9999999999999999999"),
        // `NaN` is a value every `numeric` holds and no integer type does, so
        // it is the row that decides that direction whatever the widths are.
        ("numeric(4,0)", "smallint", "'NaN'"),
        ("numeric(9,0)", "integer", "'NaN'"),
        // 32767 needs five digits, not four.
        ("smallint", "numeric(5,0)", "32767"),
        ("smallint", "numeric(4,0)", "32767"),
        // A scale larger than the precision still bounds the value.
        ("numeric(2,3)", "numeric(2,4)", "0.099"),
        ("numeric(2,4)", "numeric(2,3)", "0.0099"),
        ("numeric(2,3)", "numeric(3,3)", "0.099"),
        // The first integer a binary float cannot hold.
        ("integer", "real", "16777217"),
        ("smallint", "real", "32767"),
        ("numeric(5,0)", "real", "99999"),
        ("bigint", "double precision", "9007199254740993"),
        ("integer", "double precision", "2147483647"),
        // A `time` into an `interval` at both ends of the day and at the last
        // microsecond, and into the precision that rounds it away.
        ("time", "interval", "'24:00:00'"),
        ("time", "interval", "'23:59:59.999999'"),
        ("time", "interval", "'00:00:00'"),
        ("time", "interval(0)", "'12:34:56.654321'"),
        ("time", "interval(5)", "'12:34:56.654321'"),
        ("interval", "time", "'30 hours'"),
        // The seconds precision the family used to drop.
        ("interval(0)", "interval(6)", "'1 sec'"),
        ("interval(6)", "interval(0)", "'1.234567 sec'"),
        ("interval", "interval(3)", "'1.234567 sec'"),
        ("interval(3)", "interval", "'1.234 sec'"),
        ("numeric(10,2)", "numeric(12,2)", "12345678.90"),
        ("numeric(10,2)", "numeric(10,4)", "12345678.90"),
        // A `date` runs to 5874897 AD and a `timestamp` stops at 294276 AD.
        ("date", "timestamp without time zone", "'300000-01-01'"),
        ("date", "timestamp without time zone", "'2026-01-02'"),
        ("character varying(10)", "character varying(20)", "'abc'"),
        (
            "character varying(20)",
            "character varying(10)",
            "'abcdefghijklmnop'",
        ),
    ];

    for (from, to, value) in cases {
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .expect("drop");
        conn.execute(&format!("CREATE TABLE {table} (c {from})"))
            .await
            .unwrap_or_else(|e| panic!("create `{from}`: {e}"));
        conn.execute(&format!("INSERT INTO {table} VALUES ({value})"))
            .await
            .unwrap_or_else(|e| panic!("insert {value} into `{from}`: {e}"));
        let before = text(&mut conn, &format!("SELECT c::text FROM {table}")).await;

        let altered = conn
            .execute(&format!("ALTER TABLE {table} ALTER COLUMN c TYPE {to}"))
            .await
            .is_ok();
        let after = if altered {
            Some(text(&mut conn, &format!("SELECT c::text FROM {table}")).await)
        } else {
            None
        };

        let normalize = |s: &str| {
            Postgres
                .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
                .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
        };
        if Postgres.type_change_risk(&normalize(from), &normalize(to)) == TypeChangeRisk::Safe {
            assert_eq!(
                after.as_deref(),
                Some(before.as_str()),
                "`{from}` -> `{to}` is called Safe, and `{value}` did not survive it"
            );
        }
    }

    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
}

/// The loss a value's own printing hides.
///
/// `0.1` in a `real` prints as `0.1`, because the engine renders the shortest
/// decimal that reads back as the same float — so the test above, and every
/// round trip through `::text`, says the value survived. It did not: the stored
/// number is `0.10000000149011612`, and the arithmetic says so. This is why the
/// classification asks whether the float holds the value **exactly** rather
/// than whether its digits fit.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_exact_decimal_that_a_float_cannot_hold_is_not_a_safe_change() {
    let mut conn = connect().await;

    // What the engine stores, seen through a wider type rather than through
    // the printing that rounds it back.
    let stored = text(
        &mut conn,
        "SELECT (0.1::numeric(2,1)::real::double precision)::text",
    )
    .await;
    assert_eq!(stored, "0.10000000149011612");
    // And it is not academic: ten of them do not add up.
    let summed = text(
        &mut conn,
        "SELECT SUM(c)::text FROM (SELECT 0.1::real AS c FROM generate_series(1, 10)) s",
    )
    .await;
    let exact = text(
        &mut conn,
        "SELECT SUM(c)::text FROM (SELECT 0.1::numeric(2,1) AS c FROM generate_series(1, 10)) s",
    )
    .await;
    assert_eq!(summed, "1.0000001");
    assert_eq!(exact, "1.0");

    let normalize = |s: &str| {
        Postgres
            .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
            .expect("normalizes")
    };
    assert_eq!(
        Postgres.type_change_risk(&normalize("numeric(2,1)"), &normalize("real")),
        TypeChangeRisk::Narrowing,
        "an exact decimal into a binary float is a change that needs approval"
    );
}

/// The identity rule, asked of the engine rather than of the documentation.
///
/// `validate` refuses an `identity:` on anything but the three integers, and
/// refuses it beside a `default:` or a `nullable: true`. Each of those is a
/// sentence this engine prints, so each is asked here in the form the emitter
/// would produce: if the dialect and the engine ever part company, the pair
/// that disagrees is named.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_engine_and_the_dialect_agree_on_what_can_carry_an_identity() {
    let mut conn = connect().await;
    let table = "pbps_live_identity";
    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");

    for declared in [
        "smallint",
        "integer",
        "bigint",
        "numeric(10,0)",
        "numeric",
        "text",
        "uuid",
        "real",
        "double precision",
    ] {
        let engine = conn
            .execute(&format!(
                "CREATE TABLE {table} (id {declared} GENERATED ALWAYS AS IDENTITY)"
            ))
            .await;
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .expect("drop");

        let mut declaration = pbps_model::Table::default();
        let mut column = pbps_model::Column::new(declared.parse::<ColumnType>().expect("parses"));
        // An identity is NOT NULL on both sides, and `Column::new` is nullable.
        column.nullable = false;
        column.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        declaration.columns.insert("id".to_owned(), column);
        let found = Postgres.validate_table(&"app.t".parse().unwrap(), &declaration);

        assert_eq!(
            engine.is_ok(),
            found.is_empty(),
            "`{declared}` as an identity: the engine {}, and pbps says {found:?}",
            if engine.is_ok() { "took it" } else { "refused" }
        );
    }

    // The refusals that are not about the type. Each is the engine's, and each
    // is what `validate` says without connecting.
    for (definition, refused) in [
        ("id integer NULL GENERATED ALWAYS AS IDENTITY", "nullable"),
        (
            "id integer DEFAULT 7 GENERATED ALWAYS AS IDENTITY",
            "a default",
        ),
        (
            "id integer GENERATED ALWAYS AS IDENTITY (INCREMENT BY 0)",
            "an increment of zero",
        ),
    ] {
        let engine = conn
            .execute(&format!("CREATE TABLE {table} ({definition})"))
            .await;
        assert!(engine.is_err(), "the engine took {refused}");
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .expect("drop");
    }

    // The seed, which is bounded by the sequence behind the column and not by
    // the column: a `0` fits every one of these types and the engine still
    // refuses it, and counting down inverts which end is too far.
    for (declared, seed, increment) in [
        ("smallint", 1_i64, 1_i64),
        ("smallint", 32767, 1),
        ("smallint", 32768, 1),
        ("integer", 0, 1),
        ("integer", -5, 1),
        ("integer", -5, -1),
        ("integer", 5, -1),
        ("smallint", -32768, -1),
        ("smallint", -32769, -1),
    ] {
        let engine = conn
            .execute(&format!(
                "CREATE TABLE {table} (id {declared} GENERATED ALWAYS AS IDENTITY \
                 (START WITH {seed} INCREMENT BY {increment}))"
            ))
            .await;
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .expect("drop");

        let mut declaration = pbps_model::Table::default();
        let mut column = pbps_model::Column::new(declared.parse::<ColumnType>().expect("parses"));
        column.nullable = false;
        column.identity = Some(pbps_model::Identity { seed, increment });
        declaration.columns.insert("id".to_owned(), column);
        let found = Postgres.validate_table(&"app.t".parse().unwrap(), &declaration);

        assert_eq!(
            engine.is_ok(),
            found.is_empty(),
            "`{declared}` starting at {seed} by {increment}: the engine {}, and pbps says {found:?}",
            if engine.is_ok() { "took it" } else { "refused" }
        );
    }

    // And the rule that is SQL Server's alone: that dialect refuses a second
    // IDENTITY column, this engine takes one per column. `validate` carries no
    // such rule here, and this is what says so.
    conn.execute(&format!(
        "CREATE TABLE {table} (a integer GENERATED ALWAYS AS IDENTITY, \
         b integer GENERATED ALWAYS AS IDENTITY)"
    ))
    .await
    .expect("two identity columns in one table");
    conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .expect("drop");
}

// ---------------------------------------------------------------------------
// Introspection (Phase 5 step 3)
// ---------------------------------------------------------------------------

/// A schema of this test's own.
///
/// The process id keeps two runs against the shared container apart, and the
/// caller's own name keeps the tests in *this* run apart: they run in parallel
/// by default, and `build` drops the schema it is about to create.
fn probe_schema(test: &str) -> String {
    format!("pbps_pull_{}_{test}", std::process::id())
}

async fn build(conn: &mut Conn, schema: &str, body: &str) {
    conn.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .expect("drop the probe schema");
    conn.execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .expect("create the probe schema");
    for statement in body.split(";\n") {
        if statement.trim().is_empty() {
            continue;
        }
        conn.execute(statement)
            .await
            .unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
}

/// A pull, retried while another test in this same run is applying DDL.
///
/// The suite is its own concurrent writer: these tests run in parallel and each
/// builds and drops a schema. A pull taken across another's `DROP` fails by
/// design — `pg_get_constraintdef` reads through the syscache rather than the
/// transaction's snapshot, so the object it is rendering can be gone. That is
/// the guard firing, not a defect, and the assertion here is that it fires with
/// the message that says so rather than as a bare driver error.
async fn pull(conn: &mut Conn) -> pbps_pg::introspect::Pulled {
    for _ in 0..4 {
        match pbps_pg::catalog::introspect(conn).await {
            Ok(pulled) => return pulled,
            Err(e) => {
                assert!(
                    e.to_string()
                        .contains("the catalog changed while it was being read"),
                    "the pull failed for a reason this suite does not cause: {e}"
                );
            }
        }
    }
    panic!("four pulls in a row were taken across another test's DDL");
}

/// The limitations this suite's own schema earned.
///
/// Scoped for the same reason as [`ours`]: the container is shared, and a bare
/// `limitations.is_empty()` asserts something about every other session's
/// tables as well — which is a test that passes or fails on what somebody else
/// left behind.
fn ours_limitations<'a>(
    pulled: &'a pbps_pg::introspect::Pulled,
    schema: &str,
) -> Vec<&'a pbps_pg::introspect::Limitation> {
    pulled
        .limitations
        .iter()
        .filter(|l| l.table.schema == schema)
        .collect()
}

/// Only this suite's own schema, so that another session's tables in the same
/// container cannot decide whether an assertion holds.
fn ours(pulled: &pbps_pg::introspect::Pulled, schema: &str) -> Vec<pbps_model::TableName> {
    let mut names: Vec<_> = pulled
        .schema
        .tables
        .keys()
        .filter(|t| t.schema == schema)
        .cloned()
        .collect();
    names.sort();
    names
}

/// The whole of step 3 in one table: every field the model holds, compared
/// against what was written rather than against what this code believes it
/// wrote.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_table_built_by_hand_reads_back_field_by_field() {
    let mut conn = connect().await;
    let s = probe_schema("fields");
    build(
        &mut conn,
        &s,
        &format!(
            "CREATE TABLE {s}.parent (
                 id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 code varchar(10) NOT NULL,
                 CONSTRAINT parent_code_uq UNIQUE (code)
             );
             CREATE TABLE {s}.child (
                 id bigint NOT NULL,
                 parent_id integer NOT NULL,
                 gone text,
                 label varchar NOT NULL DEFAULT 'x',
                 amount numeric(10,2) DEFAULT 0,
                 kind char(4),
                 note text,
                 CONSTRAINT child_pk PRIMARY KEY (id, parent_id),
                 CONSTRAINT child_amount_ck CHECK (amount >= 0),
                 CONSTRAINT child_fk FOREIGN KEY (parent_id)
                     REFERENCES {s}.parent (id) ON DELETE CASCADE
             );
             ALTER TABLE {s}.child DROP COLUMN gone;
             CREATE INDEX child_note_ix ON {s}.child (note DESC, label)
                 INCLUDE (kind) WHERE note IS NOT NULL"
        ),
    )
    .await;

    let pulled = pull(&mut conn).await;
    assert!(
        ours_limitations(&pulled, &s).is_empty(),
        "nothing here is beyond the model: {:?}",
        ours_limitations(&pulled, &s)
    );
    assert_eq!(
        ours(&pulled, &s),
        [
            pbps_model::TableName::new(&s, "child"),
            pbps_model::TableName::new(&s, "parent"),
        ]
    );

    let parent = &pulled.schema.tables[&pbps_model::TableName::new(&s, "parent")];
    assert_eq!(
        parent.columns["id"].identity,
        Some(pbps_model::Identity {
            seed: 1,
            increment: 1
        })
    );
    assert!(!parent.columns["id"].nullable);
    // The respelling ADR-0009 §2 is about: `varchar(10)` comes back as the
    // engine's own name for it, and the catalogue of step 2 agrees.
    assert_eq!(
        parent.columns["code"].ty.to_string(),
        "character varying(10)"
    );
    assert_eq!(
        parent.primary_key.as_ref().unwrap().name.as_deref(),
        Some("parent_pkey"),
        "an unnamed primary key comes back under the name the engine gave it"
    );
    assert_eq!(parent.unique["parent_code_uq"].columns, ["code"]);
    // The index behind a constraint is that constraint, and not also an index.
    assert!(parent.indexes.is_empty(), "{:?}", parent.indexes);

    let child = &pulled.schema.tables[&pbps_model::TableName::new(&s, "child")];
    // ADR-0012 §6: the dropped column keeps its catalog slot and must not be
    // in the pull — and `attnum` keeps the hole, so everything after it would
    // be off by one if anything read a position.
    assert_eq!(
        child.columns.keys().collect::<Vec<_>>(),
        ["id", "parent_id", "label", "amount", "kind", "note"]
    );
    assert_eq!(
        child.primary_key.as_ref().unwrap().columns,
        ["id", "parent_id"]
    );

    // The three verbatim expressions, exactly as the engine respelled them.
    assert_eq!(
        child.columns["label"].default.as_deref(),
        Some("'x'::character varying"),
        "ADR-0013 §4's welded cast"
    );
    assert_eq!(child.columns["amount"].default.as_deref(), Some("0"));
    assert_eq!(
        child.checks["child_amount_ck"].expression, "(amount >= (0)::numeric)",
        "the expression, not the `CHECK (…)` clause `pg_get_constraintdef` wraps it in"
    );
    let ix = &child.indexes["child_note_ix"];
    assert_eq!(ix.filter.as_deref(), Some("(note IS NOT NULL)"));
    assert_eq!(ix.columns[0].name, "note");
    assert!(ix.columns[0].descending);
    assert_eq!(ix.columns[1].name, "label");
    assert!(!ix.columns[1].descending);
    assert_eq!(ix.include, ["kind"]);
    assert!(!ix.unique);

    let fk = &child.foreign_keys["child_fk"];
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(
        fk.references_table,
        pbps_model::TableName::new(&s, "parent")
    );
    assert_eq!(fk.references_columns, ["id"]);
    assert_eq!(fk.on_delete, pbps_model::ReferentialAction::Cascade);
    assert_eq!(fk.on_update, pbps_model::ReferentialAction::NoAction);

    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The reason ADR-0013 §3 pins the search path, measured rather than asserted.
///
/// The engine renders a name in `pg_get_constraintdef` and `format_type` only
/// as qualified as the path makes necessary, so two operators with different
/// paths would take two different snapshots of one database. The pull must not
/// move when the session does.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_pull_does_not_move_when_the_sessions_search_path_does() {
    let mut conn = connect().await;
    let s = probe_schema("path");
    build(
        &mut conn,
        &s,
        &format!(
            "CREATE TABLE {s}.p (id integer PRIMARY KEY);
             CREATE TABLE {s}.c (pid integer REFERENCES {s}.p (id),
                 born date DEFAULT '2020-01-02'::date,
                 CONSTRAINT c_ck CHECK (pid > 0))"
        ),
    )
    .await;

    // What the engine does under a path that makes the schema visible: the
    // reference loses its qualification.
    conn.execute(&format!("SET search_path TO {s}, public"))
        .await
        .expect("set the path");
    let under_a_path = text(
        &mut conn,
        &format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conrelid = '{s}.c'::regclass AND contype = 'f'"
        ),
    )
    .await;
    assert_eq!(under_a_path, "FOREIGN KEY (pid) REFERENCES p(id)");

    // And the pull, taken from that same session, is the qualified one.
    let pulled = pull(&mut conn).await;
    let fk = &pulled.schema.tables[&pbps_model::TableName::new(&s, "c")].foreign_keys;
    let key = fk.values().next().expect("one foreign key");
    assert_eq!(key.references_table, pbps_model::TableName::new(&s, "p"));
    assert_eq!(key.references_columns, ["id"]);

    // The same question about the settings that decide how a *value* is
    // printed, not how a name is. The expressions are carried verbatim, so a
    // session that prints them differently would manufacture drift.
    for (setting, value) in [
        ("quote_all_identifiers", "on"),
        ("datestyle", "German, DMY"),
        ("timezone", "Asia/Taipei"),
        ("intervalstyle", "sql_standard"),
        ("bytea_output", "escape"),
    ] {
        conn.execute(&format!("SET {setting} TO '{value}'"))
            .await
            .unwrap_or_else(|e| panic!("set {setting}: {e}"));
    }
    // Measured through the session itself: these are not settings the engine
    // ignores.
    let hostile = text(
        &mut conn,
        &format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conrelid = '{s}.c'::regclass AND contype = 'c'"
        ),
    )
    .await;
    assert_eq!(hostile, "CHECK ((\"pid\" > 0))");

    let pulled = pull(&mut conn).await;
    let c = &pulled.schema.tables[&pbps_model::TableName::new(&s, "c")];
    assert_eq!(
        c.checks.values().next().expect("one check").expression,
        "(pid > 0)",
        "the pull is taken under its own canonical scope, not the session's"
    );
    assert_eq!(
        c.columns["born"].default.as_deref(),
        Some("'2020-01-02'::date")
    );
    // And these come back too, for the same reason the path does.
    assert_eq!(
        text(&mut conn, "SELECT current_setting('quote_all_identifiers')").await,
        "on"
    );

    // And the session is handed back exactly as it was found — which is not a
    // restore this code performs but a consequence of the setting being local
    // to the transaction the pull runs in.
    let after = text(&mut conn, "SELECT current_setting('search_path')").await;
    assert_eq!(after, format!("{s}, public"));
    // No transaction is left open either: a pull that returned inside one
    // would hold every lock it took until the caller happened to commit.
    let in_transaction = truth(
        &mut conn,
        "SELECT pg_catalog.txid_current_if_assigned() IS NOT NULL
             OR current_setting('transaction_read_only') = 'on'",
    )
    .await;
    assert!(!in_transaction);

    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// PostgreSQL does not nest transactions, so a pull that ran inside the
/// caller's would commit it — and would not have the snapshot it committed for.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_pull_inside_the_callers_own_transaction_is_refused() {
    let mut conn = connect().await;
    let s = probe_schema("xact");
    build(
        &mut conn,
        &s,
        &format!("CREATE TABLE {s}.kept (id integer)"),
    )
    .await;

    // A transaction with a write in it, which the pull must not commit.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!("CREATE TABLE {s}.uncommitted (id integer)"))
        .await
        .expect("create inside the transaction");

    let e = pbps_pg::catalog::introspect(&mut conn)
        .await
        .expect_err("a pull inside a caller's transaction is refused");
    let message = e.to_string();
    assert!(
        message.contains("already has an open transaction"),
        "{message}"
    );

    // The caller's transaction is still the caller's: rolling it back takes the
    // write with it. A `COMMIT` inside the pull would have made this table
    // permanent, which is the failure the refusal exists for.
    conn.execute("ROLLBACK").await.expect("rollback");
    let survived = truth(
        &mut conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = '{s}' AND c.relname = 'uncommitted')"
        ),
    )
    .await;
    assert!(!survived, "the pull committed the caller's transaction");

    // And the connection is usable afterwards: a refusal is not a broken
    // session.
    let pulled = pull(&mut conn).await;
    assert!(ours(&pulled, &s).contains(&pbps_model::TableName::new(&s, "kept")));

    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// Every kind of object the model cannot hold, in one database, each measured
/// to be **named** rather than missing.
///
/// This is the test the issue asks for above all the others: absent, empty and
/// unreadable are three different things, and a pull that quietly returned the
/// expressible half of this schema would be the failure this tool is most
/// dangerous for.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn what_the_model_cannot_hold_is_named_and_never_silently_dropped() {
    let mut conn = connect().await;
    let s = probe_schema("limits");
    build(
        &mut conn,
        &s,
        &format!(
            "CREATE DOMAIN {s}.money_amount AS numeric(10,2);
             CREATE TABLE {s}.odd (
                 id integer PRIMARY KEY,
                 total integer GENERATED ALWAYS AS (id * 2) STORED,
                 by_default integer GENERATED BY DEFAULT AS IDENTITY,
                 note text
             );
             -- Two spellings the catalogue cannot read, in a table of their
             -- own: they take it out of the pull, and the rest of this fixture
             -- points at `odd`.
             CREATE TABLE {s}.unspellable (
                 id integer,
                 amt {s}.money_amount,
                 stamp timestamp(3) with time zone
             );
             CREATE INDEX odd_gin ON {s}.odd USING gin (to_tsvector('simple', note));
             CREATE INDEX odd_expr ON {s}.odd ((id + 1));
             CREATE INDEX odd_nulls ON {s}.odd (id DESC NULLS LAST);
             ALTER TABLE {s}.odd ADD CONSTRAINT odd_ck CHECK (id > 0) NOT VALID;
             CREATE TABLE {s}.restricted (
                 id integer PRIMARY KEY,
                 oid_ integer REFERENCES {s}.odd (id) ON DELETE RESTRICT
             );
             ALTER TABLE {s}.restricted
                 ADD CONSTRAINT restricted_uq UNIQUE (id) DEFERRABLE INITIALLY DEFERRED;
             CREATE TABLE {s}.unchecked (id integer, oid_ integer);
             ALTER TABLE {s}.unchecked ADD CONSTRAINT unchecked_fk
                 FOREIGN KEY (oid_) REFERENCES {s}.odd (id) NOT VALID;
             CREATE TABLE {s}.serialised (id serial PRIMARY KEY, note text);
             CREATE TABLE {s}.full_match (a integer, b integer, UNIQUE (a, b));
             CREATE TABLE {s}.partly (a integer, b integer,
                 CONSTRAINT partly_fk FOREIGN KEY (a, b)
                     REFERENCES {s}.full_match (a, b) MATCH FULL);
             CREATE TABLE {s}.nulls (a integer);
             CREATE UNIQUE INDEX nulls_nnd ON {s}.nulls (a) NULLS NOT DISTINCT;
             CREATE TABLE {s}.half_built (a integer);
             CREATE INDEX half_built_ix ON {s}.half_built (a);
             -- The state a `CREATE INDEX CONCURRENTLY` that failed leaves
             -- behind. Reached here by writing the catalog directly, because
             -- the honest route is to interrupt a build at the right instant.
             UPDATE pg_catalog.pg_index SET indisvalid = false
                 WHERE indexrelid = '{s}.half_built_ix'::regclass;
             CREATE TABLE {s}.quoted (x integer, \"a)b\" integer, UNIQUE (x, \"a)b\"));
             CREATE TABLE {s}.pointing (x integer, y integer,
                 CONSTRAINT pointing_fk FOREIGN KEY (x, y)
                     REFERENCES {s}.quoted (x, \"a)b\"));
             CREATE TABLE {s}.guarded (id integer PRIMARY KEY);
             ALTER TABLE {s}.guarded ENABLE ROW LEVEL SECURITY;
             CREATE UNLOGGED TABLE {s}.volatile_ (id integer);
             CREATE TABLE {s}.collated (a text, b text COLLATE \"C\",
                 c varchar(20), d varchar(20));
             CREATE INDEX collated_pat ON {s}.collated (a text_pattern_ops);
             -- Measured: no default btree operator class has `opcintype =
             -- varchar`, so an exact-type lookup answers NULL for both of
             -- these — for the ordinary one as much as for the pattern one.
             CREATE INDEX collated_vpat ON {s}.collated (c varchar_pattern_ops);
             CREATE INDEX collated_vplain ON {s}.collated (d);
             CREATE TABLE {s}.bounded (
                 id integer GENERATED ALWAYS AS IDENTITY (MINVALUE 5 MAXVALUE 99 CYCLE));
             CREATE TABLE {s}.cached (
                 id integer GENERATED ALWAYS AS IDENTITY (CACHE 100));
             CREATE TABLE {s}.setnull (a integer, b integer,
                 CONSTRAINT setnull_fk FOREIGN KEY (a) REFERENCES {s}.odd (id)
                     ON DELETE SET NULL (a));
             CREATE TABLE {s}.covering (a integer, b integer,
                 CONSTRAINT covering_pk PRIMARY KEY (a) INCLUDE (b));
             -- The key that uniqueness makes legal. It has to go with it.
             CREATE TABLE {s}.leaning (a integer,
                 CONSTRAINT leaning_fk FOREIGN KEY (a) REFERENCES {s}.covering (a));
             CREATE EXTENSION IF NOT EXISTS btree_gist WITH SCHEMA {s};
             CREATE TABLE {s}.temporal (
                 id integer, valid daterange,
                 CONSTRAINT temporal_pk PRIMARY KEY (id, valid WITHOUT OVERLAPS));
             CREATE TABLE {s}.unenforced (
                 id integer, CONSTRAINT unenforced_ck CHECK (id > 0) NOT ENFORCED);
             CREATE SEQUENCE {s}.loose;
             CREATE TABLE {s}.borrowing (id integer DEFAULT nextval('{s}.loose'));
             CREATE TABLE {s}.published (id integer PRIMARY KEY);
             ALTER TABLE {s}.published REPLICA IDENTITY FULL;
             -- A column name the declaration format cannot write, and a key
             -- that points at the table carrying it.
             CREATE TABLE {s}.dotted (a integer PRIMARY KEY, \"x.y\" integer);
             CREATE TABLE {s}.dotting (a integer,
                 CONSTRAINT dotting_fk FOREIGN KEY (a) REFERENCES {s}.dotted (a));
             CREATE TYPE {s}.shape AS (a integer, b text);
             CREATE TABLE {s}.shaped OF {s}.shape;
             CREATE TABLE {s}.pointed_at (a integer PRIMARY KEY);
             CREATE TABLE {s}.silent (a integer,
                 CONSTRAINT silent_fk FOREIGN KEY (a) REFERENCES {s}.pointed_at (a));
             ALTER TABLE {s}.silent DISABLE TRIGGER ALL;
             CREATE TABLE {s}.rewritten (id integer);
             CREATE RULE rewritten_swallows AS ON INSERT TO {s}.rewritten DO INSTEAD NOTHING;
             CREATE TABLE {s}.stops (
                 id integer, CONSTRAINT stops_ck CHECK (id > 0) NO INHERIT);
             -- The referenced table's own standalone unique index, which the
             -- engine records in the foreign key's `conindid`. It belongs to
             -- `referenced`, not to the key, and it has to survive the pull.
             CREATE TABLE {s}.referenced (a integer);
             CREATE UNIQUE INDEX referenced_uq ON {s}.referenced (a);
             CREATE TABLE {s}.referring (
                 a integer, CONSTRAINT referring_fk FOREIGN KEY (a)
                     REFERENCES {s}.referenced (a));
             CREATE TABLE {s}.parted (id integer, at date) PARTITION BY RANGE (at);
             CREATE TABLE {s}.ancestor (a integer, b integer);
             CREATE TABLE {s}.descendant (c integer) INHERITS ({s}.ancestor);
             CREATE TABLE {s}.__pbps_state (id integer);
             CREATE TABLE {s}.__pbps_lock (id integer);
             -- Not this tool's: nothing refuses this declaration, so hiding it
             -- would report a table that is there as absent.
             CREATE TABLE {s}.__pbps_customers (id integer)"
        ),
    )
    .await;

    let pulled = pull(&mut conn).await;
    // This suite's own schema only. `warnings` covers the whole database, so
    // joining all of them lets another session's table — or this suite's own
    // leftovers from a run that crashed before its `DROP SCHEMA` — satisfy an
    // expectation that the fixture below no longer produces.
    let all = ours_limitations(&pulled, &s)
        .iter()
        .map(|l| l.detail.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Each of these is a fact about the database that the pull cannot carry,
    // and each has to reach the operator.
    for expected in [
        // A type the catalogue of step 2 cannot spell, and a domain, which
        // ADR-0012 §1 refuses to fold into its base type.
        "timestamp(3) with time zone",
        "money_amount",
        // A generated column, and an identity whose kind the model loses.
        "generated column",
        "GENERATED BY DEFAULT AS IDENTITY",
        // An index method, an expression index, and a null ordering that is
        // not its direction's default.
        "gin",
        "over an expression",
        "NULLS LAST",
        // A check that has never been checked.
        "NOT VALID",
        // An action the model has no word for.
        "RESTRICT",
        // A table of a kind the model does not hold at all — one whose
        // `relkind` says so, and one whose does not.
        "partitioned table",
        "inherits from another",
        // A key whose check can be put off to the end of the transaction.
        "DEFERRABLE INITIALLY DEFERRED",
        // A composite key that refuses a partly-null row where the default
        // accepts it.
        "MATCH FULL",
        // A unique index that admits at most one null key.
        "NULLS NOT DISTINCT",
        // An index the planner will not use.
        "indisvalid = false",
        // A table whose rows are not all a reader's to see.
        "row-level security",
        // A table that does not survive a crash.
        "UNLOGGED",
        // A collation that decides which values compare equal. Named down to
        // the column and out to the collation itself: the operator-class
        // message says `COLLATE` too, and the table is named schema-qualified,
        // so a bare "`collated`" matches nothing.
        ".collated`.`b` is `COLLATE \"C\"`",
        // An identity that runs out, or wraps, somewhere else.
        "and cycles",
        // A `SET NULL` that names its columns.
        "ON DELETE SET",
        // An operator class that is not the column's own — on `text`, where
        // the type has a default operator class of its own, and on `varchar`,
        // where it does not and the engine resolves one through a preferred
        // type it is binary coercible to.
        "operator class",
        "`collated_vpat` on",
        // A sequence that hands out values in blocks.
        "`CACHE 100`",
        // A key that asks about ranges, wearing an ordinary key's `contype`.
        // Named down to the phrase: the fixture's table is called `temporal`
        // too, so the bare word is satisfied by any warning about it.
        "is temporal — `WITHOUT OVERLAPS`",
        // A constraint the engine records and never checks. Not `NOT VALID`,
        // which does check every new row — the two are one field apart in the
        // catalog and opposite in what they promise. Named down to the
        // constraint, because the definition it quotes says `NOT ENFORCED` too.
        "constraint `unenforced_ck` on",
        // A check that reaches this table's rows and no child's.
        "`stops_ck` on",
        // A default over a sequence nobody in the pull owns.
        "which it does not own",
        // A table whose old rows reach logical replication differently.
        "REPLICA IDENTITY FULL",
        // A table where an `INSERT` does whatever a rule says instead.
        "rewrite rules",
        // A table whose row shape follows a composite type.
        "composite type",
        // A column name that does not survive the declaration format, and the
        // key that pointed at its table.
        "cannot write back",
        "`dotting_fk`",
        // A foreign key whose triggers are not running, while the catalog
        // still calls it validated and enforced.
        "ordinary enable mode",
        // A key constraint whose index covers more than its key, and the
        // foreign key that has nothing to point at once it is gone.
        "INCLUDE",
        "`leaning_fk` on",
        // The sequence a `serial` owns and this model cannot hold.
        "serialised`.`id` defaults from the sequence",
        // A check the rows already there were never checked against, and a
        // foreign key likewise — the constraint's own name, because `NOT
        // VALID` on its own is satisfied by either of them.
        "check constraint `odd_ck`",
        "foreign key `unchecked_fk` on",
        // Measured: a generated column's expression is in `pg_attrdef`, where
        // an ordinary default lives, so the warning has to name it.
        "(id * 2)",
    ] {
        assert!(
            all.contains(expected),
            "no warning mentions `{expected}`:\n{all}"
        );
    }

    // The two that must be left out rather than read back wrong.
    let odd = &pulled.schema.tables[&pbps_model::TableName::new(&s, "odd")];
    assert!(
        !odd.indexes.contains_key("odd_gin") && !odd.indexes.contains_key("odd_expr"),
        "{:?}",
        odd.indexes
    );
    assert!(
        odd.indexes.contains_key("odd_nulls"),
        "an index the model holds all but one property of is still read back"
    );
    // The other side of that resolution: an ordinary index on a `varchar`
    // column has no exact-type default operator class either, and it is not a
    // limitation. A rule that reported every `varchar` index would be silence
    // of a different kind — a warning nobody reads.
    let collated = &pulled.schema.tables[&pbps_model::TableName::new(&s, "collated")];
    assert!(
        collated.indexes.contains_key("collated_vplain")
            && !collated.indexes.contains_key("collated_vpat")
            && !collated.indexes.contains_key("collated_pat"),
        "{:?}",
        collated.indexes
    );

    // A foreign key's backing index is the *referenced* table's, and skipping
    // it would drop the only uniqueness the key is legal against — a pull that
    // compares clean and describes a schema that cannot be built.
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "referenced")]
            .indexes
            .contains_key("referenced_uq"),
        "{:?}",
        pulled.schema.tables[&pbps_model::TableName::new(&s, "referenced")].indexes
    );
    assert_eq!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "referring")]
            .foreign_keys
            .len(),
        1
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "temporal")]
            .primary_key
            .is_none(),
        "a temporal key is not an ordinary one"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "unenforced")]
            .checks
            .is_empty(),
        "a check the engine never applies is not a check"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "leaning")]
            .foreign_keys
            .is_empty(),
        "a key against a uniqueness the pull left out is not a key"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "dotting")]
            .foreign_keys
            .is_empty(),
        "a key pointing at a table the pull refused is not a key"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "silent")]
            .foreign_keys
            .is_empty(),
        "a foreign key whose triggers are not running is not a foreign key"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "stops")]
            .checks
            .is_empty(),
        "a check that stops at this table is not the check a plan would write"
    );

    let restricted = &pulled.schema.tables[&pbps_model::TableName::new(&s, "restricted")];
    assert!(
        restricted.foreign_keys.is_empty(),
        "{:?}",
        restricted.foreign_keys
    );
    assert!(
        restricted.unique.is_empty(),
        "a deferrable key is a property of the object, so the object is left out: {:?}",
        restricted.unique
    );
    // And the other side of that line: `NOT VALID` is about the rows already
    // there, not about the key, so the key is carried and the fact is named.
    let unchecked = &pulled.schema.tables[&pbps_model::TableName::new(&s, "unchecked")];
    assert!(unchecked.foreign_keys.contains_key("unchecked_fk"));
    // A `MATCH FULL` key and a `NULLS NOT DISTINCT` index are properties of
    // the object, so the objects are left out.
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "partly")]
            .foreign_keys
            .is_empty()
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "nulls")]
            .indexes
            .is_empty()
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "half_built")]
            .indexes
            .is_empty(),
        "an index the planner will not use is not an index"
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "setnull")]
            .foreign_keys
            .is_empty()
    );
    assert!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "covering")]
            .primary_key
            .is_none(),
        "a key constraint whose index covers more than its key is left out"
    );
    // A collated column is carried — the type is what the model says — and the
    // collation beside it is named.
    assert_eq!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "collated")].columns["b"]
            .ty
            .to_string(),
        "text"
    );
    // A `serial`'s column is carried — an `integer` with a `nextval` default,
    // which is exactly what the model says — and the sequence beside it named.
    let serialised = &pulled.schema.tables[&pbps_model::TableName::new(&s, "serialised")];
    assert_eq!(serialised.columns["id"].ty.to_string(), "integer");
    assert!(
        serialised.columns["id"]
            .default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval(")),
        "{:?}",
        serialised.columns["id"].default
    );
    // And a referenced column whose name contains the `)` that a parse of
    // `pg_get_constraintdef` would have stopped inside of.
    let pointing = &pulled.schema.tables[&pbps_model::TableName::new(&s, "pointing")];
    assert_eq!(
        pointing.foreign_keys["pointing_fk"].references_columns,
        ["x", "a)b"]
    );
    // A generated column's expression shares `pg_attrdef` with the defaults,
    // and reading it back as one would make a plan that computes it once.
    assert_eq!(
        pulled.schema.tables[&pbps_model::TableName::new(&s, "odd")].columns["total"].default,
        None
    );

    // The partitioned table is not in the pull at all — and this is why the
    // warning above matters, because otherwise it reads as a table to create.
    // This tool's own tables are not in it either.
    assert_eq!(
        ours(&pulled, &s),
        [
            // Not this tool's table: the filter that keeps `__pbps_state` and
            // `__pbps_lock` out names them, and a prefix would have reported a
            // project's own table as absent.
            pbps_model::TableName::new(&s, "__pbps_customers"),
            // The inheritance parent is an ordinary table and stays.
            pbps_model::TableName::new(&s, "ancestor"),
            pbps_model::TableName::new(&s, "borrowing"),
            pbps_model::TableName::new(&s, "bounded"),
            pbps_model::TableName::new(&s, "cached"),
            pbps_model::TableName::new(&s, "collated"),
            pbps_model::TableName::new(&s, "covering"),
            pbps_model::TableName::new(&s, "dotting"),
            pbps_model::TableName::new(&s, "full_match"),
            pbps_model::TableName::new(&s, "half_built"),
            pbps_model::TableName::new(&s, "leaning"),
            pbps_model::TableName::new(&s, "nulls"),
            pbps_model::TableName::new(&s, "odd"),
            pbps_model::TableName::new(&s, "partly"),
            pbps_model::TableName::new(&s, "pointed_at"),
            pbps_model::TableName::new(&s, "pointing"),
            pbps_model::TableName::new(&s, "quoted"),
            pbps_model::TableName::new(&s, "referenced"),
            pbps_model::TableName::new(&s, "referring"),
            pbps_model::TableName::new(&s, "restricted"),
            pbps_model::TableName::new(&s, "serialised"),
            pbps_model::TableName::new(&s, "setnull"),
            pbps_model::TableName::new(&s, "silent"),
            pbps_model::TableName::new(&s, "stops"),
            pbps_model::TableName::new(&s, "temporal"),
            pbps_model::TableName::new(&s, "unchecked"),
            pbps_model::TableName::new(&s, "unenforced"),
        ],
        "the partitioned table, the inheritance child, the row-level-secured \
         table, the UNLOGGED table and this tool's own tables are not in the \
         pull"
    );

    // Every limitation names its table, so a caller can tell drift inside the
    // managed set from a fact about somebody else's — and the only tables a
    // limitation names that are *not* in the pull are the ones the pull refused
    // whole. "Absent" and "named but left out" are the two answers that must
    // not be confused; a table missing from both lists would be silence.
    let mut refused: Vec<_> = ours_limitations(&pulled, &s)
        .iter()
        .map(|l| l.table.clone())
        .filter(|t| !pulled.schema.tables.contains_key(t))
        .collect();
    refused.sort();
    refused.dedup();
    assert_eq!(
        refused,
        vec![
            pbps_model::TableName::new(&s, "descendant"),
            pbps_model::TableName::new(&s, "dotted"),
            pbps_model::TableName::new(&s, "guarded"),
            pbps_model::TableName::new(&s, "parted"),
            pbps_model::TableName::new(&s, "published"),
            pbps_model::TableName::new(&s, "rewritten"),
            pbps_model::TableName::new(&s, "shaped"),
            pbps_model::TableName::new(&s, "unspellable"),
            pbps_model::TableName::new(&s, "volatile_"),
        ],
        "a limitation about a table that is in the pull must name it, and the \
         tables left out whole must each earn one"
    );

    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

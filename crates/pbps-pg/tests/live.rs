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
        Postgres::new().normalize_definition("SELECT $tag$a  b$tag$"),
        Postgres::new().normalize_definition("SELECT $tag$a b$tag$"),
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
        Postgres::new().normalize_definition(r"SELECT E'it\'s  here'"),
        Postgres::new().normalize_definition(r"SELECT E'it\'s here'"),
    );

    // In a plain literal the backslash is only a backslash and the quote after
    // it *does* close, which is what makes the rule above specific to `E'…'`.
    // `standard_conforming_strings` is `on` by default and this is what it means.
    assert!(truth(&mut conn, r"SELECT 'a\' = chr(97) || chr(92)").await);

    // Block comments nest here as they do in T-SQL, so the scanner may leave
    // one only at the `*/` that matches its opener.
    assert_eq!(number(&mut conn, "SELECT /* a /* b */ c */ 1").await, 1);
    assert_ne!(
        Postgres::new().normalize_definition("SELECT /* outer /* inner */ it's here */ 'a  b'"),
        Postgres::new().normalize_definition("SELECT /* outer /* inner */ it's here */ 'a b'"),
    );

    // A `[` is a subscript, not a quote: a reindent inside one is not a change.
    assert_eq!(number(&mut conn, "SELECT (ARRAY[1, 2])[1  +  1]").await, 2);
    assert_eq!(
        Postgres::new().normalize_definition("SELECT a[1  +  2] FROM t"),
        Postgres::new().normalize_definition("SELECT a[1 + 2] FROM t"),
    );

    // And `a$b$c` is one identifier, which is why a `$` that continues a name
    // opens nothing.
    assert_eq!(number(&mut conn, "SELECT 1 AS a$b$c").await, 1);
    assert_eq!(
        Postgres::new().normalize_definition("SELECT a$b$c  ,  d FROM t"),
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
        let message = Postgres::new()
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
        Postgres::new().fold_ident(&declared),
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
    assert!(Postgres::new().quote_ident(&long).is_err());
    assert!(Postgres::new().quote_ident(&long[..63]).is_ok());
    assert!(Postgres::new().quote_ident(&"ä".repeat(32)).is_err());
    assert!(Postgres::new().quote_ident(&"ä".repeat(31)).is_ok());
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
        let normalized = Postgres::new()
            .normalize_type(&spelling.parse().expect("a type parses"))
            .unwrap_or_else(|e| panic!("`{spelling}` should normalize: {e}"));
        assert_eq!(
            normalized, engine,
            "declared `{spelling}`: the dialect says `{normalized}` and the engine says \
             `{read_back}`, so every run would report a change that is not there"
        );
        // And the contract's other half, against the engine's own spelling
        // rather than against this crate's idea of it.
        let again = Postgres::new().normalize_type(&engine).expect("idempotent");
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
                Postgres::new()
                    .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
                    .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
            };
            let judged = Postgres::new().type_change_risk(&normalize(from), &normalize(to));
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

    let refusal = Postgres::new()
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
            Postgres::new()
                .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
                .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
        };
        if Postgres::new().type_change_risk(&normalize(from), &normalize(to))
            == TypeChangeRisk::Safe
        {
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
        Postgres::new()
            .normalize_type(&s.parse::<ColumnType>().expect("a type parses"))
            .expect("normalizes")
    };
    assert_eq!(
        Postgres::new().type_change_risk(&normalize("numeric(2,1)"), &normalize("real")),
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
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &declaration);

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
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &declaration);

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
    drop_schema(conn, schema).await;
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
    // Twenty, with a wait between them, and the number is not arbitrary: four
    // in a row with no wait was enough while step 3 was the only thing building
    // schemas here, and step 4's tests each build and drop one of their own. A
    // retry budget sized to the suite as it was is a budget that expires the
    // next time the suite grows, and it expires as a failure that reads like a
    // defect in the pull.
    for _ in 0..20 {
        match pbps_pg::catalog::introspect(conn).await {
            Ok(pulled) => return pulled,
            Err(e) => {
                assert!(
                    e.to_string()
                        .contains("the catalog changed while it was being read"),
                    "the pull failed for a reason this suite does not cause: {e:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    panic!("twenty pulls in a row were taken across another test's DDL");
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

    drop_schema(&mut conn, &s).await;
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

    drop_schema(&mut conn, &s).await;
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

    drop_schema(&mut conn, &s).await;
}

/// Two settings that decide how the pull's *own SQL* is read, rather than how
/// the answer is printed: the string-literal mode a `LIKE` pattern is parsed
/// under, and a schema whose name that pattern can swallow.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_schema_a_broken_pattern_would_swallow_is_in_the_pull() {
    let mut conn = connect().await;
    // Not under the probe schema: the name has to begin `pg`, which is what a
    // pattern of `pg_%` — what `'pg\_%'` becomes when the engine eats the
    // backslash — matches with its `_` acting as a wildcard.
    let s = format!("pga_{}", std::process::id());
    build(&mut conn, &s, &format!("CREATE TABLE {s}.t (id integer)")).await;

    // Measured: with this off the engine consumes the backslash and warns,
    // and nothing in the driver reads that warning.
    conn.execute("SET standard_conforming_strings = off")
        .await
        .expect("set the literal mode");
    let swallowed = truth(
        &mut conn,
        &format!("SELECT '{s}' LIKE 'pg\\_%' AS swallowed"),
    )
    .await;
    assert!(
        swallowed,
        "the session really does read the pattern that way"
    );

    let pulled = pull(&mut conn).await;
    assert!(
        ours(&pulled, &s).contains(&pbps_model::TableName::new(&s, "t")),
        "a project schema is not the engine's bookkeeping: {:?}",
        ours(&pulled, &s)
    );

    drop_schema(&mut conn, &s).await;
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
                 stamp timestamp(3) with time zone,
                 -- The one that parses back as a different value rather than
                 -- failing to parse at all.
                 flags bit(3)
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
             -- A policy with the switch off does nothing today, which is the
             -- trap: a rebuild drops it, and turning the switch on afterwards
             -- gives a table with no policies at all.
             CREATE TABLE {s}.dormant (id integer);
             CREATE POLICY dormant_p ON {s}.dormant USING (id > 0);
             -- And `FORCE` is its own flag, which a table can carry with
             -- row-level security not enabled.
             CREATE TABLE {s}.forced (id integer);
             ALTER TABLE {s}.forced FORCE ROW LEVEL SECURITY;
             CREATE UNLOGGED TABLE {s}.volatile_ (id integer);
             CREATE TABLE {s}.collated (a text, b text COLLATE \"C\",
                 c varchar(20), d varchar(20));
             CREATE INDEX collated_pat ON {s}.collated (a text_pattern_ops);
             -- Measured: no default btree operator class has `opcintype =
             -- varchar`, so an exact-type lookup answers NULL for both of
             -- these — for the ordinary one as much as for the pattern one.
             CREATE INDEX collated_vpat ON {s}.collated (c varchar_pattern_ops);
             CREATE INDEX collated_vplain ON {s}.collated (d);
             -- Measured: a column that is already an identity may also own a
             -- second sequence, and then `pg_depend` has both an `i` row and
             -- an `a` row for it. One join returned the column twice and took
             -- the seed from whichever came back first.
             CREATE TABLE {s}.twinned (
                 id integer GENERATED ALWAYS AS IDENTITY (START WITH 7 INCREMENT BY 3));
             CREATE SEQUENCE {s}.spare START 900 INCREMENT 11;
             ALTER SEQUENCE {s}.spare OWNED BY {s}.twinned.id;
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
             -- Two module-shaped objects the model does not hold, owned by an
             -- extension. `ALTER EXTENSION … ADD` is how the fixture is built
             -- rather than an extension that ships them: what the reader keys
             -- on is the `pg_depend deptype = 'e'` edge, and this makes one.
             -- Here rather than in a test of its own because `CREATE
             -- EXTENSION` is expensive and a second one running beside this
             -- one deadlocked the suite: four other tests came back `40P01`.
             --
             -- The extension has to be one this schema owns. Measured, a
             -- `DROP SCHEMA … CASCADE` over a schema holding a member drops
             -- the *extension* — attaching these to `plpgsql` took plpgsql and
             -- every plpgsql function in the database with the schema.
             CREATE MATERIALIZED VIEW {s}.theirs_mv AS SELECT id FROM {s}.odd;
             CREATE AGGREGATE {s}.theirs_agg(integer) (sfunc = int4pl, stype = integer);
             ALTER EXTENSION btree_gist ADD MATERIALIZED VIEW {s}.theirs_mv;
             ALTER EXTENSION btree_gist ADD AGGREGATE {s}.theirs_agg(integer);
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
        // ADR-0012 §1 refuses to fold into its base type. And `bit(3)`, which
        // parses on the way back into a type that is not the one written out.
        "timestamp(3) with time zone",
        "money_amount",
        "`flags` (`bit(3)`)",
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
        // And the other end of it, which is no more an ordinary table.
        "whose reads return their rows",
        // A key whose check can be put off to the end of the transaction.
        "DEFERRABLE INITIALLY DEFERRED",
        // A composite key that refuses a partly-null row where the default
        // accepts it.
        "MATCH FULL",
        // A unique index that admits at most one null key.
        "NULLS NOT DISTINCT",
        // An index the planner will not use.
        "indisvalid = false",
        // A table whose rows are not all a reader's to see, one whose
        // policies are not in force, and one whose owner is not exempt.
        "row-level security",
        "not in force",
        "FORCE ROW LEVEL SECURITY",
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
        // The sequence a column owns without defaulting from it.
        "`spare`",
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

    // An extension's own objects are left out **silently** (DECISIONS 305),
    // and that holds for the limitation reader as much as for the ordinary
    // one. Reported, they would be worse than noise: `managed_limitations`
    // refuses every command for a limitation whose name is in the managed set,
    // so an extension object colliding with a declared name would refuse a
    // plan that is correct. A materialized view and an aggregate are the two
    // kinds the unheld reader reports, so they are the two this checks.
    for theirs in ["theirs_mv", "theirs_agg"] {
        assert!(
            !all.contains(theirs),
            "`{s}.{theirs}` belongs to an extension and is nobody's declaration:\n{all}"
        );
    }
    // And the extension's objects are not read back as this project's either,
    // which is the same rule on the other reader.
    assert!(
        !pulled
            .schema
            .modules
            .keys()
            .any(|id| id.schema() == s && id.name().starts_with("theirs")),
        "{:?}",
        pulled.schema.modules.keys().collect::<Vec<_>>()
    );

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

    // One row per column, and each fact from the dependency that means it.
    let twinned = &pulled.schema.tables[&pbps_model::TableName::new(&s, "twinned")];
    assert_eq!(twinned.columns.len(), 1, "{:?}", twinned.columns);
    let identity = twinned.columns["id"]
        .identity
        .as_ref()
        .expect("the identity is read");
    assert_eq!(
        (identity.seed, identity.increment),
        (7, 3),
        "the identity's own sequence, not the one merely owned by the column"
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
            pbps_model::TableName::new(&s, "twinned"),
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
            pbps_model::TableName::new(&s, "ancestor"),
            pbps_model::TableName::new(&s, "descendant"),
            pbps_model::TableName::new(&s, "dormant"),
            pbps_model::TableName::new(&s, "dotted"),
            pbps_model::TableName::new(&s, "forced"),
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

    drop_schema(&mut conn, &s).await;
}

// ---------------------------------------------------------------------------
// The emitter (Phase 5 step 4)
// ---------------------------------------------------------------------------

use pbps_model::{
    CheckConstraint, Column, ForeignKey, Identity, IdsFile, Index, IndexColumn, Intent, PrimaryKey,
    ReferentialAction, Schema, Strategy, Table, TableName, UniqueConstraint,
};

/// A schema of this step's own, kept apart from step 3's by its prefix.
fn emit_schema(test: &str) -> String {
    format!("pbps_emit_{}_{test}", std::process::id())
}

async fn fresh(conn: &mut Conn, schema: &str) {
    drop_schema(conn, schema).await;
    conn.execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .expect("create the emit schema");
}

/// The `SQLSTATE` of a refusal, which is what this suite asserts on.
///
/// Not the message: `tokio-postgres`'s own `Display` is the two words `db
/// error` and everything a reader wants is in its source chain (issue #167), so
/// a test that matched on text would be asserting nothing. The code is carried
/// through the seam as text because `42P01` has a letter in it (DECISIONS 193).
fn sqlstate(e: &DbError) -> &str {
    match e {
        DbError::Driver { code, .. } => code.as_deref().unwrap_or("no code"),
        // Named rather than wildcarded, for the reason the connection-failure
        // test names them: a variant added later has to be looked at here.
        other @ (DbError::BadConnectionString(_)
        | DbError::Connect { .. }
        | DbError::ConnectTimeout { .. }
        | DbError::WrongSession { .. }
        | DbError::BadRow(_)) => panic!("not a refusal from the server: {other:?}"),
    }
}

/// Emits and executes every change of a plan, in plan order.
///
/// One statement at a time through [`Conn::execute`], which is what `apply`
/// does: each emitted statement is one batch, and the write scope it carries
/// only reaches the batch it is in.
async fn apply(conn: &mut Conn, pg: &Postgres, cs: &pbps_model::ChangeSet) {
    for p in &cs.changes {
        for stmt in pg.emit(&p.change, p.strategy).expect("emit") {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("the engine rejected:\n{}\n{e}", stmt.sql));
        }
    }
}

fn ctx() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "live-test".into(),
        today: "2026-09-07".into(),
    }
}

fn mint_ids(schema: &Schema, base: &IdsFile, intents: &[Intent]) -> IdsFile {
    pbps_diff::resolve(schema, base, intents, &ctx())
        .expect("resolve")
        .ids
}

fn plan(
    base_schema: &Schema,
    base_ids: &IdsFile,
    declared: &Schema,
    declared_ids: &IdsFile,
) -> pbps_model::ChangeSet {
    pbps_diff::diff(
        pbps_diff::Side {
            schema: base_schema,
            ids: base_ids,
        },
        pbps_diff::Side {
            schema: declared,
            ids: declared_ids,
        },
        &Postgres::new(),
        &pbps_model::Hints::default(),
    )
    .expect("diff")
}

/// The declared side normalized the way introspection reports it, so the two
/// can be compared with `==`.
fn normalized(schema: &Schema) -> Schema {
    let mut s = schema.clone();
    for table in s.tables.values_mut() {
        for col in table.columns.values_mut() {
            col.ty = Postgres::new().normalize_type(&col.ty).expect("normalize");
        }
    }
    s
}

/// Only this test's own schema, as a [`Schema`] that can be compared whole.
///
/// The container is shared with every other test in this run and with other
/// sessions, so a bare comparison against `pulled.schema` asserts something
/// about tables nobody here created.
fn ours_only(pulled: &pbps_pg::introspect::Pulled, schema: &str) -> Schema {
    let mut out = Schema::default();
    for (name, table) in &pulled.schema.tables {
        if name.schema == schema {
            out.tables.insert(name.clone(), table.clone());
        }
    }
    out
}

fn ty(s: &str) -> ColumnType {
    s.parse().expect("a type parses")
}

/// A schema using every construct the emitter can spell, in the spelling the
/// engine stores.
///
/// The expressions are written the way `pg_get_expr` reads them back — casts
/// welded on, parentheses and all (ADR-0013 §4) — for the reason the SQL Server
/// suite writes `[amount]>=(0)`: this asserts that emit and introspection agree,
/// and a declaration in a *different* spelling would be asserting that the
/// engine respells nothing, which it does.
fn rich_schema(s: &str) -> Schema {
    let mut region = Table::default();
    region
        .columns
        .insert("region_id".into(), Column::new(ty("integer")).not_null());
    // A *string* default, in the spelling the engine reads it back in.
    // Measured, this engine welds a cast onto every one of them — `'unnamed'`
    // becomes `'unnamed'::character varying` — so a fixture without one would
    // never exercise the shape that made the SQL Server plan restate a default
    // on every run.
    let mut region_name = Column::new(ty("varchar(100)")).not_null();
    region_name.default = Some("'unnamed'::character varying".into());
    region.columns.insert("name".into(), region_name);
    region.primary_key = Some(PrimaryKey {
        name: Some("pk_region".into()),
        columns: vec!["region_id".into()],
    });

    let mut customer = Table::default();
    let mut id = Column::new(ty("bigint")).not_null();
    id.identity = Some(Identity {
        seed: 1,
        increment: 1,
    });
    customer.columns.insert("id".into(), id);
    customer
        .columns
        .insert("email".into(), Column::new(ty("varchar(255)")));
    let mut status = Column::new(ty("smallint")).not_null();
    status.default = Some("0".into());
    customer.columns.insert("status".into(), status);
    let mut amount = Column::new(ty("numeric(18,2)")).not_null();
    amount.default = Some("0.00".into());
    customer.columns.insert("amount".into(), amount);
    customer
        .columns
        .insert("notes".into(), Column::new(ty("text")));
    customer.columns.insert(
        "created_at".into(),
        Column::new(ty("timestamptz")).not_null(),
    );
    customer
        .columns
        .insert("region_id".into(), Column::new(ty("integer")));
    customer.primary_key = Some(PrimaryKey {
        name: Some("pk_customer".into()),
        columns: vec!["id".into()],
    });
    customer.unique.insert(
        "uq_customer_email".into(),
        UniqueConstraint {
            columns: vec!["email".into()],
        },
    );
    customer.foreign_keys.insert(
        "fk_customer_region".into(),
        ForeignKey {
            columns: vec!["region_id".into()],
            references_table: TableName::new(s, "region"),
            references_columns: vec!["region_id".into()],
            on_delete: ReferentialAction::SetNull,
            on_update: ReferentialAction::NoAction,
        },
    );
    customer.checks.insert(
        "ck_customer_amount".into(),
        CheckConstraint {
            expression: "(amount >= (0)::numeric)".into(),
        },
    );
    customer.indexes.insert(
        "ix_customer_region".into(),
        Index {
            columns: vec![
                IndexColumn {
                    name: "region_id".into(),
                    descending: false,
                },
                IndexColumn {
                    name: "created_at".into(),
                    descending: true,
                },
            ],
            include: vec!["status".into()],
            unique: false,
            filter: Some("(region_id IS NOT NULL)".into()),
        },
    );

    let mut schema = Schema::default();
    schema.tables.insert(TableName::new(s, "region"), region);
    schema
        .tables
        .insert(TableName::new(s, "customer"), customer);
    schema
}

/// SPEC §11.5 invariant 2, and the first of issue #79's three named live tests:
/// a plan that *creates* a table with a foreign key, applied, read back, and
/// equal to what was declared.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_created_table_with_a_foreign_key_reads_back_as_declared() {
    let s = emit_schema("bootstrap");
    let declared = rich_schema(&s);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::CreateTable { .. })),
        "the plan has to create the tables it is about to read back"
    );

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(&mut conn, &Postgres::new(), &cs).await;

    let pulled = pull(&mut conn).await;
    let ours = ours_only(&pulled, &s);
    let limitations = ours_limitations(&pulled, &s);
    // The property behind the `USING heap` this emitter writes: the reader
    // accepts a table only when its access method is heap, so a table created
    // under any other one is created successfully and then read back as an
    // unsupported object. The clause says it in the statement, where no
    // session and no rendered script can answer differently.
    let methods = text(
        &mut conn,
        &format!(
            "SELECT string_agg(DISTINCT am.amname, ',') FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid = c.relam \
             WHERE n.nspname = '{s}' AND c.relkind = 'r'"
        ),
    )
    .await;
    drop_schema(&mut conn, &s).await;

    assert!(
        limitations.is_empty(),
        "what this emitter writes must read back whole: {limitations:?}"
    );
    assert_eq!(ours, normalized(&declared));
    assert_eq!(methods, "heap");
}

/// Issue #79's third named live test: a bootstrap, and the plan straight after
/// it, which must be empty.
///
/// On SQL Server this exact sequence restated a default, dropped and re-added a
/// check and rebuilt a filtered index on every run, because the engine respells
/// every expression it stores. Here the declaration is written in the stored
/// spelling, which is the same fixpoint from the other side: what the emitter
/// writes is what the pull returns, so the differ sees nothing.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_plan_straight_after_a_bootstrap_is_empty() {
    let s = emit_schema("fixpoint");
    let declared = rich_schema(&s);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(
        &mut conn,
        &Postgres::new(),
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let pulled = pull(&mut conn).await;
    let ours = ours_only(&pulled, &s);
    drop_schema(&mut conn, &s).await;

    let again = plan(&ours, &ids, &declared, &ids);
    assert!(
        again.is_empty(),
        "the plan straight after a bootstrap must be empty: {again:#?}"
    );
}

/// Issue #79's second named live test, and SPEC §11.5 invariant 3: a table that
/// is already there gains a column, a key, a unique, an index and a foreign
/// key, and the plan that does it is computed against the *introspected* state,
/// exactly as `plan --db` computes one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_table_already_there_gains_a_column_a_key_a_unique_an_index_and_a_foreign_key() {
    let s = emit_schema("converge");
    let mut a = Schema::default();
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("varchar(10)")).not_null());
    parent.primary_key = Some(PrimaryKey {
        name: Some("pk_parent".into()),
        columns: vec!["code".into()],
    });
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    a.tables.insert(TableName::new(&s, "parent"), parent);
    a.tables.insert(TableName::new(&s, "child"), child);
    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(
        &mut conn,
        &Postgres::new(),
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    let state_a = ours_only(&pull(&mut conn).await, &s);
    assert_eq!(
        state_a,
        normalized(&a),
        "precondition: A is what was written"
    );

    // B: the child gains a column, a primary key, a unique constraint, an
    // index and a foreign key into the parent.
    let mut b = a.clone();
    {
        let child = b.tables.get_mut(&TableName::new(&s, "child")).unwrap();
        child
            .columns
            .insert("parent_code".into(), Column::new(ty("varchar(10)")));
        child.primary_key = Some(PrimaryKey {
            name: Some("pk_child".into()),
            columns: vec!["id".into()],
        });
        child.unique.insert(
            "uq_child_parent".into(),
            UniqueConstraint {
                columns: vec!["parent_code".into()],
            },
        );
        child.indexes.insert(
            "ix_child_parent".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "parent_code".into(),
                    descending: true,
                }],
                include: vec![],
                unique: false,
                filter: None,
            },
        );
        child.foreign_keys.insert(
            "fk_child_parent".into(),
            ForeignKey {
                columns: vec!["parent_code".into()],
                references_table: TableName::new(&s, "parent"),
                references_columns: vec!["code".into()],
                on_delete: ReferentialAction::Cascade,
                on_update: ReferentialAction::NoAction,
            },
        );
    }
    let ids_b = mint_ids(&b, &ids_a, &[]);
    let migration = plan(&state_a, &ids_a, &b, &ids_b);
    let kinds: Vec<_> = migration
        .changes
        .iter()
        .map(|p| std::mem::discriminant(&p.change))
        .collect();
    assert_eq!(kinds.len(), 5, "one change each: {migration:#?}");

    apply(&mut conn, &Postgres::new(), &migration).await;
    let pulled = pull(&mut conn).await;
    let state_b = ours_only(&pulled, &s);
    let limitations = ours_limitations(&pulled, &s);
    drop_schema(&mut conn, &s).await;

    assert!(limitations.is_empty(), "{limitations:?}");
    assert_eq!(state_b, normalized(&b));
    let again = plan(&state_b, &ids_b, &b, &ids_b);
    assert!(again.is_empty(), "the plan after convergence: {again:#?}");
}

/// The write `search_path` is what makes an unqualified name in a declared
/// expression mean what the project means by it (ADR-0013 §3).
///
/// All three of the verbatim expressions this model holds are checked, because
/// all three bind at creation and a rule written around one of them is the
/// narrowing this branch has made before. Without the extras the engine refuses
/// each of them by name, which is the failure the scope prevents — and it is a
/// refusal of a declaration PostgreSQL would accept under the project's own
/// path, not a silent difference.
///
/// The second half measures the limit of "first": `pg_catalog` is searched
/// ahead of every schema the path names, so a project function with a built-in's
/// signature does not win, and only a path that names `pg_catalog` explicitly
/// would let it. DECISIONS 276 says why this dialect leaves it implicit — the
/// repair would let a project *type* shadow a built-in one, and multi-word type
/// names have no qualified spelling for the emitter to defend with.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_unqualified_name_in_a_declared_expression_binds_through_the_write_path() {
    let s = emit_schema("writepath");
    let ext = format!("{s}_ext");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &ext).await;
    conn.execute(&format!(
        "CREATE FUNCTION {ext}.floorish(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1'"
    ))
    .await
    .expect("the helper the expressions call");

    let mut t = Table::default();
    let mut id = Column::new(ty("integer")).not_null();
    id.default = Some("floorish(1)".into());
    t.columns.insert("id".into(), id);
    t.checks.insert(
        "ck_floor".into(),
        CheckConstraint {
            expression: "floorish(id) > 0".into(),
        },
    );
    t.indexes.insert(
        "ix_floor".into(),
        Index {
            columns: vec![IndexColumn {
                name: "id".into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: Some("floorish(id) > 0".into()),
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(TableName::new(&s, "bound"), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);

    // Without the extras: the object's own schema alone, and every one of the
    // three is refused.
    let bare = Postgres::new();
    let mut refusals = Vec::new();
    for p in &cs.changes {
        for stmt in bare.emit(&p.change, p.strategy).expect("emit") {
            if let Err(e) = conn.execute(&stmt.sql).await {
                refusals.push(sqlstate(&e).to_owned());
            }
        }
    }
    // `42883` is `undefined_function`: the `CREATE TABLE` carries the default
    // and cannot find the helper, and the two statements after it cannot find
    // the table the first one did not create (`42P01`).
    assert_eq!(
        refusals,
        vec!["42883", "42P01", "42P01"],
        "the default's own binding is what fails first"
    );

    // With them: accepted, and every expression bound to the helper.
    let pg = Postgres::with_write_path_extras(vec![ext.clone()]);
    apply(&mut conn, &pg, &cs).await;
    let pulled = pull(&mut conn).await;
    let ours = ours_only(&pulled, &s);
    let table = &ours.tables[&TableName::new(&s, "bound")];
    let bindings = format!(
        "{:?} {:?} {:?}",
        table.columns["id"].default,
        table.checks["ck_floor"].expression,
        table.indexes["ix_floor"].filter
    );
    drop_schema(&mut conn, &s).await;
    drop_schema(&mut conn, &ext).await;
    assert_eq!(
        bindings.matches(&format!("{ext}.floorish")).count(),
        3,
        "each of the three had to bind to the helper in the extra schema: {bindings}"
    );

    // What "first" does not reach, measured beside the property above.
    let shadow = emit_schema("shadow");
    fresh(&mut conn, &shadow).await;
    conn.execute(&format!(
        "CREATE FUNCTION {shadow}.lower(text) RETURNS text LANGUAGE sql IMMUTABLE \
         AS $fn$ SELECT 'the project function' $fn$"
    ))
    .await
    .expect("a function with a built-in's signature");
    conn.execute(&format!("SET search_path = {shadow}"))
        .await
        .expect("the path this dialect writes");
    let implicit = text(&mut conn, "SELECT lower('X')").await;
    conn.execute(&format!("SET search_path = {shadow}, pg_catalog"))
        .await
        .expect("the path that would name it");
    let explicit = text(&mut conn, "SELECT lower('X')").await;
    conn.execute("RESET search_path").await.expect("reset");
    drop_schema(&mut conn, &shadow).await;
    assert_eq!(
        implicit, "x",
        "the built-in wins wherever the path omits it"
    );
    assert_eq!(
        explicit, "the project function",
        "and only naming it explicitly would change that"
    );
    // Which is why an extra of that name is refused rather than quietly
    // dropped: the caller asking for it is asking for the second path.
    let named = Postgres::with_write_path_extras(vec!["pg_catalog".into()]);
    let first = &cs.changes[0];
    let refusal = named
        .emit(&first.change, first.strategy)
        .expect_err("a path that lists pg_catalog moves it behind the object's own schema");
    assert!(refusal.to_string().contains("pg_catalog"), "{refusal}");
}

/// A declared expression ending in a comment still runs, in every place this
/// dialect writes one verbatim.
///
/// The emitter's own syntax follows the user's text on the same line — `);`
/// after a check, `,` after a default in a column list, `);` after an index
/// filter — and a line comment swallows whatever is behind it. **Measured**,
/// before the fix: `CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 --
/// reason));` is `syntax error at end of input`, and so is a column list whose
/// default ends in one. So a valid declaration produced a statement that could
/// not run at all, which is why this is a live test: only the engine can say
/// the newline is in the right place, at every site.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_declared_expression_that_ends_in_a_comment_still_runs() {
    let s = emit_schema("trailingcomment");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;

    let mut n = Column::new(ty("integer")).not_null();
    n.default = Some("1 -- the reason for the default".into());
    let mut t = Table::default();
    t.columns.insert("n".into(), n);
    t.checks.insert(
        "ck_n".into(),
        CheckConstraint {
            expression: "n > 0 -- the reason for the check".into(),
        },
    );
    t.indexes.insert(
        "ix_n".into(),
        Index {
            columns: vec![IndexColumn {
                name: "n".into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: Some("n > 0 -- the reason for the filter".into()),
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(TableName::new(&s, "commented"), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let pg = Postgres::new();
    apply(&mut conn, &pg, &cs).await;

    // And a second plan, so the `ALTER … SET DEFAULT` site is exercised too:
    // that one ends the statement with `;` right behind the expression.
    let mut second = declared.clone();
    second
        .tables
        .get_mut(&TableName::new(&s, "commented"))
        .expect("the table")
        .columns
        .get_mut("n")
        .expect("the column")
        .default = Some("2 -- the reason for the new default".into());
    let cs = plan(&declared, &ids, &second, &ids);
    assert!(
        !cs.changes.is_empty(),
        "the second plan has to carry the default change"
    );
    apply(&mut conn, &pg, &cs).await;

    let pulled = pull(&mut conn).await;
    let ours = ours_only(&pulled, &s);
    let table = &ours.tables[&TableName::new(&s, "commented")];
    let read = format!(
        "{:?} {:?} {:?}",
        table.columns["n"].default, table.checks["ck_n"].expression, table.indexes["ix_n"].filter
    );
    drop_schema(&mut conn, &s).await;
    // The engine keeps none of the comments — `pg_get_constraintdef` renders
    // the parsed expression — so what this asserts is that all three arrived
    // and none of them took the syntax behind it away.
    assert!(
        !read.contains("the reason"),
        "the engine does not store the comments: {read}"
    );
    assert!(
        read.contains('2'),
        "the new default has to be there: {read}"
    );
}

/// `standard_conforming_strings` is pinned by the transaction framing, and the
/// pin has to survive the operator having turned it off.
///
/// Measured, and this is why the pin is not in the statement's own batch: a
/// multi-statement simple query is lexed as a whole before any of it runs, so a
/// `SET` in front of the statement it protects protects nothing. Under `off`
/// the engine *accepts* `CHECK (label <> 'it\'s  here')` as one literal while
/// this crate's scanner closes it at the escaped quote — a whitespace edit
/// inside that literal would then compare equal and never be planned. Under the
/// pin the same text is a syntax error, which is the loud failure.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_framing_pins_the_settings_that_decide_what_a_definition_means() {
    let s = emit_schema("pinned");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    // The operator's environment, as an `ALTER ROLE … SET` would leave it.
    conn.execute("SET standard_conforming_strings = off; SET check_function_bodies = off")
        .await
        .expect("the operator's settings");

    let mut t = Table::default();
    t.columns
        .insert("label".into(), Column::new(ty("text")).not_null());
    t.checks.insert(
        "ck_label".into(),
        CheckConstraint {
            // Two characters, a backslash and a quote, inside a plain literal.
            expression: r"label <> 'it\'s  here'".into(),
        },
    );
    let change = pbps_model::Change::CreateTable {
        uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
        name: TableName::new(&s, "pinned"),
        table: Box::new(t),
    };
    let statements = Postgres::new()
        .emit(&change, Strategy::default())
        .expect("emit");

    // Every statement of the change, because the check is added after the
    // table: the one carrying the literal is the one the setting decides.
    async fn run(conn: &mut Conn, statements: &[pbps_dialect::Statement]) -> Vec<String> {
        let mut refusals = Vec::new();
        for stmt in statements {
            if let Err(e) = conn.execute(&stmt.sql).await {
                refusals.push(sqlstate(&e).to_owned());
            }
        }
        refusals
    }

    // Without the pin — the operator's environment as it stands — the engine
    // accepts the text as one literal, and nothing anywhere says so.
    assert!(
        run(&mut conn, &statements).await.is_empty(),
        "under `off` this is accepted, which is the silence the pin is for"
    );
    let swallowed = text(
        &mut conn,
        &format!(
            "SELECT pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
             WHERE conrelid = '{s}.pinned'::pg_catalog.regclass AND conname = 'ck_label'"
        ),
    )
    .await;
    assert!(
        swallowed.contains("it''s  here"),
        "the backslash was consumed and the quote closed nothing: {swallowed}"
    );
    conn.execute(&format!("DROP TABLE {s}.pinned"))
        .await
        .expect("drop what the unpinned session accepted");

    // With it, the same text is a syntax error — `42601` — and the settings
    // the framing established are what made it one.
    let framing = Postgres::new().transaction_framing();
    conn.begin(framing).await.expect("begin");
    assert_eq!(
        text(
            &mut conn,
            "SELECT current_setting('standard_conforming_strings')"
        )
        .await,
        "on"
    );
    assert_eq!(
        text(&mut conn, "SELECT current_setting('check_function_bodies')").await,
        "on"
    );
    let refusals = run(&mut conn, &statements).await;
    conn.rollback(framing).await.expect("rollback");
    drop_schema(&mut conn, &s).await;
    assert_eq!(
        refusals,
        vec!["42601"],
        "the pin's failure is the loud one, and it is the check that carries it"
    );
}

/// `online: true` becomes `CONCURRENTLY`, and the statement says it cannot run
/// inside a transaction rather than being special-cased in the runner.
///
/// The second half is the measured constraint that decides the first: a
/// concurrent build cannot share a batch with anything, so it cannot carry the
/// write `search_path` a filter would be bound under. An index with a filter is
/// therefore built the ordinary way and the hint is dropped — the trait's own
/// rule for a hint a dialect cannot honour on this statement.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_online_index_is_built_concurrently_and_says_it_leaves_the_transaction() {
    let s = emit_schema("concurrent");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.t (id integer NOT NULL, n integer)"
    ))
    .await
    .expect("the table the indexes go on");

    let table = TableName::new(&s, "t");
    let plain = Index {
        columns: vec![IndexColumn {
            name: "id".into(),
            descending: false,
        }],
        include: vec![],
        unique: false,
        filter: None,
    };
    let filtered = Index {
        filter: Some("(n IS NOT NULL)".into()),
        ..plain.clone()
    };

    let online = Strategy { online: true };
    let add = |name: &str, index: &Index| pbps_model::Change::AddIndex {
        table: table.clone(),
        name: name.to_owned(),
        index: Box::new(index.clone()),
    };

    let concurrent = Postgres::new()
        .emit(&add("ix_plain", &plain), online)
        .expect("emit");
    assert_eq!(concurrent.len(), 1);
    assert!(
        concurrent[0].sql.contains("CONCURRENTLY"),
        "{}",
        concurrent[0].sql
    );
    assert!(!concurrent[0].transactional, "it has to say so itself");
    assert!(concurrent[0].own_batch, "and that it is alone in its batch");
    assert!(
        !concurrent[0].sql.contains("search_path"),
        "a scope would make it a transaction block: {}",
        concurrent[0].sql
    );

    let ordinary = Postgres::new()
        .emit(&add("ix_filtered", &filtered), online)
        .expect("emit");
    assert!(
        !ordinary[0].sql.contains("CONCURRENTLY"),
        "a filter needs the write path, and the write path needs a batch: {}",
        ordinary[0].sql
    );
    assert!(ordinary[0].transactional);
    assert!(
        ordinary[0].sql.contains("SET search_path"),
        "{}",
        ordinary[0].sql
    );

    // Both run: the concurrent one outside any transaction, the filtered one
    // inside the transaction the framing opens.
    conn.execute(&concurrent[0].sql).await.expect("concurrent");
    let framing = Postgres::new().transaction_framing();
    conn.begin(framing).await.expect("begin");
    conn.execute(&ordinary[0].sql).await.expect("filtered");
    conn.commit(framing).await.expect("commit");

    // And the engine's own refusal is what `non_transactional` is reporting.
    conn.begin(framing).await.expect("begin");
    let inside = conn
        .execute(&concurrent[0].sql.replace("ix_plain", "ix_again"))
        .await
        .expect_err("a concurrent build inside a transaction");
    conn.rollback(framing).await.expect("rollback");
    drop_schema(&mut conn, &s).await;
    // `25001` is `active_sql_transaction`: `CREATE INDEX CONCURRENTLY cannot
    // run inside a transaction block`, which is the engine's own words for
    // what `Statement::non_transactional` reports at plan time.
    assert_eq!(sqlstate(&inside), "25001");
}

/// A bare-literal default on a setting-sensitive column is refused, and the
/// engine is what says the refusal is not pedantry.
///
/// Measured: the identical `CREATE TABLE` run under two `DateStyle`s stores two
/// different dates, with no error and no warning either way — so which value a
/// column defaults to would be decided by whoever ran the apply. The resolved
/// typed spelling, which is what `pg_get_expr` reads back, stores the same
/// value under both (ADR-0013 §3, §4).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_default_whose_value_the_session_decides_is_refused_and_the_resolved_one_is_not() {
    let s = emit_schema("datestyle");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;

    // What the refusal is about, measured rather than asserted from memory.
    async fn stored(conn: &mut Conn, table: &str) -> String {
        text(
            conn,
            &format!(
                "SELECT string_agg(DISTINCT pg_catalog.pg_get_expr(d.adbin, d.adrelid), ' ') \
                 FROM pg_catalog.pg_attrdef d \
                 WHERE d.adrelid = '{table}'::pg_catalog.regclass"
            ),
        )
        .await
    }
    // Every spelling of one literal, because a rule about `'…'` alone would
    // have let the other three through with exactly this behaviour.
    // The last two are the same constant behind a comment: measured, this
    // engine reads a `--` comment as part of the whitespace a string constant
    // may be continued across, and a parenthesis inside a comment is data and
    // not the one that closes a grouping. Both are one ambiguous literal and
    // neither is an expression. The continued one is written last because its
    // comment runs to the end of its line.
    let columns = "d date DEFAULT '01/02/2026', e date DEFAULT E'01/02/2026', \
                   f date DEFAULT $$01/02/2026$$, g date DEFAULT U&'01/02/2026', \
                   i date DEFAULT (/* ) */ '01/02/2026'), \
                   h date DEFAULT '01/02/' -- split here\n'2026'";
    conn.execute(&format!(
        "SET DateStyle = 'ISO, MDY'; CREATE TABLE {s}.mdy ({columns})"
    ))
    .await
    .expect("under MDY");
    conn.execute(&format!(
        "SET DateStyle = 'ISO, DMY'; CREATE TABLE {s}.dmy ({columns})"
    ))
    .await
    .expect("under DMY");
    conn.execute(&format!(
        "CREATE TABLE {s}.resolved (d date DEFAULT '2026-01-02'::date)"
    ))
    .await
    .expect("the resolved spelling, still under DMY");
    conn.execute(&format!(
        "CREATE TABLE {s}.national (d date DEFAULT N'01/02/2026')"
    ))
    .await
    .expect_err("a national-character literal has no assignment cast to date");
    // And the cast does **not** resolve anything, which is the reason the gap
    // below is recorded rather than closed: an ambiguous literal moves with the
    // session whether it is cast or not.
    let cast = "d date DEFAULT '01/02/2026'::date, e date DEFAULT DATE '01/02/2026', \
                f date DEFAULT CAST('01/02/2026' AS date)";
    conn.execute(&format!("CREATE TABLE {s}.cast_dmy ({cast})"))
        .await
        .expect("cast, under DMY");
    conn.execute("SET DateStyle = 'ISO, MDY'")
        .await
        .expect("back");
    assert_eq!(
        stored(&mut conn, &format!("{s}.mdy")).await,
        "'2026-01-02'::date"
    );
    assert_eq!(
        stored(&mut conn, &format!("{s}.dmy")).await,
        "'2026-02-01'::date",
        "the same declaration, a different value, and nothing said so"
    );
    assert_eq!(
        stored(&mut conn, &format!("{s}.resolved")).await,
        "'2026-01-02'::date",
        "the resolved spelling does not move"
    );
    assert_eq!(
        stored(&mut conn, &format!("{s}.cast_dmy")).await,
        "'2026-02-01'::date",
        "all three cast spellings moved with the session, so a cast is not a \
         resolution — which is why only the bare literal is refused here and \
         the rest waits for ADR-0013 §3's canonicalization at plan time"
    );
    drop_schema(&mut conn, &s).await;

    // So the emitter refuses the first spelling and emits the second.
    let table = |default: &str| {
        let mut t = Table::default();
        let mut d = Column::new(ty("date"));
        d.default = Some(default.to_owned());
        t.columns.insert("d".into(), d);
        pbps_model::Change::CreateTable {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
            name: TableName::new(&s, "t"),
            table: Box::new(t),
        }
    };
    for spelling in [
        "'01/02/2026'",
        r"E'01/02/2026'",
        "$$01/02/2026$$",
        "U&'01/02/2026'",
        "N'01/02/2026'",
        "n'01/02/2026'",
        "(N') ')",
        "'01/02/' -- split here\n'2026'",
        "(/* ) */ '01/02/2026')",
    ] {
        let refusal = Postgres::new()
            .emit(&table(spelling), Strategy::default())
            .unwrap_err();
        let message = refusal.to_string();
        assert!(message.contains("DateStyle"), "{spelling}: {message}");
        assert!(
            message.contains("'2026-01-02'::date"),
            "{spelling}: {message}"
        );
    }
    Postgres::new()
        .emit(&table("'2026-01-02'::date"), Strategy::default())
        .expect("the resolved typed spelling is emitted as written");
    // And a type whose input function reads no setting is never in question.
    let mut plain = Table::default();
    let mut n = Column::new(ty("integer"));
    n.default = Some("'7'".into());
    plain.columns.insert("n".into(), n);
    Postgres::new()
        .emit(
            &pbps_model::Change::CreateTable {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
                name: TableName::new(&s, "plain"),
                table: Box::new(plain),
            },
            Strategy::default(),
        )
        .expect("a bare literal on an integer is not setting-sensitive");
}

/// A type change this engine will not make on its own is refused with the
/// clause named, and never performed under a cast pbps chose (ADR-0012 §5).
///
/// The engine's own refusal is measured here rather than assumed, because the
/// emitter's refusal is only worth having if the alternative is a failure and
/// not a silent conversion.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_type_change_that_would_need_a_using_clause_is_refused_by_name() {
    let s = emit_schema("using");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!("CREATE TABLE {s}.t (c varchar(10) NOT NULL)"))
        .await
        .expect("the table");
    // `42804` is `datatype_mismatch`: `column "c" cannot be cast automatically
    // to type integer … You might need to specify "USING c::integer"`.
    let engine = conn
        .execute(&format!("ALTER TABLE {s}.t ALTER COLUMN c TYPE integer"))
        .await
        .expect_err("this engine will not choose the conversion either");
    assert_eq!(sqlstate(&engine), "42804");
    drop_schema(&mut conn, &s).await;

    let column = TableName::new(&s, "t").column("c");
    let refusal = Postgres::new()
        .emit(
            &pbps_model::Change::AlterColumnType {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column: column.clone(),
                from: ty("varchar(10)"),
                to: ty("integer"),
                from_nullable: false,
                to_nullable: false,
            },
            Strategy::default(),
        )
        .expect_err("a conversion the engine refuses outright");
    let message = refusal.to_string();
    assert!(message.contains("USING"), "{message}");
    assert!(message.contains("`c`"), "{message}");
    // And nothing here emits one.
    let widened = Postgres::new()
        .emit(
            &pbps_model::Change::AlterColumnType {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column,
                from: ty("varchar(10)"),
                to: ty("varchar(20)"),
                from_nullable: false,
                to_nullable: true,
            },
            Strategy::default(),
        )
        .expect("a widening the engine makes on its own");
    assert!(!widened[0].sql.contains("USING"), "{}", widened[0].sql);
    // Both subcommands in one `ALTER TABLE` (ADR-0011, Amendment 1).
    assert!(
        widened[0]
            .sql
            .contains("TYPE character varying(20), ALTER COLUMN \"c\" DROP NOT NULL"),
        "{}",
        widened[0].sql
    );
}

/// A nullable primary key column is refused, and the engine is why.
///
/// SQL Server refuses this at `CREATE`, loudly. Measured here, PostgreSQL
/// **accepts** the table and sets `NOT NULL` itself — so the declaration and
/// the database disagree from the moment the table exists, the pull reads
/// `nullable: false`, and the plan that would put it back is refused for ever.
/// A declaration this engine silently rewrites is worse than one it rejects,
/// which is why the rule is not inherited from the other dialect but measured
/// on this one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_nullable_primary_key_column_is_refused_because_this_engine_would_not() {
    let s = emit_schema("nullablepk");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.t (id integer, CONSTRAINT pk PRIMARY KEY (id))"
    ))
    .await
    .expect("this engine accepts the table a nullable declaration describes");
    assert!(
        truth(
            &mut conn,
            &format!(
                "SELECT attnotnull FROM pg_catalog.pg_attribute \
                 WHERE attrelid = '{s}.t'::pg_catalog.regclass AND attname = 'id'"
            ),
        )
        .await,
        "the engine set NOT NULL itself, which the declaration never said"
    );
    // And the plan that would put the declaration back is refused, so the
    // disagreement is permanent.
    let refusal = conn
        .execute(&format!("ALTER TABLE {s}.t ALTER COLUMN id DROP NOT NULL"))
        .await
        .expect_err("a primary key column cannot be made nullable");
    // `42P16` is `invalid_table_definition`: `column "id" is in a primary key`.
    assert_eq!(sqlstate(&refusal), "42P16");
    drop_schema(&mut conn, &s).await;

    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new(ty("integer")));
    table.primary_key = Some(PrimaryKey {
        name: Some("pk".into()),
        columns: vec!["id".into()],
    });
    let problems = Postgres::new().validate_table(&TableName::new(&s, "t"), &table);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].to_string().contains("must be NOT NULL"),
        "{}",
        problems[0]
    );
}

/// A rename that crosses a schema takes two statements, and each says what it
/// does to the name so that a staged checkpoint can find the table in between.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_rename_across_schemas_is_two_statements_that_each_say_where_the_table_went() {
    let s = emit_schema("rename");
    let to_schema = format!("{s}_to");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &to_schema).await;
    conn.execute(&format!("CREATE TABLE {s}.before (id integer NOT NULL)"))
        .await
        .expect("the table");

    let from = TableName::new(&s, "before");
    let to = TableName::new(&to_schema, "after");
    let statements = Postgres::new()
        .emit(
            &pbps_model::Change::RenameTable {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
                from: from.clone(),
                to: to.clone(),
            },
            Strategy::default(),
        )
        .expect("emit");
    assert_eq!(statements.len(), 2);
    // The transfer first, so the name in between is the old one in the new
    // schema — and it is that name the checkpoint has to look under.
    let between = TableName::new(&to_schema, "before");
    assert_eq!(statements[0].renames, vec![(from, between.clone())]);
    assert_eq!(statements[1].renames, vec![(between, to.clone())]);

    for stmt in &statements {
        conn.execute(&stmt.sql)
            .await
            .unwrap_or_else(|e| panic!("{}\n{e}", stmt.sql));
    }
    let pulled = pull(&mut conn).await;
    let landed = ours_only(&pulled, &to_schema);
    drop_schema(&mut conn, &s).await;
    drop_schema(&mut conn, &to_schema).await;
    assert_eq!(landed.tables.keys().collect::<Vec<_>>(), vec![&to]);
}

/// A primary key the declaration did not name is dropped by asking the catalog
/// what it is called, never by guessing the name this engine would have made.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_unnamed_primary_key_is_dropped_by_the_name_the_catalog_holds() {
    let s = emit_schema("unnamedpk");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    // The constraint is named by hand as something no rule would derive, so a
    // guess cannot pass — and the table's own name carries every character that
    // has to survive being written into a `DO` block: an apostrophe, the `%` of
    // `format`, and the dollar-quote tag itself.
    let odd = "it's $pbps$ %s";
    conn.execute(&format!(
        "CREATE TABLE {s}.\"{odd}\" (id integer NOT NULL, CONSTRAINT nobody_would_guess_this PRIMARY KEY (id))"
    ))
    .await
    .expect("the table");

    let table = TableName::new(&s, odd);
    let statements = Postgres::new()
        .emit(
            &pbps_model::Change::SetPrimaryKey {
                table: table.clone(),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
            Strategy::default(),
        )
        .expect("emit");
    assert_eq!(statements.len(), 1);
    assert!(
        !statements[0].sql.contains("nobody_would_guess_this"),
        "the emitter cannot know the name: {}",
        statements[0].sql
    );
    conn.execute(&statements[0].sql)
        .await
        .unwrap_or_else(|e| panic!("{}\n{e}", statements[0].sql));

    let pulled = pull(&mut conn).await;
    let ours = ours_only(&pulled, &s);
    drop_schema(&mut conn, &s).await;
    assert_eq!(ours.tables[&table].primary_key, None);
}

/// SPEC §11.5 invariant 3 over the altering half: a rename, a widening, a
/// nullability, a default, a dropped column, a dropped constraint and a dropped
/// index, all planned against the introspected state and all converging.
///
/// The paths here are the ones the creating tests never reach, and each of them
/// is a statement this engine spells differently from the other one: a
/// nullability is a subcommand rather than a restated column definition, a
/// default is a property rather than a named constraint, and a rename is
/// `ALTER TABLE … RENAME` rather than a procedure call.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_migration_that_renames_widens_retypes_and_drops_converges() {
    let s = emit_schema("alters");
    let mut a = Schema::default();
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    t.columns
        .insert("email".into(), Column::new(ty("varchar(50)")));
    t.columns.insert("notes".into(), Column::new(ty("text")));
    let mut n = Column::new(ty("integer"));
    n.default = Some("0".into());
    t.columns.insert("n".into(), n);
    t.primary_key = Some(PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
    });
    t.checks.insert(
        "ck_n".into(),
        CheckConstraint {
            expression: "(n >= 0)".into(),
        },
    );
    t.indexes.insert(
        "ix_n".into(),
        Index {
            columns: vec![IndexColumn {
                name: "n".into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: None,
        },
    );
    a.tables.insert(TableName::new(&s, "t"), t);
    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(
        &mut conn,
        &Postgres::new(),
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    let state_a = ours_only(&pull(&mut conn).await, &s);
    assert_eq!(state_a, normalized(&a), "precondition");

    let mut b = a.clone();
    {
        let t = b.tables.get_mut(&TableName::new(&s, "t")).unwrap();
        let email = t.columns.shift_remove("email").expect("email");
        t.columns.insert("contact_email".into(), email);
        t.columns["contact_email"].ty = ty("varchar(200)");
        // A column that was nullable becomes required, and one that had a
        // default loses it.
        t.columns["contact_email"].nullable = false;
        t.columns["n"].default = None;
        t.columns.shift_remove("notes");
        t.checks.remove("ck_n");
        t.indexes.remove("ix_n");
    }
    let intents = [
        Intent::RenameColumn {
            table: TableName::new(&s, "t"),
            from: "email".into(),
            to: "contact_email".into(),
        },
        Intent::DropColumn {
            column: TableName::new(&s, "t").column("notes"),
            reason: "merged elsewhere".into(),
        },
    ];
    let ids_b = mint_ids(&b, &ids_a, &intents);
    let migration = plan(&state_a, &ids_a, &b, &ids_b);
    assert!(
        migration
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::RenameColumn { .. })),
        "the rename must plan as a rename, not a drop plus an add: {migration:#?}"
    );

    apply(&mut conn, &Postgres::new(), &migration).await;
    let pulled = pull(&mut conn).await;
    let state_b = ours_only(&pulled, &s);
    let limitations = ours_limitations(&pulled, &s);
    drop_schema(&mut conn, &s).await;

    assert!(limitations.is_empty(), "{limitations:?}");
    assert_eq!(state_b, normalized(&b));
    let again = plan(&state_b, &ids_b, &b, &ids_b);
    assert!(again.is_empty(), "the plan after convergence: {again:#?}");
}

/// The other three settings the framing pins, and the reason they are pinned
/// there rather than around each statement: unlike `search_path` they do not
/// vary per object, so they are established once and the plan a reviewer reads
/// stays the SQL and not the scaffolding (SPEC §14.1).
///
/// What they decide is measured here, not recalled. The same three declared
/// expressions, created by an operator whose session says `DMY`, New York and
/// `sql_standard`, become three different stored constraints — a different day,
/// a different instant, and an interval with the opposite sign. None of it is
/// an error and none of it is visible afterwards: `pg_get_constraintdef` gives
/// back the *value* the session decided, so the environment that decided it is
/// gone by the time anybody looks.
///
/// This is the whole reason a verbatim expression can be carried at all. The
/// emitter refuses a bare literal *default* on a setting-sensitive column, but
/// a check and a filter name columns rather than carrying a type, and no
/// offline rule can tell `'01/02/2026'` inside one from a string that merely
/// looks like a date. Pinning the reader is what makes the text mean one thing.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_framing_pins_what_an_ambiguous_temporal_literal_means() {
    let s = emit_schema("temporal");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;

    let mut t = Table::default();
    t.columns.insert("d".into(), Column::new(ty("date")));
    t.columns
        .insert("at".into(), Column::new(ty("timestamptz")));
    t.columns.insert("i".into(), Column::new(ty("interval")));
    t.columns.insert("x".into(), Column::new(ty("integer")));
    // One expression per setting, each unambiguous to the person who wrote it.
    // The last two are not temporal input at all: an abbreviation is read from
    // a dictionary `TimeZone` does not cover, and `x = NULL` is rewritten by
    // the *parser* into a different predicate.
    for (name, expression) in [
        ("ck_d", "d >= '01/02/2026'"),
        ("ck_at", "at >= '2026-01-02 00:00'"),
        ("ck_i", "i >= '-1 2:00:00'"),
        ("ck_abbrev", "at >= '2026-01-15 12:00:00 CST'"),
        ("ck_null", "x = NULL"),
    ] {
        t.checks.insert(
            name.into(),
            CheckConstraint {
                expression: expression.into(),
            },
        );
    }
    let change = pbps_model::Change::CreateTable {
        uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
        name: TableName::new(&s, "temporal"),
        table: Box::new(t),
    };
    let statements = Postgres::new()
        .emit(&change, Strategy::default())
        .expect("emit");

    async fn stored(conn: &mut Conn, schema: &str) -> String {
        text(
            conn,
            &format!(
                "SELECT string_agg(pg_catalog.pg_get_constraintdef(oid), ' | ' ORDER BY conname) \
                 FROM pg_catalog.pg_constraint \
                 WHERE conrelid = '{schema}.temporal'::pg_catalog.regclass AND contype = 'c'"
            ),
        )
        .await
    }
    async fn run(conn: &mut Conn, statements: &[pbps_dialect::Statement]) {
        for stmt in statements {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("the engine rejected:\n{}\n{e}", stmt.sql));
        }
    }

    // The operator's environment, as an `ALTER ROLE … SET` would leave it.
    const THEIRS: &str = "SET DateStyle = 'ISO, DMY'; SET TimeZone = 'America/New_York'; \
         SET IntervalStyle = 'sql_standard'; SET timezone_abbreviations = 'Australia'; \
         SET transform_null_equals = on";
    conn.execute(THEIRS).await.expect("the operator's settings");
    run(&mut conn, &statements).await;
    // Read from a canonical session, and that is not a detail: two of these
    // three settings render on the way *out* as well as reading on the way in,
    // so a definition fetched by the session that wrote it shows its own
    // spelling of its own value and the two runs look alike. One reader, and
    // what is left between them is the value.
    conn.execute("SET TimeZone = 'UTC'; SET IntervalStyle = 'postgres'")
        .await
        .expect("the reader's settings");
    let theirs = stored(&mut conn, &s).await;
    conn.execute(&format!("DROP TABLE {s}.temporal"))
        .await
        .expect("drop what the unpinned session decided");
    conn.execute(THEIRS)
        .await
        .expect("the operator's settings again");

    // The same statements, under the framing, with the operator's settings
    // still in the session underneath it.
    let framing = Postgres::new().transaction_framing();
    conn.begin(framing).await.expect("begin");
    run(&mut conn, &statements).await;
    // Read it back before the rollback takes the table away; the catalogue sees
    // this transaction's own DDL.
    let ours = stored(&mut conn, &s).await;
    conn.rollback(framing).await.expect("rollback");
    drop_schema(&mut conn, &s).await;

    // A day, an instant, a sign, an abbreviation fifteen and a half hours out,
    // and a predicate that stopped being the one that was written.
    for decided in [
        "'2026-02-01'",
        "2026-01-02 05:00:00+00",
        "-1 days -02:00:00",
        "2026-01-15 02:30:00+00",
        "x IS NULL",
    ] {
        assert!(theirs.contains(decided), "{decided} not in {theirs}");
    }
    // Under the pin, one meaning each, and it is the one the framing names.
    for pinned in [
        "'2026-01-02'",
        "2026-01-02 00:00:00+00",
        "-1 days +02:00:00",
        "2026-01-15 18:00:00+00",
        "x = NULL::integer",
    ] {
        assert!(ours.contains(pinned), "{pinned} not in {ours}");
    }
}

/// A type change that gains or loses the time zone is refused by name, and the
/// measurement beside it is why: the engine does not fail it, it *answers* it
/// from a session setting, and two operators applying one approved plan store
/// two different instants.
///
/// The framing pins `TimeZone` to UTC, which makes the answer reproducible.
/// Reproducible is not declared: it would silently reinterpret every stored
/// value as UTC, which is a data transformation nobody wrote down and nobody
/// reviewed — the same ground the `USING` refusal stands on (ADR-0012 §5).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_type_change_that_the_session_would_decide_is_refused_by_name() {
    let s = emit_schema("zone");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;

    // What the refusal is about, on this server. The same stored value, the
    // same `ALTER`, two sessions, two instants.
    //
    // Twice, because the guard covers two shapes: the offset gained, and the
    // offset *kept* while the date part goes. The second was missed by a
    // predicate that asked whether the offset changed rather than whether the
    // session decided.
    let mut instants = Vec::new();
    for zone in ["UTC", "America/New_York"] {
        conn.execute(&format!(
            "SET TimeZone = '{zone}'; \
             CREATE TABLE {s}.z (at timestamp); \
             INSERT INTO {s}.z VALUES ('2026-01-02 12:00'); \
             ALTER TABLE {s}.z ALTER COLUMN at TYPE timestamptz"
        ))
        .await
        .expect("the engine performs it without a word");
        // `AT TIME ZONE 'UTC'` for the reading, because a `timestamptz` is
        // *rendered* in the session's zone too: read from the session that
        // wrote it, both rows name the same wall clock and the difference the
        // refusal is about is invisible.
        instants.push(
            text(
                &mut conn,
                &format!("SELECT (at AT TIME ZONE 'UTC')::text FROM {s}.z"),
            )
            .await,
        );
        conn.execute(&format!("DROP TABLE {s}.z"))
            .await
            .expect("drop");
    }
    drop_schema(&mut conn, &s).await;
    assert_eq!(
        instants,
        vec![
            "2026-01-02 12:00:00".to_owned(),
            "2026-01-02 17:00:00".to_owned()
        ],
        "the conversion reads the naive value in the session's zone"
    );

    // The second shape, on the same server: `timestamptz` keeps its offset
    // into `timetz` and the wall clock is still the session's.
    let s2 = emit_schema("zone2");
    fresh(&mut conn, &s2).await;
    let mut kept = Vec::new();
    for zone in ["UTC", "America/New_York"] {
        conn.execute(&format!(
            "SET TimeZone = '{zone}'; \
             CREATE TABLE {s2}.z (at timestamptz); \
             INSERT INTO {s2}.z VALUES ('2026-01-02 12:00:00+00'); \
             ALTER TABLE {s2}.z ALTER COLUMN at TYPE timetz"
        ))
        .await
        .expect("the engine performs this one without a word either");
        conn.execute("SET TimeZone = 'UTC'")
            .await
            .expect("one reader");
        kept.push(text(&mut conn, &format!("SELECT at::text FROM {s2}.z")).await);
        conn.execute(&format!("DROP TABLE {s2}.z"))
            .await
            .expect("drop");
    }
    drop_schema(&mut conn, &s2).await;
    assert_eq!(
        kept,
        vec!["12:00:00+00".to_owned(), "07:00:00-05".to_owned()],
        "the offset survives and the session still decided the wall clock"
    );

    // So pbps does not emit either of them. The message names the zone and the
    // two-step remedy, because a refusal a reader cannot act on is a wall.
    let refusal = Postgres::new()
        .emit(
            &pbps_model::Change::AlterColumnType {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column: pbps_model::ColumnRef {
                    table: TableName::new(&s, "z"),
                    name: "at".into(),
                },
                from: ty("timestamp"),
                to: ty("timestamptz"),
                from_nullable: true,
                to_nullable: true,
            },
            Strategy::default(),
        )
        .expect_err("the zone is not pbps's to choose");
    let message = refusal.to_string();
    assert!(message.contains("time zone"), "{message}");
    assert!(message.contains("AT TIME ZONE"), "{message}");

    // The pair with a zone that is *not* the session's, measured beside them
    // because a guard that cannot say yes is a guard nobody can use: `timetz`
    // stores the offset next to the local time, so dropping it keeps what is
    // already there.
    let s3 = emit_schema("zone3");
    fresh(&mut conn, &s3).await;
    let mut projected = Vec::new();
    for zone in ["UTC", "America/New_York"] {
        conn.execute(&format!(
            "SET TimeZone = '{zone}'; \
             CREATE TABLE {s3}.z (at timetz); \
             INSERT INTO {s3}.z VALUES ('12:00:00+03'); \
             ALTER TABLE {s3}.z ALTER COLUMN at TYPE time"
        ))
        .await
        .expect("the projection this dialect must not refuse");
        projected.push(text(&mut conn, &format!("SELECT at::text FROM {s3}.z")).await);
        conn.execute(&format!("DROP TABLE {s3}.z"))
            .await
            .expect("drop");
    }
    drop_schema(&mut conn, &s3).await;
    assert_eq!(
        projected,
        vec!["12:00:00".to_owned(), "12:00:00".to_owned()],
        "the stored local time is kept and no session was consulted"
    );
    assert!(
        Postgres::new()
            .emit(
                &pbps_model::Change::AlterColumnType {
                    uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                    column: pbps_model::ColumnRef {
                        table: TableName::new(&s, "z"),
                        name: "at".into(),
                    },
                    from: ty("timetz"),
                    to: ty("time"),
                    from_nullable: true,
                    to_nullable: true,
                },
                Strategy::default(),
            )
            .is_ok(),
        "and the emitter has to let it through"
    );

    let kept_refusal = Postgres::new()
        .emit(
            &pbps_model::Change::AlterColumnType {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column: pbps_model::ColumnRef {
                    table: TableName::new(&s, "z"),
                    name: "at".into(),
                },
                from: ty("timestamptz"),
                to: ty("timetz"),
                from_nullable: true,
                to_nullable: true,
            },
            Strategy::default(),
        )
        .expect_err("keeping the offset does not make it pbps's to choose");
    assert!(
        kept_refusal.to_string().contains("time zone"),
        "{kept_refusal}"
    );
}

/// A declaration that gives up its primary key and relaxes the column it held
/// applies, in the order the differ now puts the two changes in.
///
/// The plan was valid, reviewed and unapplicable: `DROP NOT NULL` ran first
/// and this engine answered `42P16`, `column "id" is in a primary key`. SQL
/// Server refuses the same shape with 5074 and 4922 behind it, so the ordering
/// that fixes it belongs to the differ and not to either dialect
/// (`a_key_is_dropped_before_the_column_it_held_is_relaxed`).
///
/// A key being *replaced* is the same shape and is covered by the same
/// ordering, because it is planned as two changes rather than one — see
/// `a_key_is_replaced_around_the_columns_both_of_its_shapes_name`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_given_up_leaves_before_its_column_is_relaxed() {
    let s = emit_schema("pkorder");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let table = TableName::new(&s, "t");

    let mut with_key = Table::default();
    with_key
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    with_key.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
    });
    let mut a = Schema::default();
    a.tables.insert(table.clone(), with_key);

    let mut without = Table::default();
    without
        .columns
        .insert("id".into(), Column::new(ty("integer")));
    let mut b = Schema::default();
    b.tables.insert(table.clone(), without);

    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);
    let ids_b = mint_ids(&b, &ids_a, &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    // The whole migration, statement by statement, in the plan's own order —
    // `apply` panics with the SQL and the engine's word if any of it is
    // refused.
    apply(&mut conn, &pg, &plan(&a, &ids_a, &b, &ids_b)).await;

    let pulled = pull(&mut conn).await;
    drop_schema(&mut conn, &s).await;
    let state = ours_only(&pulled, &s);
    assert_eq!(state, normalized(&b));
    let again = plan(&state, &ids_b, &b, &ids_b);
    assert!(again.is_empty(), "the plan after convergence: {again:#?}");
}

/// A declaration that replaces its primary key, adds the column the new key
/// names and relaxes the column the old key held — every dependency in one
/// plan — applies and converges.
///
/// This is the shape one `SetPrimaryKey` could not order: its drop has to
/// precede the `DROP NOT NULL` on `id`, which this engine refuses with `42P16`
/// while the key stands, and its add has to follow the `ADD COLUMN` that makes
/// `other` exist. Opposite ends of the plan, so the differ emits the two halves
/// as two changes (DECISIONS 270); the statements are the ones the emitter
/// always produced.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_is_replaced_around_the_columns_both_of_its_shapes_name() {
    let s = emit_schema("pkswap");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let table = TableName::new(&s, "t");

    let mut before = Table::default();
    before
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    before.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
    });
    let mut a = Schema::default();
    a.tables.insert(table.clone(), before);

    let mut after = Table::default();
    after
        .columns
        .insert("id".into(), Column::new(ty("integer")));
    after
        .columns
        .insert("other".into(), Column::new(ty("integer")).not_null());
    after.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["other".into()],
    });
    let mut b = Schema::default();
    b.tables.insert(table.clone(), after);

    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);
    let ids_b = mint_ids(&b, &ids_a, &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    apply(&mut conn, &pg, &plan(&a, &ids_a, &b, &ids_b)).await;

    let pulled = pull(&mut conn).await;
    drop_schema(&mut conn, &s).await;
    let state = ours_only(&pulled, &s);
    assert_eq!(state, normalized(&b));
    let again = plan(&state, &ids_b, &b, &ids_b);
    assert!(again.is_empty(), "the plan after convergence: {again:#?}");
}

/// A conversion to text runs the stored value through the type's *output*
/// function, so the two settings the framing was told were output-only decide
/// what it leaves behind.
///
/// They were excluded on a measurement that was true of what it measured: a
/// declared expression stores the same constraint under `hex`/`1` and under
/// `escape`/`0`, because nothing in `CHECK (b >= '\x0102')` renders a value.
/// An `ALTER COLUMN … TYPE text` does, and the same approved statement then
/// leaves two different strings in the table. Pinning them is the whole answer
/// where refusing the conversion would be an enumeration: every cast to text
/// goes through an output function.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_conversion_to_text_renders_under_the_framings_settings_and_not_the_operators() {
    let s = emit_schema("render");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.t (b bytea, f double precision); \
         INSERT INTO {s}.t VALUES ('\\x0102', 0.1234567890123456789)"
    ))
    .await
    .expect("the row both conversions render");
    // The operator's environment, as an `ALTER ROLE … SET` would leave it.
    conn.execute("SET bytea_output = 'escape'; SET extra_float_digits = -3")
        .await
        .expect("the operator's settings");

    let table = TableName::new(&s, "t");
    let pg = Postgres::new();
    let retype = |column: &str, from: &str| pbps_model::Change::AlterColumnType {
        uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
        column: pbps_model::ColumnRef {
            table: table.clone(),
            name: column.to_owned(),
        },
        from: ty(from),
        to: ty("text"),
        from_nullable: true,
        to_nullable: true,
    };

    let framing = pg.transaction_framing();
    conn.begin(framing).await.expect("begin");
    for change in [retype("b", "bytea"), retype("f", "double precision")] {
        for stmt in pg.emit(&change, Strategy::default()).expect("emit") {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("the engine rejected:\n{}\n{e}", stmt.sql));
        }
    }
    let rendered = text(&mut conn, &format!("SELECT b || ' ' || f FROM {s}.t")).await;
    conn.rollback(framing).await.expect("rollback");
    drop_schema(&mut conn, &s).await;

    // What the pin decided, not what the session held: `escape` would have
    // stored `\001\002` and `-3` would have stopped at twelve digits.
    assert_eq!(rendered, "\\x0102 0.12345678901234568");
}

/// A column with a default and a new type applies in three phases: the old
/// default out, the type changed, the new default in.
///
/// Both kinds are class 9 and the tiebreaker there is the change's rendering,
/// which sorted `AlterColumnDefault` first by the alphabet. Measured here, that
/// order cannot apply: this engine refuses `SET DEFAULT 'abc'` on an `integer`
/// column outright, and the statement that would have made the column `text` is
/// the next one. The other end is measured on the other engine — SQL Server
/// refuses the type change itself while a default constraint stands — which is
/// why the middle phase exists and why the ranks are three and not two.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_default_written_for_the_new_type_is_set_after_the_type_is() {
    let s = emit_schema("retype");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let table = TableName::new(&s, "t");

    let with = |t: &str, d: &str| Column {
        default: Some(d.to_owned()),
        ..Column::new(ty(t))
    };
    let mut before = Table::default();
    before.columns.insert("n".into(), with("integer", "0"));
    // A default on both sides, so the plan carries all three phases rather
    // than only the two this engine can fail on.

    let mut a = Schema::default();
    a.tables.insert(table.clone(), before);

    let mut after = Table::default();
    after
        .columns
        .insert("n".into(), with("text", "'abc'::text"));
    let mut b = Schema::default();
    b.tables.insert(table.clone(), after);

    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);
    let ids_b = mint_ids(&b, &ids_a, &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    apply(&mut conn, &pg, &plan(&a, &ids_a, &b, &ids_b)).await;

    let pulled = pull(&mut conn).await;
    drop_schema(&mut conn, &s).await;
    let state = ours_only(&pulled, &s);
    assert_eq!(state, normalized(&b));
    let again = plan(&state, &ids_b, &b, &ids_b);
    assert!(again.is_empty(), "the plan after convergence: {again:#?}");
}

// ---------------------------------------------------------------------------
// Phase 5 step 8: the ledger, the lock and `doctor`, against a real server.
//
// Every test here works in a database of its own, because the ledger is one
// pair of tables in `public` and two tests sharing them would take each other's
// lock. That is also how the SQL Server suite does it, and it is what lets this
// suite keep running its tests in parallel.
// ---------------------------------------------------------------------------

use pbps_db::LedgerError;
use pbps_db::ledger::LockInfo;
use pbps_model::{ObjectName, StateKind, StateSnapshot, Unreadable};
use pbps_pg::{doctor, state};

/// A throwaway database that removes itself.
///
/// A **database**, not a schema: the ledger lives in `public` (SPEC §8.1, and
/// `pbps_pg::state`'s header for why `public`), so there is exactly one of it
/// per database and nothing smaller can isolate two tests from each other.
struct TestDb {
    name: String,
    conn: Conn,
}

impl TestDb {
    async fn create(tag: &str) -> TestDb {
        // The pid keeps two concurrent `cargo test` runs apart; the tag keeps
        // this run's own tests apart.
        let name = format!("pbps_test_{tag}_{}", std::process::id());
        let mut admin = connect().await;
        // `WITH (FORCE)` disconnects whatever is still attached — a previous
        // run killed halfway leaves a database behind, and `DROP DATABASE`
        // without it fails while anything is connected.
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .expect("clear a database left by an earlier run");
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create the test database");
        let conn = Conn::connect(Driver::Postgres, &conn_str_for(&name))
            .await
            .expect("connect to the test database");
        TestDb { name, conn }
    }

    /// Opens a second connection to the same database, for the tests that need
    /// two — a lock has to be contended by somebody.
    async fn second(&self) -> Conn {
        Conn::connect(Driver::Postgres, &conn_str_for(&self.name))
            .await
            .expect("a second connection to the test database")
    }

    async fn drop(self) {
        let name = self.name;
        // Dropped before the database is: `WITH (FORCE)` would terminate this
        // very connection, and a test that cleans up by killing itself reads
        // as a flake later.
        std::mem::drop(self.conn);
        let mut admin = connect().await;
        // Failure to clean up must not obscure the test's own verdict; the
        // container is throwaway anyway.
        let _ = admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await;
    }
}

/// The suite's connection string, pointed at another database on the same
/// server.
///
/// Keyword form only, which is what `scripts/live-tests-pg.sh` exports. A URL
/// is accepted by the driver and deliberately not rewritten here: guessing at
/// which part of a URL is the database name is how a suite ends up running
/// against the wrong one, and these tests create and drop databases.
fn conn_str_for(database: &str) -> String {
    settings_with(&[("dbname", database)])
}

/// The same, as another role.
fn conn_str_as(role: &str, password: &str, database: &str) -> String {
    settings_with(&[("dbname", database), ("user", role), ("password", password)])
}

fn settings_with(overrides: &[(&str, &str)]) -> String {
    let base = conn_str();
    assert!(
        !base.contains("://"),
        "these tests need PBPS_TEST_PG_DB in libpq keyword form (`host=... dbname=...`), \
         not a URL: {base}"
    );
    let mut parts: Vec<String> = base
        .split_whitespace()
        .filter(|p| {
            !overrides
                .iter()
                .any(|(key, _)| p.starts_with(&format!("{key}=")))
        })
        .map(str::to_owned)
        .collect();
    for (key, value) in overrides {
        parts.push(format!("{key}={value}"));
    }
    parts.join(" ")
}

/// A role that holds nothing but the right to connect, and its password.
///
/// The SQL Server suite creates one for the reason this one needs it too:
/// `postgres` is a superuser, a superuser passes every permission question
/// without asking the catalog, and *that* is how three permission bugs survived
/// the first live test on the other engine.
async fn least_privilege_role(db: &mut TestDb, tag: &str) -> String {
    let role = format!("pbps_dep_{tag}_{}", std::process::id());
    // Roles are cluster objects, not database ones (ADR-0010 §1), so one left
    // by an earlier run is still here. Dropping it can fail while it still owns
    // something in another database; the create below then says so loudly.
    let _ = db
        .conn
        .execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await;
    db.conn
        .execute(&format!("CREATE ROLE {role} LOGIN PASSWORD 'live-test'"))
        .await
        .expect("create the least-privilege role");
    role
}

fn snapshot(kind: StateKind) -> StateSnapshot {
    StateSnapshot::new(kind, Schema::default(), IdsFile::default(), "live-test")
}

/// SPEC §8.1: the whole state goes in and comes back out unchanged. Everything
/// downstream — drift, the plan checksum, `status` — reads this row, so a
/// serialization that lost a field would make every one of them quietly wrong.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_ledger_returns_exactly_what_was_recorded() {
    let mut db = TestDb::create("ledger").await;

    let mut first = snapshot(StateKind::Baseline);
    first.reason = Some("adopting the database as it stands".into());
    first.git_sha = Some("bd4be74".into());
    let id = state::record(&mut db.conn, &first).await.expect("record");

    let back = state::latest(&mut db.conn)
        .await
        .expect("latest")
        .expect("an entry");
    assert_eq!(back.id, id);
    assert_eq!(
        back.snapshot, first,
        "the snapshot must survive the round trip"
    );
    // The server's clock, rendered as ISO 8601 by the query rather than cast.
    assert_eq!(back.applied_at.len(), 23, "{}", back.applied_at);
    assert!(back.applied_at.contains('T'), "{}", back.applied_at);

    // Recording again must append, never overwrite: the ledger is a history.
    let second = snapshot(StateKind::Apply);
    let second_id = state::record(&mut db.conn, &second)
        .await
        .expect("record again");
    assert!(second_id > id);
    assert_eq!(
        state::latest(&mut db.conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .kind,
        StateKind::Apply
    );

    let removed = state::prune(&mut db.conn, 1).await.expect("prune");
    assert_eq!(removed, 1, "one old entry removed");
    let history = state::history(&mut db.conn, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].id, second_id,
        "the newest is the one that survives"
    );

    db.drop().await;
}

/// The three answers this project keeps getting bitten by, kept apart on a real
/// server: *absent, empty and unreachable are three different things, and only
/// one of them is good news.*
///
/// The direction that matters is the last one. A connection failure rendered as
/// an empty history is a `status` screen that says "never deployed" about a
/// database nobody could reach — and the remedy it prints is `bootstrap`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_ledger_never_initialized_an_empty_one_and_an_unreachable_database_stay_three_answers() {
    let mut db = TestDb::create("three_answers").await;

    // Never initialized: there is no ledger, and the remedy is `baseline` or
    // `bootstrap`.
    assert!(!state::is_initialized(&mut db.conn).await.expect("probe"));
    assert!(matches!(
        state::latest(&mut db.conn).await,
        Err(LedgerError::NotInitialized)
    ));
    assert!(matches!(
        state::history(&mut db.conn, 10).await,
        Err(LedgerError::NotInitialized)
    ));
    assert!(matches!(
        state::timeline(&mut db.conn, 10).await,
        Err(LedgerError::NotInitialized)
    ));

    // Empty: the ledger is there and holds nothing. Nothing to compare
    // against, and nothing to fix either — this is a database that was
    // initialized and then pruned to nothing.
    state::ensure_tables(&mut db.conn).await.expect("create");
    assert!(state::is_initialized(&mut db.conn).await.expect("probe"));
    assert!(matches!(state::latest(&mut db.conn).await, Ok(None)));
    assert_eq!(state::timeline(&mut db.conn, 10).await.unwrap(), Vec::new());

    // Unreachable: not a ledger answer at all. No call in this module can be
    // reached without a connection, and the failure that stops one being opened
    // stays a connection failure with the address in it.
    let error = refusal("host=127.0.0.1 port=1 user=postgres").await;
    assert!(
        matches!(&error, DbError::Connect { addr, .. } if addr == "127.0.0.1:1"),
        "a server that cannot be reached is not an empty ledger: {error:?}"
    );

    // And the fourth answer, which is neither of the three and is measured
    // here because it looks like the first: a database that does not exist is
    // refused **by the server**, so it arrives as a driver error carrying
    // `3D000` (`invalid_catalog_name`) rather than as a failure to connect. It
    // is still not an empty history, which is the only thing any caller may
    // conclude from it.
    let error = refusal(&conn_str_for("no_such_database_here")).await;
    match &error {
        DbError::Driver { code, .. } => assert_eq!(code.as_deref(), Some("3D000"), "{error:?}"),
        // Named rather than wildcarded, as the connection-failure tests name
        // them: a category added later has to be looked at here.
        other @ (DbError::BadConnectionString(_)
        | DbError::Connect { .. }
        | DbError::ConnectTimeout { .. }
        | DbError::WrongSession { .. }
        | DbError::BadRow(_)) => {
            panic!("a database that is not there is refused by the server: {other:?}")
        }
    }

    db.drop().await;
}

/// The distinction the other engine needs a measured error number for, made
/// here by the engine itself — and asserted, because "I could not look"
/// reported as "there is no ledger" ends in `bootstrap` against a database that
/// already has one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_ledger_this_role_may_not_read_is_not_an_uninitialized_one() {
    let mut db = TestDb::create("denied_ledger").await;
    state::ensure_tables(&mut db.conn).await.expect("create");
    state::record(&mut db.conn, &snapshot(StateKind::Baseline))
        .await
        .expect("record");

    let role = least_privilege_role(&mut db, "denied").await;
    let mut theirs = Conn::connect(Driver::Postgres, &conn_str_as(&role, "live-test", &db.name))
        .await
        .expect("connect as the least-privilege role");

    // The premise: the table is there and this role holds nothing on it.
    let error = state::is_initialized(&mut theirs)
        .await
        .expect_err("a role with no privileges must not be told the ledger is absent");
    assert_eq!(sqlstate(&error), "42501", "{error:?}");

    // And the other half of the same question, so the answer cannot be "it
    // errors either way": once the read is granted, the same call says yes.
    db.conn
        .execute(&format!("GRANT SELECT ON {} TO {role}", state::STATE_TABLE))
        .await
        .expect("grant the read");
    assert!(state::is_initialized(&mut theirs).await.expect("probe"));

    // A role that may read the ledger and not the lock is a third state again:
    // nothing holds the lock is what `None` means, and it must not be what
    // "you may not look" means.
    let error = state::lock_holder(&mut theirs)
        .await
        .expect_err("a lock table this role cannot read is not an empty one");
    assert_eq!(sqlstate(&error), "42501", "{error:?}");

    db.drop().await;
}

/// SPEC §11.5: the lock admits exactly one holder, and the second caller is
/// told who has it. The mechanism is this engine's; the invariant is not.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_lock_admits_one_holder_and_names_it_to_the_second() {
    let mut db = TestDb::create("lock").await;
    let mut other = db.second().await;

    state::lock(&mut db.conn, "pipeline-one")
        .await
        .expect("the first caller takes the lock");

    match state::lock(&mut other, "pipeline-two").await {
        Err(LedgerError::Locked(LockInfo {
            locked_by,
            locked_at,
        })) => {
            assert_eq!(locked_by, "pipeline-one");
            assert_eq!(locked_at.len(), 23, "{locked_at}");
            assert!(locked_at.contains('T'), "{locked_at}");
        }
        other => panic!("the second caller must be refused and told who holds it: {other:?}"),
    }

    // Released by whoever runs `pbps unlock`, which is not necessarily the
    // holder — a pipeline that died cannot release its own lock, and that is
    // the case the command exists for.
    assert!(state::unlock(&mut other).await.expect("unlock"));
    assert!(
        !state::unlock(&mut other).await.expect("unlock again"),
        "a lock that was not held must say so rather than reporting a release"
    );
    state::lock(&mut other, "pipeline-two")
        .await
        .expect("the lock is free again");

    db.drop().await;
}

/// The shape the other dialect's lock cannot be ported in: on this engine a
/// failed statement aborts the whole transaction, so "insert, and read the
/// holder when the insert fails" would leave the caller in `25P02` with no
/// holder to name.
///
/// Measured here through the real thing: the lock is taken inside a caller's
/// transaction, the refusal names the holder, and the transaction is still
/// usable afterwards.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_contended_lock_inside_a_transaction_names_its_holder_and_leaves_it_alive() {
    let mut db = TestDb::create("lock_in_tx").await;
    let mut other = db.second().await;
    state::lock(&mut db.conn, "pipeline-one")
        .await
        .expect("take the lock");

    other.execute("BEGIN").await.expect("open a transaction");
    match state::lock(&mut other, "pipeline-two").await {
        Err(LedgerError::Locked(info)) => assert_eq!(info.locked_by, "pipeline-one"),
        other => panic!("expected the holder to be named: {other:?}"),
    }
    // The premise of the whole design: the caller's transaction survived the
    // contention. The bare `INSERT` the other dialect uses would have aborted
    // it, and this read is what proves it did not.
    assert_eq!(number(&mut other, "SELECT 1").await, 1);
    other.execute("ROLLBACK").await.expect("close it");

    db.drop().await;
}

/// ADR-0003: a staged apply's checkpoint is what makes a mid-way failure
/// visible rather than mysterious, and what `--resume` starts from. It only
/// does that if the marker survives `state_json`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_staged_checkpoint_survives_the_ledger() {
    let mut db = TestDb::create("staged").await;

    let mut checkpoint = snapshot(StateKind::Staged);
    checkpoint.plan_checksum = Some("a".repeat(64));
    checkpoint.staged = Some(pbps_model::StagedProgress {
        completed: 1,
        total: 2,
        last_statement: "CREATE INDEX \"ix_live\" ON \"app\".\"customer\" (\"email\")".into(),
    });
    state::record(&mut db.conn, &checkpoint)
        .await
        .expect("record the checkpoint");

    let back = state::latest(&mut db.conn)
        .await
        .expect("latest")
        .expect("an entry");
    assert_eq!(back.snapshot, checkpoint);
    assert!(
        !back
            .snapshot
            .staged
            .expect("the progress marker")
            .is_finished()
    );

    // The closing entry carries no marker, and its absence is what tells every
    // later command the environment is no longer mid-deployment.
    state::record(&mut db.conn, &snapshot(StateKind::Apply))
        .await
        .expect("record the finish");
    assert!(
        state::latest(&mut db.conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .staged
            .is_none()
    );

    db.drop().await;
}

/// Two entries written inside one transaction carry the times they were
/// written, not the time the transaction began.
///
/// `now()` is the transaction's start and does not move inside it — measured,
/// two reads 300ms apart in one transaction return the same value — so a ledger
/// defaulting to it would stamp a staged checkpoint and the entry that follows
/// it with the same instant, and the history would show them as simultaneous.
/// The sleep is what makes the difference bigger than the column's millisecond
/// resolution, so the assertion is about the clock and not about how fast the
/// test ran.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn two_entries_recorded_in_one_transaction_carry_the_times_they_were_written() {
    let mut db = TestDb::create("clock").await;
    state::ensure_tables(&mut db.conn).await.expect("create");

    db.conn.execute("BEGIN").await.expect("open a transaction");
    state::record(&mut db.conn, &snapshot(StateKind::Staged))
        .await
        .expect("the checkpoint");
    db.conn
        .query("SELECT pg_sleep(0.01)")
        .await
        .expect("longer than the column's resolution");
    state::record(&mut db.conn, &snapshot(StateKind::Apply))
        .await
        .expect("the entry that closes it");
    db.conn.execute("COMMIT").await.expect("commit");

    let rows = state::timeline(&mut db.conn, 10).await.expect("timeline");
    assert_eq!(rows.len(), 2);
    assert_ne!(
        rows[0].applied_at, rows[1].applied_at,
        "two entries of one transaction must not read as simultaneous"
    );

    db.drop().await;
}

/// The recorded time is the server's, and it reads the same however the session
/// asking for it renders dates.
///
/// The negative half is the point: the same column cast to text under this
/// session really is unreadable, so the test cannot pass because the setting
/// failed to take.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_recorded_time_reads_the_same_under_a_session_that_renders_dates_differently() {
    let mut db = TestDb::create("datestyle").await;
    state::record(&mut db.conn, &snapshot(StateKind::Baseline))
        .await
        .expect("record");

    db.conn
        .execute("SET DateStyle = 'German, DMY'")
        .await
        .expect("a session that renders dates its own way");

    let back = state::latest(&mut db.conn)
        .await
        .expect("latest")
        .expect("an entry");
    assert!(
        back.applied_at.contains('T') && back.applied_at.len() == 23,
        "the ledger's own rendering must not move with the session: {}",
        back.applied_at
    );

    let cast = text(
        &mut db.conn,
        "SELECT applied_at::text FROM public.__pbps_state ORDER BY id DESC LIMIT 1",
    )
    .await;
    assert!(
        !cast.contains('T') && cast.contains('.'),
        "the premise is wrong if the session setting did not take: {cast}"
    );

    db.drop().await;
}

/// A reason as wide as the column can hold is stored whole, and one character
/// more is refused rather than silently cut. The unit is characters here and
/// UTF-16 code units on the other engine, so this is measured rather than
/// carried over.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_reason_at_the_columns_width_is_stored_whole_and_one_more_is_refused() {
    let mut db = TestDb::create("reason").await;

    // Emoji, because this is exactly where the two engines disagree: 1000 of
    // them are 2000 UTF-16 units and do not fit an `NVARCHAR(1000)`.
    let full = "😀".repeat(state::REASON_CHARS);
    let mut wide = snapshot(StateKind::Baseline);
    wide.reason = Some(full.clone());
    state::record(&mut db.conn, &wide)
        .await
        .expect("a reason of exactly the declared width");
    assert_eq!(
        state::latest(&mut db.conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .reason,
        Some(full.clone())
    );

    let mut over = snapshot(StateKind::Baseline);
    over.reason = Some(format!("{full}😀"));
    let error = state::record(&mut db.conn, &over)
        .await
        .expect_err("one character too many must be refused, not truncated");
    match error {
        LedgerError::Db(e) => assert_eq!(sqlstate(&e), "22001", "{e:?}"),
        other @ (LedgerError::NotInitialized
        | LedgerError::Locked(_)
        | LedgerError::BadEntry { .. }) => panic!("expected the server's refusal: {other:?}"),
    }

    // And the audit paths, which cut rather than fail: what `truncate_reason`
    // leaves fits by construction.
    let mut cut = snapshot(StateKind::Failed);
    cut.reason = Some(state::truncate_reason(&format!("{full}😀")));
    state::record(&mut db.conn, &cut)
        .await
        .expect("a truncated reason fits");

    db.drop().await;
}

/// An entry this build cannot read is refused **by its version**, and it does
/// not erase the history above it (DECISIONS 218, 222). The reader is
/// `StateSnapshot::from_json`, and there is no second path to it.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_entry_from_a_newer_build_is_refused_by_its_version_and_still_appears_in_the_timeline() {
    let mut db = TestDb::create("unreadable").await;
    state::ensure_tables(&mut db.conn).await.expect("create");

    // A row written by a pbps that does not exist yet. Only the version is
    // needed: the reader asks for it before it asks about the shape.
    db.conn
        .execute_with(
            "INSERT INTO public.__pbps_state (kind, state_json, operator, reason) \
             VALUES ('apply', $1, 'a-later-pbps', 'from the future')",
            &[r#"{"version":999}"#.into()],
        )
        .await
        .expect("write the row by hand");

    let error = state::latest(&mut db.conn)
        .await
        .expect_err("a state this build cannot read is not a state");
    match error {
        LedgerError::BadEntry { message, .. } => {
            assert!(message.contains("999"), "{message}");
        }
        other @ (LedgerError::Db(_) | LedgerError::NotInitialized | LedgerError::Locked(_)) => {
            panic!("expected the entry to be refused by its version: {other:?}")
        }
    }

    // The timeline is the other question: every row of it exists whether or not
    // this build understands the snapshot inside, or "when was this database
    // last applied to?" is answered with an error.
    let rows = state::timeline(&mut db.conn, 10).await.expect("timeline");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].operator, "a-later-pbps");
    assert_eq!(rows[0].kind, "apply");
    assert_eq!(rows[0].reason.as_deref(), Some("from the future"));
    assert!(
        matches!(&rows[0].state, Err(Unreadable::UnsupportedVersion(m)) if m.contains("999")),
        "{:?}",
        rows[0].state
    );

    // And a readable row above it is still readable: one bad row must not take
    // the history with it.
    state::record(&mut db.conn, &snapshot(StateKind::Apply))
        .await
        .expect("record");
    let rows = state::timeline(&mut db.conn, 10).await.expect("timeline");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].state.is_ok(), "{:?}", rows[0].state);
    assert!(rows[1].state.is_err(), "{:?}", rows[1].state);

    // `history` is not the timeline and must not pretend to be: a caller
    // asking for states wants states, and half a state is not one.
    assert!(matches!(
        state::history(&mut db.conn, 10).await,
        Err(LedgerError::BadEntry { .. })
    ));

    db.drop().await;
}

/// `ensure_tables` is run by every command that writes a ledger row, so two
/// pipelines starting together really do race on it. `IF NOT EXISTS` is not
/// atomic; the loser sees the table it wanted.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn two_pipelines_creating_the_ledger_at_once_both_find_it_there() {
    let mut db = TestDb::create("concurrent_create").await;
    let mut other = db.second().await;

    let (first, second) = tokio::join!(
        state::ensure_tables(&mut db.conn),
        state::ensure_tables(&mut other)
    );
    first.expect("the first creator");
    second.expect("the second creator must find the table, not a duplicate-key failure");

    assert!(state::is_initialized(&mut db.conn).await.expect("probe"));
    assert!(
        state::lock_holder(&mut other)
            .await
            .expect("read the lock")
            .is_none()
    );

    db.drop().await;
}

/// The loser of the creation race, inside a transaction of its own.
///
/// `record` runs inside the apply's transaction (DECISIONS 147), so this is the
/// shape that matters: measured on 18.6, the loser's `CREATE TABLE IF NOT
/// EXISTS` returns `23505` **and aborts its transaction**, so tolerating that
/// error without rolling back to a savepoint hands the caller a connection
/// whose every next statement is `25P02: current transaction is aborted` — a
/// deployment failing on a race the code claims to have handled.
///
/// The race is made rather than hoped for: the winner holds its transaction
/// open until the loser is already blocked on the relation lock.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_loser_of_the_creation_race_keeps_the_transaction_it_was_called_in() {
    let mut db = TestDb::create("create_race").await;
    let mut winner = db.second().await;

    winner.execute("BEGIN").await.expect("the winner's own");
    state::ensure_tables(&mut winner)
        .await
        .expect("the winner creates the pair");

    // The loser starts while the winner still holds the tables uncommitted: its
    // catalog probe sees nothing, and its `CREATE` blocks on the winner's lock.
    db.conn.execute("BEGIN").await.expect("the loser's own");
    let (committed, lost) = tokio::join!(
        async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            winner.execute("COMMIT").await
        },
        state::ensure_tables(&mut db.conn)
    );
    committed.expect("the winner commits");
    lost.expect("the loser must find the table, not a duplicate-key failure");

    // The point of the fix: the loser's transaction is still its own.
    assert_eq!(number(&mut db.conn, "SELECT 1").await, 1);
    let id = state::record(&mut db.conn, &snapshot(StateKind::Bootstrap))
        .await
        .expect("and it can still do the work it opened the transaction for");
    assert!(id > 0);
    db.conn.execute("COMMIT").await.expect("commit");

    assert_eq!(
        state::timeline(&mut db.conn, 10)
            .await
            .expect("timeline")
            .len(),
        1
    );

    db.drop().await;
}

/// Every answer this module gives by *tolerating an error* has to leave the
/// caller's transaction where it found it.
///
/// Measured on 18.6, each of these raises inside the call and is handled by it:
/// `is_initialized` on a database with no ledger is `42P01`, and so are
/// `unlock` and `lock_holder` against a missing lock table. An error aborts the
/// transaction it happens in, so without a savepoint each of them would hand
/// back `Ok(...)` on a connection whose next statement is `25P02` and whose
/// `COMMIT` is a `ROLLBACK` — and the ledger is read inside the apply's own
/// transaction (DECISIONS 147).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_expected_failure_leaves_the_callers_transaction_where_it_found_it() {
    let mut db = TestDb::create("expected_failures").await;

    db.conn.execute("BEGIN").await.expect("the caller's own");
    assert!(!state::is_initialized(&mut db.conn).await.expect("probe"));
    assert_eq!(
        number(&mut db.conn, "SELECT 1").await,
        1,
        "the absent-ledger answer must not cost the caller its transaction"
    );
    assert!(matches!(
        state::latest(&mut db.conn).await,
        Err(LedgerError::NotInitialized)
    ));
    assert_eq!(number(&mut db.conn, "SELECT 1").await, 1);
    assert!(
        state::lock_holder(&mut db.conn)
            .await
            .expect("no lock table is not a held lock")
            .is_none()
    );
    assert_eq!(number(&mut db.conn, "SELECT 1").await, 1);
    assert!(
        !state::unlock(&mut db.conn)
            .await
            .expect("nothing to unlock")
    );
    assert_eq!(number(&mut db.conn, "SELECT 1").await, 1);

    // And the transaction is still the caller's to finish: the work it opened
    // it for commits.
    db.conn
        .execute("CREATE TABLE public.the_callers_own (id integer)")
        .await
        .expect("the caller's own work");
    db.conn.execute("COMMIT").await.expect("commit");
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT count(*)::int FROM pg_class WHERE relname = 'the_callers_own'"
        )
        .await,
        1,
        "a COMMIT after a handled failure must commit, not roll back"
    );

    db.drop().await;
}

/// `42P07` names *a* relation the DDL would create, not the ledger, so the
/// answer is not taken from the error.
///
/// Measured on 18.6: with an unrelated `public.pk___pbps_state` — the name this
/// DDL gives the ledger's primary key, and an ordinary name for something else
/// to have — `CREATE TABLE IF NOT EXISTS public.__pbps_state` fails with
/// `42P07` and the ledger is **still absent**. Read as "somebody else created
/// it", that is `ensure_tables` reporting success over a database with no
/// ledger.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_name_collision_that_is_not_the_ledger_is_not_reported_as_a_ledger() {
    let mut db = TestDb::create("name_collision").await;
    db.conn
        .execute("CREATE TABLE public.pk___pbps_state (x integer)")
        .await
        .expect("something else holding the primary key's name");

    let error = state::ensure_tables(&mut db.conn)
        .await
        .expect_err("a ledger that was not created is not a ledger");
    assert_eq!(sqlstate(&error), "42P07", "{error:?}");
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT count(*)::int FROM pg_class WHERE relname = '__pbps_state'"
        )
        .await,
        0,
        "the premise is wrong if the table was created after all"
    );
    // And the caller is told what to do about it by the engine's own words:
    // the name in the error is the squatter's, not the ledger's.
    assert!(state::latest(&mut db.conn).await.is_err());

    db.drop().await;
}

/// `doctor` against a **real least-privilege role**, which is the only way this
/// answer means anything: `postgres` is a superuser and passes every question
/// without the catalog being asked.
///
/// The measurement this test exists for: every privilege PostgreSQL has to give
/// on a table, held, and not one statement of a plan can run. A readiness check
/// that asked only about privileges would call this environment ready.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn doctor_reads_a_real_version_and_a_permission_set_ownership_decides() {
    let mut db = TestDb::create("doctor").await;
    let role = least_privilege_role(&mut db, "doctor").await;

    db.conn
        .execute(&format!(
            "CREATE SCHEMA app; \
             CREATE TABLE app.customer (id integer PRIMARY KEY, email text); \
             GRANT USAGE, CREATE ON SCHEMA app TO {role}; \
             GRANT ALL PRIVILEGES ON app.customer TO {role}"
        ))
        .await
        .expect("a schema this role may use and a table it does not own");

    let mut theirs = Conn::connect(Driver::Postgres, &conn_str_as(&role, "live-test", &db.name))
        .await
        .expect("connect as the least-privilege role");

    let version = doctor::server_version(&mut theirs)
        .await
        .expect("the server's version");
    assert!(version.number >= 180_000, "{version:?}");
    assert!(version.text.starts_with("18."), "{version:?}");

    let customer = ObjectName {
        schema: "app".to_owned(),
        name: "customer".to_owned(),
    };
    let ask = doctor::Ask {
        managed_schemas: &["app".to_owned()],
        managed_tables: std::slice::from_ref(&customer),
        referenced: &[],
    };
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read what this role holds");

    // The premise, asserted rather than assumed: the role really does hold
    // every table privilege, and really does not own the table.
    let rights = &held.tables[&customer];
    assert!(rights.privileges.contains("SELECT"), "{rights:?}");
    assert!(rights.privileges.contains("INSERT"), "{rights:?}");
    assert!(rights.privileges.contains("REFERENCES"), "{rights:?}");
    assert!(!rights.owned, "{rights:?}");

    // And the engine agrees with the gap: this is the statement a plan is made
    // of, run by the role the check just described.
    let refused = theirs
        .execute("ALTER TABLE app.customer ADD COLUMN note text")
        .await
        .expect_err("a non-owner must not be able to alter the table");
    assert_eq!(sqlstate(&refused), "42501", "{refused:?}");

    let gaps = doctor::missing(&held);
    assert!(
        gaps.iter()
            .any(|g| g.permission == doctor::OWNERSHIP
                && g.securable() == "TABLE \"app\".\"customer\""),
        "the ownership gap is what makes this check worth running: {gaps:?}"
    );
    // The ledger's create-time gap, which every fresh PostgreSQL has: since 15,
    // `public` grants `USAGE` to every role and `CREATE` to none.
    assert!(
        gaps.iter()
            .any(|g| g.permission == "CREATE" && g.securable() == "SCHEMA \"public\""),
        "{gaps:?}"
    );

    // Both remedies applied, and the gaps go: ownership is transferred, the
    // ledger's schema is opened, and the ledger is created by the role itself
    // — which then owns it, so the writes need no further grant.
    db.conn
        .execute(&format!(
            "ALTER TABLE app.customer OWNER TO {role}; \
             GRANT CREATE ON SCHEMA public TO {role}"
        ))
        .await
        .expect("apply the remedies");
    state::ensure_tables(&mut theirs)
        .await
        .expect("the role can now create the ledger it was told it could not");
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read again");
    assert_eq!(
        doctor::missing(&held),
        Vec::new(),
        "a role granted exactly what the report named must pass"
    );

    db.drop().await;
}

/// The least-privilege configuration SPEC §8.1 asks for, run end to end: a DBA
/// creates the ledger, grants the deployment role the DML on those two tables
/// and nothing else, and the role deploys.
///
/// Two things are pinned here and both were found by review.
///
/// `ensure_tables` must send no DDL when there is nothing to create. Measured
/// on 18.6, `CREATE TABLE IF NOT EXISTS public.__pbps_state` is
/// `42501: permission denied for schema public` for this role **even though the
/// table is already there** — the engine checks the schema privilege before it
/// notices. Every `record` and every `lock` calls it, so the whole deployment
/// failed on a grant `doctor` had correctly reported as spent.
///
/// And the identity column needs no sequence privilege. The ledger's `id` is
/// `GENERATED ALWAYS AS IDENTITY`, and measured, this role holds no `USAGE` on
/// `__pbps_state_id_seq` and the insert returns its id — where a `serial`
/// column would be `permission denied for sequence` (ADR-0010 §7).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_role_that_may_write_the_ledger_and_not_create_it_deploys() {
    let mut db = TestDb::create("ledger_dml_only").await;
    let role = least_privilege_role(&mut db, "dmlonly").await;

    // The DBA's half: the ledger exists, and the role is granted exactly what
    // writing it needs.
    state::ensure_tables(&mut db.conn).await.expect("create");
    db.conn
        .execute(&format!(
            "GRANT SELECT, INSERT, DELETE ON {}, {} TO {role}",
            state::STATE_TABLE,
            state::LOCK_TABLE
        ))
        .await
        .expect("grant the ledger's DML");

    let mut theirs = Conn::connect(Driver::Postgres, &conn_str_as(&role, "live-test", &db.name))
        .await
        .expect("connect as the least-privilege role");

    // The premise, asserted rather than assumed: no `CREATE` on the schema, and
    // no privilege on the identity's sequence.
    assert!(
        !truth(
            &mut theirs,
            "SELECT has_schema_privilege('public', 'CREATE')"
        )
        .await
    );
    assert!(
        !truth(
            &mut theirs,
            "SELECT has_sequence_privilege('public.__pbps_state_id_seq', 'USAGE')"
        )
        .await
    );

    let id = state::record(&mut theirs, &snapshot(StateKind::Apply))
        .await
        .expect("a role granted the ledger's DML must be able to record a state");
    assert!(id > 0);
    state::lock(&mut theirs, "pipeline-one")
        .await
        .expect("and to take the lock");
    assert!(state::unlock(&mut theirs).await.expect("and release it"));

    // And `doctor` agrees with the engine about this configuration: nothing
    // missing, with the create-time privilege spent and not asked for.
    let ask = doctor::Ask {
        managed_schemas: &[],
        managed_tables: &[],
        referenced: &[],
    };
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read permissions");
    assert_eq!(doctor::missing(&held), Vec::new(), "{held:?}");

    db.drop().await;
}

/// `USAGE` on the ledger's schema is not spent when the ledger is created, and
/// no grant on the two tables confers it.
///
/// Measured on 18.6 and the reason this is asked at all: with `USAGE` revoked,
/// `has_table_privilege` still answers `true` — it is asked by oid and never
/// resolves the name — while every statement that names the ledger is
/// `42501: permission denied for schema public`. A check that asked only about
/// the two objects called this environment ready.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_ledger_whose_schema_is_closed_is_a_gap_however_the_tables_are_granted() {
    let mut db = TestDb::create("ledger_usage").await;
    let role = least_privilege_role(&mut db, "usage").await;
    state::ensure_tables(&mut db.conn).await.expect("create");
    db.conn
        .execute(&format!(
            "GRANT SELECT, INSERT, DELETE ON {}, {} TO {role}; \
             REVOKE USAGE ON SCHEMA public FROM {role}; \
             REVOKE USAGE ON SCHEMA public FROM PUBLIC",
            state::STATE_TABLE,
            state::LOCK_TABLE
        ))
        .await
        .expect("grant the tables and close the schema");

    let mut theirs = Conn::connect(Driver::Postgres, &conn_str_as(&role, "live-test", &db.name))
        .await
        .expect("connect as the least-privilege role");

    let ask = doctor::Ask {
        managed_schemas: &[],
        managed_tables: &[],
        referenced: &[],
    };
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read permissions");
    // The premise: the table privileges really do read as held.
    for table in doctor::ledger_tables() {
        assert!(
            held.ledger_objects[&table].privileges.contains("INSERT"),
            "{held:?}"
        );
    }
    let gaps = doctor::missing(&held);
    assert!(
        gaps.iter()
            .any(|g| g.permission == "USAGE" && g.securable() == "SCHEMA \"public\""),
        "{gaps:?}"
    );

    // And the engine agrees: the statement the grants describe cannot run.
    let refused = state::record(&mut theirs, &snapshot(StateKind::Apply))
        .await
        .expect_err("a closed schema is not a writable ledger");
    match refused {
        LedgerError::Db(e) => assert_eq!(sqlstate(&e), "42501", "{e:?}"),
        other @ (LedgerError::NotInitialized
        | LedgerError::Locked(_)
        | LedgerError::BadEntry { .. }) => panic!("expected the server's refusal: {other:?}"),
    }

    db.drop().await;
}

/// A foreign key into a **partitioned** table outside the managed schemas.
///
/// The model cannot hold a partitioned table and the pull names it as
/// unexpressible, which is what made `relkind = 'r'` look like the right filter
/// everywhere. It is not the right filter for somebody else's table: measured
/// on 18.6, a key into one is an ordinary declaration, authorized on the parent
/// like any other, and read at `r` alone the target came back *absent* — so
/// `doctor` demanded nothing and the next `apply` was refused.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_foreign_key_into_a_partitioned_table_asks_for_the_grants_that_key_needs() {
    let mut db = TestDb::create("partitioned_fk").await;
    let role = least_privilege_role(&mut db, "fk").await;
    db.conn
        .execute(&format!(
            "CREATE SCHEMA shared; \
             CREATE TABLE shared.parent (id integer PRIMARY KEY, v text) PARTITION BY RANGE (id); \
             CREATE TABLE shared.parent_lo PARTITION OF shared.parent FOR VALUES FROM (0) TO (100); \
             CREATE SCHEMA app; ALTER SCHEMA app OWNER TO {role}; \
             GRANT USAGE ON SCHEMA shared TO {role}; \
             GRANT USAGE, CREATE ON SCHEMA app TO {role}"
        ))
        .await
        .expect("a partitioned table in a schema this project does not manage");

    let mut theirs = Conn::connect(Driver::Postgres, &conn_str_as(&role, "live-test", &db.name))
        .await
        .expect("connect as the least-privilege role");

    let parent = ObjectName {
        schema: "shared".to_owned(),
        name: "parent".to_owned(),
    };
    let ask = doctor::Ask {
        managed_schemas: &["app".to_owned()],
        managed_tables: &[],
        referenced: std::slice::from_ref(&parent),
    };
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read permissions");
    // The premise: the target is seen at all. Read at `r` alone it was absent,
    // and an absent target is asked for nothing.
    assert!(held.referenced_objects.contains_key(&parent), "{held:?}");

    let gaps = doctor::missing(&held);
    for permission in ["REFERENCES", "SELECT"] {
        assert!(
            gaps.iter()
                .any(|g| g.permission == permission
                    && g.securable() == "TABLE \"shared\".\"parent\""),
            "{permission} on the referenced parent: {gaps:?}"
        );
    }

    // And the engine agrees, in both directions.
    let refused = theirs
        .execute("CREATE TABLE app.child (id integer PRIMARY KEY, pid integer REFERENCES shared.parent(id))")
        .await
        .expect_err("a key into a table this role may not reference");
    assert_eq!(sqlstate(&refused), "42501", "{refused:?}");

    db.conn
        .execute(&format!(
            "GRANT REFERENCES, SELECT ON shared.parent TO {role}"
        ))
        .await
        .expect("grant what the report named");
    let held = doctor::permissions(&mut theirs, &ask)
        .await
        .expect("read again");
    let gaps = doctor::missing(&held);
    // Only the referenced target is this test's business: this database has no
    // ledger yet, so the create-time gap on its schema is a true report about
    // something else.
    assert!(
        !gaps.iter().any(|g| g.securable().contains("shared")),
        "{gaps:?}"
    );
    theirs
        .execute("CREATE TABLE app.child (id integer PRIMARY KEY, pid integer REFERENCES shared.parent(id))")
        .await
        .expect("the key the report said was now possible");

    db.drop().await;
}

/// A managed schema that is not there is not a permission problem, and not
/// nothing either: the emitter never writes `CREATE SCHEMA`, so the first
/// statement of the plan would fail.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_managed_schema_that_is_absent_is_reported_as_absent_and_not_as_a_gap() {
    let mut db = TestDb::create("absent_schema").await;

    let ask = doctor::Ask {
        managed_schemas: &["not_here".to_owned()],
        managed_tables: &[],
        referenced: &[],
    };
    let held = doctor::permissions(&mut db.conn, &ask)
        .await
        .expect("read permissions");
    assert!(held.absent_schemas.contains("not_here"), "{held:?}");
    assert!(held.schemas.is_empty(), "{held:?}");
    assert!(
        !doctor::missing(&held)
            .iter()
            .any(|g| g.securable().contains("not_here")),
        "there is nothing to grant on a schema that does not exist"
    );

    db.drop().await;
}

/// The pins a staged apply needs, which no `BEGIN` establishes for it.
///
/// Both halves are measured: the pins really do reach the connection, and the
/// `SET LOCAL` they are deliberately not spelled as really would do nothing
/// outside a transaction — which is how a pin that looks right silently stops
/// pinning anything.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_settings_a_staged_run_depends_on_reach_a_connection_that_opens_no_transaction() {
    let mut conn = connect().await;
    conn.execute("SET standard_conforming_strings = off; SET DateStyle = 'German, DMY'")
        .await
        .expect("a session that means something else by the same text");

    let pins = Postgres::new()
        .session_pins()
        .expect("this dialect pins its session");
    conn.execute(pins).await.expect("pin the session");
    assert_eq!(
        text(&mut conn, "SHOW standard_conforming_strings").await,
        "on"
    );
    assert_eq!(text(&mut conn, "SHOW DateStyle").await, "ISO, MDY");

    // The negative case, on the same connection: `SET LOCAL` outside a
    // transaction lasts exactly as long as the statement it is in.
    conn.execute("SET LOCAL DateStyle = 'German, DMY'")
        .await
        .expect("the engine accepts it and warns");
    assert_eq!(
        text(&mut conn, "SHOW DateStyle").await,
        "ISO, MDY",
        "`SET LOCAL` outside a transaction is a no-op, which is why the pins are not spelled \
         that way"
    );
}

// ---- modules (issue #80, ADR-0009) ----

/// Only this test's own modules, so that a comparison says something about
/// what this test created and nothing about the shared container.
fn our_modules(
    pulled: &pbps_pg::introspect::Pulled,
    schema: &str,
) -> std::collections::BTreeMap<pbps_model::ModuleId, pbps_model::Module> {
    pulled
        .schema
        .modules
        .iter()
        .filter(|(id, _)| id.schema() == schema)
        .map(|(id, m)| (id.clone(), m.clone()))
        .collect()
}

fn module(kind: pbps_model::ModuleKind, definition: &str) -> pbps_model::Module {
    pbps_model::Module {
        kind,
        description: None,
        definition: definition.to_owned(),
    }
}

/// A table and one module of every kind this model holds, in one schema.
fn schema_with_modules(s: &str) -> Schema {
    let mut schema = Schema::default();
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    t.columns.insert("a".into(), Column::new(ty("text")));
    t.primary_key = Some(PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
    });
    schema.tables.insert(TableName::new(s, "t"), t);

    schema.modules.insert(
        format!("{s}.v").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT id, a FROM {s}.t"),
        ),
    );
    schema.modules.insert(
        format!("{s}.f(integer)").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "(n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n + 1 $$",
        ),
    );
    schema.modules.insert(
        format!("{s}.p(integer)").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Procedure,
            &format!("(n integer) LANGUAGE sql AS $$ INSERT INTO {s}.t (id) VALUES (n) $$"),
        ),
    );
    schema.modules.insert(
        format!("{s}.trf()").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
        ),
    );
    schema.modules.insert(
        format!("{s}.t.audit").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Trigger,
            &format!("AFTER INSERT ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trf()"),
        ),
    );
    schema
}

/// The round trip issue #80 asks for first: a module of every kind planned,
/// applied, and read back **as the same object**.
///
/// The identity is what is asserted, not the text — ADR-0009 §2 measured that
/// this engine does not store what it was given, so a declaration and its
/// read-back are two spellings of one module and §2.2 keeps them in separate
/// comparisons. What must survive exactly is the key: a routine read back under
/// a signature the declaration did not write is a routine whose next `DROP`
/// names another object.
///
/// The second half is what `bootstrap` needs and nothing else proves: the text
/// the engine gave back is applied again, and the engine gives back the same
/// text. A deparsed definition that cannot be re-emitted is a state this tool
/// can record and never restore.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_module_of_every_kind_survives_the_round_trip_as_the_same_object() {
    let s = emit_schema("modules");
    let declared = schema_with_modules(&s);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let first = our_modules(&pull(&mut conn).await, &s);
    assert_eq!(
        first.keys().cloned().collect::<Vec<_>>(),
        declared.modules.keys().cloned().collect::<Vec<_>>(),
        "the pull did not read back the objects the plan created"
    );
    for (id, m) in &first {
        assert_eq!(
            m.kind, declared.modules[id].kind,
            "`{id}` came back as another kind"
        );
        assert!(!m.definition.trim().is_empty(), "`{id}` came back empty");
    }

    // Re-applied from the read-back, which is what `bootstrap` does with a
    // recorded state. Every module is dropped and created again — this engine
    // has no other shape (ADR-0009 §3) — so the order matters and the trigger
    // has to go before the function it calls.
    let rebuild = Schema {
        modules: first.clone(),
        ..Schema::default()
    };
    let statements: Vec<_> = plan(&Schema::default(), &IdsFile::default(), &rebuild, &ids)
        .changes
        .iter()
        .map(|p| p.change.clone())
        .collect();
    assert!(!statements.is_empty(), "nothing to rebuild");
    for id in first.keys().rev() {
        let kind = first[id].kind;
        for stmt in pg
            .emit(
                &pbps_model::Change::DropModule {
                    id: id.clone(),
                    kind,
                },
                pbps_model::Strategy::default(),
            )
            .expect("emit a drop")
        {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("dropping `{id}`:\n{}\n{e}", stmt.sql));
        }
    }
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &rebuild, &ids),
    )
    .await;

    let second = our_modules(&pull(&mut conn).await, &s);
    drop_schema(&mut conn, &s).await;
    assert_eq!(
        second, first,
        "the text this engine deparsed did not rebuild into the same object"
    );
}

/// ADR-0009 §1's whole reason for a typed `ModuleId`: on this engine a name is
/// not an identity. Two overloads are declared, applied and read back as **two**
/// objects, and each `DROP` names exactly one of them.
///
/// The negative half is the one that matters: `DROP FUNCTION` without a
/// signature is refused by the engine where more than one overload exists, so a
/// dialect that answered `overloads → false` would emit a statement that cannot
/// run.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn two_overloads_of_one_name_are_two_objects_and_each_drop_names_one() {
    let s = emit_schema("overload");
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.f(integer)").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "(n integer) RETURNS text LANGUAGE sql AS $$ SELECT 'integer overload' $$",
        ),
    );
    declared.modules.insert(
        format!("{s}.f(text)").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "(n text) RETURNS text LANGUAGE sql AS $$ SELECT 'text overload' $$",
        ),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();

    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let pulled = our_modules(&pull(&mut conn).await, &s);
    assert_eq!(
        pulled.keys().cloned().collect::<Vec<_>>(),
        declared.modules.keys().cloned().collect::<Vec<_>>(),
        "two overloads did not read back as two objects"
    );

    // The engine's own answer to the name-only question, which is why the
    // signature is in the key.
    let refused = conn
        .execute(&format!("DROP FUNCTION {s}.f"))
        .await
        .expect_err("a bare name names two functions");
    assert_eq!(sqlstate(&refused), "42725", "{refused:?}");

    // And with the signature, exactly one goes.
    for stmt in pg
        .emit(
            &pbps_model::Change::DropModule {
                id: format!("{s}.f(integer)").parse().expect("a module id"),
                kind: pbps_model::ModuleKind::Function,
            },
            pbps_model::Strategy::default(),
        )
        .expect("emit")
    {
        conn.execute(&stmt.sql).await.expect("drop one overload");
    }
    let left = our_modules(&pull(&mut conn).await, &s);
    drop_schema(&mut conn, &s).await;
    assert_eq!(
        left.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec![format!("{s}.f(text)")],
        "the drop with a signature took the wrong object, or both"
    );
}

/// Opens a transaction, because [`pbps_pg::modules::before_a_rebuild`] takes
/// the object's lock and holds it through the rebuild — outside a transaction
/// the lock would be released at the end of the statement that took it.
/// A rebuild's carried state, read in its own transaction, retried when the
/// engine breaks a lock cycle.
///
/// `before_a_rebuild` takes `ACCESS EXCLUSIVE` on the object it is about to
/// replace (ADR-0009 §3). Every other test in this file pulls, and a pull
/// deparses every view in the database — `pg_get_viewdef` opens each one, so
/// the read holds `ACCESS SHARE` on relations this test wants exclusively.
/// Measured from the server log, the two orders cross and the engine kills a
/// side:
///
/// ```text
/// deadlock detected
/// Process A: LOCK TABLE "…_carried"."granted" IN ACCESS EXCLUSIVE MODE
/// Process B: SELECT … pg_get_viewdef(c.oid, true) …
/// ```
///
/// Which side it kills is its choice, so both have to be able to lose. Outside
/// this suite a deployer holds the deployment lock and a pull is a moment;
/// here fifty tests read the catalog while a handful lock objects, and that is
/// the suite's own doing rather than anything the product has to answer for.
/// What the product owes is the message, and [`deadlocked`] is what checks it.
async fn read_a_rebuild(
    conn: &mut Conn,
    id: &pbps_model::ModuleId,
    kind: pbps_model::ModuleKind,
) -> pbps_pg::modules::Rebuild {
    for _ in 0..20 {
        in_a_transaction(conn).await;
        match pbps_pg::modules::before_a_rebuild(conn, id, kind).await {
            Ok(rebuild) => {
                rollback(conn).await;
                return rebuild;
            }
            Err(e) => {
                rollback(conn).await;
                assert!(deadlocked(&e), "{id}: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    panic!("twenty rebuild reads in a row lost a lock cycle to another test");
}

/// Whether the engine broke a lock cycle and this side lost.
fn deadlocked(e: &DbError) -> bool {
    matches!(e, DbError::Driver { code, .. } if code.as_deref() == Some("40P01"))
}

/// `DROP SCHEMA … CASCADE`, retried for the same reason.
///
/// The teardown takes `ACCESS EXCLUSIVE` on everything in the schema, which
/// includes its views — so a pull in another test is exactly the other half of
/// the cycle above.
async fn drop_schema(conn: &mut Conn, schema: &str) {
    for _ in 0..20 {
        match conn
            .execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
            .await
        {
            Ok(()) => return,
            Err(e) => {
                assert!(deadlocked(&e), "drop {schema}: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    panic!("twenty drops in a row lost a lock cycle to another test");
}

async fn in_a_transaction(conn: &mut Conn) {
    conn.execute("BEGIN").await.expect("begin");
}

async fn rollback(conn: &mut Conn) {
    conn.execute("ROLLBACK").await.expect("rollback");
}

/// ADR-0009 §3's whole finding, measured end to end: `CREATE OR ALTER` was
/// buying grant preservation, and this engine will not sell it — so an object
/// carrying anything the declarations cannot reproduce refuses the rebuild
/// **by name** rather than reporting success while quietly changing who may use
/// it.
///
/// Six carried things, each measured on this branch and each in ADR-0009 §3's
/// table: a grant, a revocation from `PUBLIC` (which is the *absence* of a row
/// and the more dangerous of the two directions), `reloptions`, a view column
/// default in `pg_attrdef`, a trigger's `tgenabled`, and grants the *new*
/// object would arrive with from `pg_default_acl`. The last is the one every
/// earlier version of the ADR missed: an object with no grants at all is the
/// easiest case to wave through, and it is the one where a rebuild hands an
/// unmanaged role `SELECT`.
///
/// Roles and grants are Phase 5 step 6, so today every one of these is a
/// refusal; the step that adds them narrows this to what the declarations still
/// cannot reproduce, and ADR-0010 §5 keeps `PUBLIC` on the refusing side for
/// good.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_module_carrying_what_a_rebuild_would_destroy_refuses_and_names_it() {
    let s = emit_schema("carried");
    let reader = format!("{s}_reader");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("DROP ROLE IF EXISTS {reader}"),
        format!("CREATE ROLE {reader}"),
        format!("CREATE TABLE {s}.t (id int primary key, a text)"),
        format!("CREATE VIEW {s}.plain AS SELECT id FROM {s}.t"),
        format!("CREATE VIEW {s}.granted AS SELECT id FROM {s}.t"),
        format!("GRANT SELECT ON {s}.granted TO {reader}"),
        // The grant that is not in the object's own ACL. Measured, `relacl`
        // stays NULL and the privilege lives in `pg_attribute.attacl`, so an
        // object-level check reports nothing carried and the rebuild removes
        // it.
        format!("CREATE VIEW {s}.column_granted AS SELECT id, a FROM {s}.t"),
        format!("GRANT SELECT (a) ON {s}.column_granted TO {reader}"),
        format!("CREATE VIEW {s}.optioned WITH (security_invoker = true) AS SELECT id FROM {s}.t"),
        format!("CREATE VIEW {s}.defaulted AS SELECT id, a FROM {s}.t"),
        format!("ALTER VIEW {s}.defaulted ALTER COLUMN a SET DEFAULT 'from the view'"),
        format!("CREATE FUNCTION {s}.open(n int) RETURNS int LANGUAGE sql AS $$ SELECT n $$"),
        format!("CREATE FUNCTION {s}.closed(n int) RETURNS int LANGUAGE sql AS $$ SELECT n $$"),
        format!("REVOKE EXECUTE ON FUNCTION {s}.closed(int) FROM PUBLIC"),
        format!(
            "CREATE FUNCTION {s}.trf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; \
             END $$"
        ),
        format!(
            "CREATE TRIGGER live AFTER INSERT ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trf()"
        ),
        format!("CREATE TRIGGER off AFTER UPDATE ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trf()"),
        format!("ALTER TABLE {s}.t DISABLE TRIGGER off"),
        // A note somebody wrote, on each of the three kinds and on a view's
        // column. Measured, `pg_get_viewdef` and `pg_get_functiondef` do not
        // carry it and a drop-and-create loses it outright — and nothing in
        // this project writes `COMMENT ON`, so there is nothing to put it
        // back.
        format!("CREATE VIEW {s}.noted AS SELECT id, a FROM {s}.t"),
        format!("COMMENT ON VIEW {s}.noted IS 'why this view exists'"),
        format!("CREATE VIEW {s}.column_noted AS SELECT id, a FROM {s}.t"),
        format!("COMMENT ON COLUMN {s}.column_noted.a IS 'what this column means'"),
        format!("CREATE FUNCTION {s}.noted_fn(n int) RETURNS int LANGUAGE sql AS $$ SELECT n $$"),
        format!("COMMENT ON FUNCTION {s}.noted_fn(int) IS 'why this routine exists'"),
        format!(
            "CREATE TRIGGER noted AFTER DELETE ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trf()"
        ),
        format!("COMMENT ON TRIGGER noted ON {s}.t IS 'why this trigger exists'"),
        // A security label. Written into `pg_seclabel` directly because no
        // label provider is loaded in this image — `SECURITY LABEL` answers
        // `no security label providers have been loaded` — and the honest
        // route needs one. The same reason, and the same precedent, as the
        // `pg_index` write in the limits fixture: what is under test is what
        // the reader does with the row, and the row is what the provider would
        // have written. Measured, a `DROP` takes it.
        format!("CREATE VIEW {s}.labelled AS SELECT id FROM {s}.t"),
        format!(
            "INSERT INTO pg_catalog.pg_seclabel (objoid, classoid, objsubid, provider, label)
             VALUES ('{s}.labelled'::regclass, 'pg_class'::regclass, 0, 'a_provider', \
             'classified')"
        ),
        // And an outgoing tie the definition does not carry: measured,
        // `pg_get_functiondef` does not write `DEPENDS ON EXTENSION`, so a
        // rebuild creates a routine that outlives the extension it was tied
        // to. `plpgsql` because it is installed in every database — the tie
        // points *at* the extension, so dropping this schema leaves it alone.
        format!("CREATE FUNCTION {s}.tied(n int) RETURNS int LANGUAGE sql AS $$ SELECT n $$"),
        format!("ALTER FUNCTION {s}.tied(int) DEPENDS ON EXTENSION plpgsql"),
        format!(
            "CREATE TRIGGER tied AFTER TRUNCATE ON {s}.t FOR EACH STATEMENT \
             EXECUTE FUNCTION {s}.trf()"
        ),
        format!("ALTER TRIGGER tied ON {s}.t DEPENDS ON EXTENSION plpgsql"),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    use pbps_model::ModuleKind::{Function, Trigger, View};
    let cases: Vec<(String, pbps_model::ModuleKind, Option<&str>)> = vec![
        // Nothing attached: this is what a rebuild is allowed to do.
        (format!("{s}.plain"), View, None),
        (format!("{s}.granted"), View, Some(&reader)),
        (
            format!("{s}.column_granted"),
            View,
            Some("column `a` is granted"),
        ),
        (format!("{s}.optioned"), View, Some("security_invoker")),
        (format!("{s}.defaulted"), View, Some("from the view")),
        (format!("{s}.open(integer)"), Function, None),
        // A revocation is not a row in the ACL — it is the absence of the
        // engine's default — so a rebuild restores the default and silently
        // reopens a function somebody deliberately closed.
        (format!("{s}.closed(integer)"), Function, Some("=X/")),
        (format!("{s}.t.live"), Trigger, None),
        (format!("{s}.t.off"), Trigger, Some("disabled")),
        // A comment is carried state on every kind, and on a column of one.
        (format!("{s}.noted"), View, Some("why this view exists")),
        (
            format!("{s}.column_noted"),
            View,
            Some("on column `a`: what this column means"),
        ),
        (
            format!("{s}.noted_fn(integer)"),
            Function,
            Some("why this routine exists"),
        ),
        (
            format!("{s}.t.noted"),
            Trigger,
            Some("why this trigger exists"),
        ),
        (
            format!("{s}.labelled"),
            View,
            Some("a_provider: classified"),
        ),
        (
            format!("{s}.tied(integer)"),
            Function,
            Some("extension plpgsql"),
        ),
        (format!("{s}.t.tied"), Trigger, Some("extension plpgsql")),
    ];
    for (id, kind, expected) in &cases {
        let id: pbps_model::ModuleId = id.parse().expect("a module id");
        let rebuild = read_a_rebuild(&mut conn, &id, *kind).await;
        match expected {
            None => assert_eq!(
                rebuild.refusal(),
                None,
                "`{id}` carries nothing and must not refuse: {:?}",
                rebuild.carries
            ),
            Some(needle) => {
                let refusal = rebuild
                    .refusal()
                    .unwrap_or_else(|| panic!("`{id}` must refuse, and did not"));
                assert!(refusal.contains(needle), "`{id}`: {refusal}");
            }
        }
    }

    // And the guard that makes the case above the one it says it is: the
    // object's own ACL is empty, so a reader that only looked there would have
    // called this view safe to rebuild.
    assert_eq!(
        number(
            &mut conn,
            &format!(
                "SELECT COALESCE(pg_catalog.array_length(c.relacl, 1), 0)
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = '{s}' AND c.relname = 'column_granted'"
            )
        )
        .await,
        0,
        "this fixture must exercise `attacl`, and its `relacl` is not empty"
    );

    // The grant that arrives uninvited. Added after the objects exist, so the
    // *old* ACL of every one of them is still what it was — which is exactly
    // the gap a "reproduce the old ACL" check leaves.
    conn.execute(&format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT SELECT ON TABLES TO {reader}"
    ))
    .await
    .expect("default privileges");
    let plain: pbps_model::ModuleId = format!("{s}.plain").parse().expect("a module id");
    let arriving = read_a_rebuild(&mut conn, &plain, View).await;
    let refusal = arriving
        .refusal()
        .expect("a view that would arrive granted must refuse");
    assert!(refusal.contains("pg_default_acl"), "{refusal}");
    assert!(refusal.contains(&reader), "{refusal}");

    // The other side of the same obligation, and the one that makes it safe:
    // the question asked again of the object a `CREATE` just made. Nothing
    // locks `pg_default_acl`, so a plan-time read can only describe the world
    // before the statement — what has to be true is a fact about the
    // statement's own result. Measured here: a view created while the entry is
    // in force arrives already granted to a role no declaration mentions, and
    // the postcondition reports the ACL rather than the prediction.
    conn.execute(&format!("CREATE VIEW {s}.arrived AS SELECT id FROM {s}.t"))
        .await
        .expect("a view created while the default privilege is in force");
    let arrived: pbps_model::ModuleId = format!("{s}.arrived").parse().expect("a module id");
    let after = read_a_rebuild(&mut conn, &arrived, View).await;
    let landed = after
        .refusal()
        .expect("the object the CREATE made carries a grant nothing declared");
    assert!(
        landed.contains(&format!("{reader}=r/")),
        "the postcondition must report the ACL the object has, not the one \
         predicted: {landed}"
    );

    conn.execute(&format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE SELECT ON TABLES FROM {reader}"
    ))
    .await
    .expect("undo the default privileges");
    drop_schema(&mut conn, &s).await;
    conn.execute(&format!("DROP ROLE {reader}"))
        .await
        .expect("drop the role");
}

/// The read has to be serialized against a concurrent change, and **measured**,
/// the mechanism differs by kind and one of them is out of reach: a view takes
/// its own lock, a trigger takes its parent table's, and a routine's would be a
/// row lock on `pg_proc` that the accounts this tool is built for cannot take.
///
/// Refusing instead would refuse every function edit, since §3 makes them all
/// rebuilds — so the residual is named where the reviewer sees it. This suite
/// connects as a superuser, so the routine's lock *is* reachable here; what is
/// asserted is that each kind reports which of the four shapes applied, and
/// never that a rebuild was serialized when it was not.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn each_kind_says_what_serialized_its_read_or_that_nothing_did() {
    let s = emit_schema("serialize");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE TABLE {s}.t (id int primary key)"),
        format!("CREATE VIEW {s}.v AS SELECT id FROM {s}.t"),
        format!("CREATE FUNCTION {s}.f(n int) RETURNS int LANGUAGE sql AS $$ SELECT n $$"),
        format!(
            "CREATE FUNCTION {s}.trf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; \
             END $$"
        ),
        format!("CREATE TRIGGER a AFTER INSERT ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trf()"),
    ] {
        conn.execute(&sql).await.expect("build");
    }

    use pbps_model::ModuleKind::{Function, Trigger, View};
    for (id, kind, expected) in [
        (format!("{s}.v"), View, "the view's own"),
        (format!("{s}.t.a"), Trigger, "parent table"),
        (format!("{s}.f(integer)"), Function, "pg_proc"),
    ] {
        let id: pbps_model::ModuleId = id.parse().expect("a module id");
        let rebuild = read_a_rebuild(&mut conn, &id, kind).await;
        match &rebuild.serialized {
            pbps_pg::modules::Serialized::By(what) => {
                assert!(what.contains(expected), "{id}: {what}");
            }
            pbps_pg::modules::Serialized::Not(why) => {
                panic!("{id} was not serialized, and this account can: {why}")
            }
        }
    }

    // A module that is not there is refused rather than answered with
    // silence. "Nothing is attached to it" is what lets a rebuild go ahead,
    // and a read that matched no row says exactly that.
    let absent: pbps_model::ModuleId = format!("{s}.gone").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let missing = pbps_pg::modules::before_a_rebuild(&mut conn, &absent, View)
        .await
        .expect_err("a module that is not in the catalog");
    rollback(&mut conn).await;
    assert!(
        format!("{missing}").contains("not in this database's catalog"),
        "{missing}"
    );
    in_a_transaction(&mut conn).await;
    let missing_deps = pbps_pg::modules::dependents(&mut conn, &absent, View)
        .await
        .expect_err("the same question, from the other reader");
    rollback(&mut conn).await;
    assert!(
        format!("{missing_deps}").contains("not in this database's catalog"),
        "{missing_deps}"
    );

    // And outside a transaction the lock would be gone before the `DROP`, so
    // the read refuses rather than answering something that is true only while
    // it is being said.
    let id: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");
    let refused = pbps_pg::modules::before_a_rebuild(&mut conn, &id, View)
        .await
        .expect_err("a read with no transaction to hold the lock");
    assert!(
        format!("{refused}").contains("inside the transaction"),
        "{refused}"
    );
    drop_schema(&mut conn, &s).await;
}

/// The account this tool is actually built for cannot take a routine's lock,
/// and the attempt must not destroy the transaction it was protecting.
///
/// **Measured**, as a non-superuser owning its own function:
///
/// ```text
/// BEGIN; SELECT … FROM pg_proc … FOR UPDATE;
///     ERROR:  permission denied for table pg_proc
/// SELECT 1;
///     ERROR:  current transaction is aborted, commands ignored …
/// ```
///
/// Without a savepoint around the attempt, a rebuild that could perfectly well
/// have gone ahead unserialized fails instead — and it fails at the *first*
/// read after the lock, which is a message about `pg_proc` for an operator who
/// asked about a view's ACL. What this asserts is both halves: the answer says
/// the rebuild is not serialized, **and** the reads that follow it still work.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_account_that_cannot_lock_a_routine_says_so_and_the_reads_after_it_still_run() {
    let s = emit_schema("unprivileged");
    let deployer = format!("{s}_deploy");
    let mut admin = connect().await;
    fresh(&mut admin, &s).await;
    for sql in [
        format!("DROP ROLE IF EXISTS {deployer}"),
        format!("CREATE ROLE {deployer} LOGIN PASSWORD 'live-test'"),
        format!("GRANT CREATE, USAGE ON SCHEMA {s} TO {deployer}"),
    ] {
        admin
            .execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    // As the deploying account, which owns what it creates and nothing else.
    // The password goes with the user: a string that swapped the user and kept
    // the admin's password held only where the two were spelled alike, and CI
    // spells them differently.
    let as_deployer = settings_with(&[("user", deployer.as_str()), ("password", "live-test")]);
    let mut conn = Conn::connect(Driver::Postgres, &as_deployer)
        .await
        .expect("connect as the deploying account");
    conn.execute(&format!(
        "CREATE FUNCTION {s}.f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$"
    ))
    .await
    .expect("the deploying account owns its own function");

    let id: pbps_model::ModuleId = format!("{s}.f(integer)").parse().expect("a module id");
    // The read must survive an attempt that could not take the lock.
    let rebuild = read_a_rebuild(&mut conn, &id, pbps_model::ModuleKind::Function).await;

    match &rebuild.serialized {
        pbps_pg::modules::Serialized::Not(why) => {
            assert!(why.contains("pg_proc"), "{why}");
            assert!(why.contains("not serialized"), "{why}");
        }
        pbps_pg::modules::Serialized::By(what) => {
            panic!("this account should not be able to lock `pg_proc`: {what}")
        }
    }
    // And the reads after the failed attempt ran: this function is owned by
    // the account that would rebuild it and carries no ACL, so there is
    // nothing to refuse — which is only distinguishable from "the reads never
    // ran" because the reads did run.
    assert_eq!(rebuild.refusal(), None, "{:?}", rebuild.carries);

    drop(conn);
    admin
        .execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    admin
        .execute(&format!("DROP ROLE {deployer}"))
        .await
        .expect("drop the role");
}

/// ADR-0009 §4: on this engine the dependency refusal is the ordinary case, and
/// the enumeration is over **every** reverse `pg_depend` edge and not only
/// modules.
///
/// The four the ADR measured all hang off a function — a check constraint, a
/// column default, a generated column and an expression index — and since §3
/// makes every module change a rebuild, a function edit meets them every time.
/// Two of the four are **not representable**: `Column` has no
/// generated-expression field and `IndexColumn` is a name and a direction, so
/// promising to restore them would be promising to emit a statement pbps cannot
/// write. They take the unmanaged path with everything else the model cannot
/// hold.
///
/// The engine's own refusal is asserted beside the reader's, because the plan a
/// reviewer approves and the failure an operator would otherwise hit should
/// name the same objects.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn every_kind_of_dependent_blocks_the_rebuild_and_the_refusal_names_it() {
    let s = emit_schema("dependents");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!(
            "CREATE FUNCTION {s}.g(a int) RETURNS int IMMUTABLE LANGUAGE sql AS $$ SELECT a $$"
        ),
        format!(
            "CREATE TABLE {s}.t (id int primary key, n int CONSTRAINT ck CHECK ({s}.g(n) > 0), \
             d int DEFAULT {s}.g(1), gen int GENERATED ALWAYS AS ({s}.g(id)) STORED)"
        ),
        format!("CREATE INDEX ix ON {s}.t ({s}.g(n))"),
        format!(
            "CREATE FUNCTION {s}.atomic() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT {s}.g(1); END"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let g: pbps_model::ModuleId = format!("{s}.g(integer)").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let found = pbps_pg::modules::dependents(&mut conn, &g, pbps_model::ModuleKind::Function)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    let described: Vec<&str> = found.iter().map(|d| d.described.as_str()).collect();
    assert_eq!(
        described,
        vec![
            format!("constraint ck on table {s}.t"),
            format!("default value for column d of table {s}.t"),
            format!("default value for column gen of table {s}.t"),
            format!("function {s}.atomic()"),
            format!("index {s}.ix"),
        ],
        "the enumeration is over every reverse edge"
    );

    // With nothing declared, every one of them is a refusal — and it names
    // them all in one message, because an operator reading it is deciding what
    // to do about the function.
    let nothing = Schema::default();
    let refusal = pbps_pg::modules::unmanaged_refusal(&g, &found, &nothing)
        .expect("five undeclared dependents must refuse");
    for one in &described {
        assert!(refusal.contains(one), "{one} is missing from:\n{refusal}");
    }
    // The engine's own `HINT` for this refusal is `Use DROP ... CASCADE`, and
    // this tool's answer says the opposite in as many words: the plan names
    // every object it drops, or it does not drop (SPEC 14.3).
    assert!(
        refusal.contains("`DROP … CASCADE` is not offered"),
        "{refusal}"
    );

    // Declared, the two representable table parts become the plan's to drop
    // and restore around the rebuild. The generated column and the expression
    // index stay refused whatever the project declares — the model has nothing
    // to recreate them from.
    let mut declared = Schema::default();
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    t.columns.insert("n".into(), Column::new(ty("integer")));
    let mut d = Column::new(ty("integer"));
    d.default = Some(format!("{s}.g(1)"));
    t.columns.insert("d".into(), d);
    t.checks.insert(
        "ck".into(),
        pbps_model::CheckConstraint {
            expression: format!("{s}.g(n) > 0"),
        },
    );
    declared.tables.insert(TableName::new(&s, "t"), t);
    declared.modules.insert(
        format!("{s}.atomic()").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            &format!("() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT {s}.g(1); END"),
        ),
    );

    let still = pbps_pg::modules::unmanaged_refusal(&g, &found, &declared)
        .expect("two of the four are not representable at all");
    assert!(still.contains("column gen"), "{still}");
    assert!(still.contains("ix"), "{still}");
    assert!(!still.contains("constraint ck"), "{still}");
    assert!(!still.contains("atomic"), "{still}");

    let rebuilt: Vec<&str> = pbps_pg::modules::to_rebuild(&found, &declared)
        .iter()
        .map(|d| d.described.as_str())
        .collect();
    assert_eq!(
        rebuilt,
        vec![
            format!("constraint ck on table {s}.t"),
            format!("default value for column d of table {s}.t"),
            format!("function {s}.atomic()"),
        ],
        "these are what a plan has to drop before the rebuild and create after it"
    );

    // Outside a transaction the answer would be true when it was given and
    // stale by the `DROP`, and the canonical `search_path` it pins is set
    // `is_local` — which outside one does nothing at all, so the descriptions
    // would be worded by whatever path the operator's session held.
    let unheld = pbps_pg::modules::dependents(&mut conn, &g, pbps_model::ModuleKind::Function)
        .await
        .expect_err("a dependency read with no transaction to hold it");
    assert!(
        format!("{unheld}").contains("inside the transaction"),
        "{unheld}"
    );

    // And the engine names the same objects, which is the failure a plan
    // without this reader would be applyable straight into.
    let engine = conn
        .execute(&format!("DROP FUNCTION {s}.g(int)"))
        .await
        .expect_err("the engine refuses too");
    assert_eq!(sqlstate(&engine), "2BP01", "{engine:?}");

    // A dependent in a catalog with no arm of its own. Measured, a function
    // behind a cast has its reverse edge in `pg_cast`, and with six arms and
    // no fallback the edge was dropped entirely — so this reader called the
    // rebuild unblocked and the `DROP FUNCTION` it allowed failed at apply,
    // which is the one outcome SPEC §7.5 exists to prevent.
    for sql in [
        // The cast's target is a domain of this schema's own: a cast is
        // database-global, so one over two built-in types would collide with
        // every other run on the shared server.
        format!("CREATE DOMAIN {s}.label AS text"),
        format!(
            "CREATE FUNCTION {s}.tocast(a int) RETURNS {s}.label IMMUTABLE LANGUAGE sql AS $$ \
             SELECT a::text $$"
        ),
        format!("CREATE CAST (int AS {s}.label) WITH FUNCTION {s}.tocast(int) AS ASSIGNMENT"),
    ] {
        conn.execute(&sql).await.expect("the cast");
    }
    let tocast: pbps_model::ModuleId = format!("{s}.tocast(integer)").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let behind_a_cast =
        pbps_pg::modules::dependents(&mut conn, &tocast, pbps_model::ModuleKind::Function)
            .await
            .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        behind_a_cast
            .iter()
            .map(|d| d.described.as_str())
            .collect::<Vec<_>>(),
        vec![format!("cast from integer to {s}.label")],
        "an edge in a catalog with no arm of its own must still be reported"
    );
    let cast_refusal = pbps_pg::modules::unmanaged_refusal(&tocast, &behind_a_cast, &nothing)
        .expect("a dependent this project cannot recreate refuses");
    assert!(cast_refusal.contains("pg_cast"), "{cast_refusal}");
    let engine_agrees = conn
        .execute(&format!("DROP FUNCTION {s}.tocast(int)"))
        .await
        .expect_err("and the engine refuses the drop this reader would have allowed");
    assert_eq!(sqlstate(&engine_agrees), "2BP01", "{engine_agrees:?}");
    conn.execute(&format!("DROP CAST (int AS {s}.label)"))
        .await
        .expect("drop the cast");

    // A domain's check constraint is a `pg_constraint` row like a table's,
    // and it is not on a table: measured, `conrelid` is 0 and `contypid`
    // names the domain. The inner join to `pg_class` therefore threw the row
    // away, and a reader that reported no blocker emitted a `DROP FUNCTION`
    // the engine refuses — the case the fallback arm exists for, arriving
    // inside a class the list already knew.
    for sql in [
        format!(
            "CREATE FUNCTION {s}.ok(t text) RETURNS boolean IMMUTABLE LANGUAGE sql \
             AS $$ SELECT length(t) > 2 $$"
        ),
        format!("CREATE DOMAIN {s}.checked AS text CHECK ({s}.ok(VALUE))"),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let ok: pbps_model::ModuleId = format!("{s}.ok(text)").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let behind_a_domain =
        pbps_pg::modules::dependents(&mut conn, &ok, pbps_model::ModuleKind::Function)
            .await
            .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        behind_a_domain
            .iter()
            .map(|d| d.described.as_str())
            .collect::<Vec<_>>(),
        vec!["constraint checked_check"],
        "a constraint an arm's own join drops is as absent as one with no arm"
    );
    assert!(
        matches!(&behind_a_domain[0].holds, pbps_pg::modules::Holds::Unrepresentable(why)
                 if why.contains("domain")),
        "a domain constraint is something this project cannot put back, not nothing: {:?}",
        behind_a_domain[0].holds
    );
    let domain_refusal = pbps_pg::modules::unmanaged_refusal(&ok, &behind_a_domain, &nothing)
        .expect("a dependent this project cannot recreate refuses");
    assert!(domain_refusal.contains("checked_check"), "{domain_refusal}");
    let engine_agrees_again = conn
        .execute(&format!("DROP FUNCTION {s}.ok(text)"))
        .await
        .expect_err("and the engine refuses the drop this reader would have allowed");
    assert_eq!(
        sqlstate(&engine_agrees_again),
        "2BP01",
        "{engine_agrees_again:?}"
    );

    // A view's own edges are not its dependents, and this is the case that
    // shows it: `pg_depend` holds an *internal* edge from a view's `_RETURN`
    // rule and its row type to the view itself, so a reader that took every
    // reverse edge would report every view as depending on itself and refuse
    // to rebuild any of them.
    for sql in [
        format!("CREATE VIEW {s}.v AS SELECT id FROM {s}.t"),
        format!("CREATE VIEW {s}.v2 AS SELECT id FROM {s}.v"),
    ] {
        conn.execute(&sql).await.expect("the views");
    }
    conn.execute(&format!("CREATE VIEW {s}.v3 AS SELECT id FROM {s}.v2"))
        .await
        .expect("a third view, two levels away");
    let v: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let on_the_view = pbps_pg::modules::dependents(&mut conn, &v, pbps_model::ModuleKind::View)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        on_the_view
            .iter()
            .map(|d| d.described.as_str())
            .collect::<Vec<_>>(),
        vec![
            format!("rule _RETURN on view {s}.v3"),
            format!("rule _RETURN on view {s}.v2"),
        ],
        "a view's own `_RETURN` rule and row type are internal edges and not          dependents; `v3` is a dependent even though nothing joins it to `v`, and it comes          first because that is the order the drops go in"
    );
    assert_eq!(
        on_the_view[1].holds,
        pbps_pg::modules::Holds::Module(format!("{s}.v2").parse().expect("a module id")),
        "the dependent is the view that holds the rule, not the rule"
    );
    // And the reason the order is not a preference: measured, dropping the
    // direct dependent without the one beyond it is refused.
    let out_of_order = conn
        .execute(&format!("DROP VIEW {s}.v2"))
        .await
        .expect_err("the level below has to go first");
    assert_eq!(sqlstate(&out_of_order), "2BP01", "{out_of_order:?}");

    // One dependent, three edges. Measured, `pg_depend` holds a row per
    // *column* a dependent uses — a routine reading three columns of a view
    // has three edges to it — so a query that unnests `proargtypes` once per
    // edge rebuilds a one-argument routine as `f(integer,integer,integer)`.
    // That identity no declaration holds, so an otherwise manageable rebuild
    // is refused and the walk cannot resolve the object it has just named.
    for sql in [
        format!("CREATE VIEW {s}.wide AS SELECT id, n, d FROM {s}.t"),
        format!(
            "CREATE FUNCTION {s}.reads(a int) RETURNS int LANGUAGE sql \
             BEGIN ATOMIC SELECT a + x.id + x.n + x.d FROM {s}.wide x; END"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        number(
            &mut conn,
            &format!(
                "SELECT pg_catalog.count(*)::int FROM pg_catalog.pg_depend d
                  WHERE d.refclassid = 'pg_catalog.pg_class'::regclass
                    AND d.refobjid = '{s}.wide'::regclass
                    AND d.classid = 'pg_catalog.pg_proc'::regclass
                    AND d.deptype <> 'i'"
            )
        )
        .await,
        3,
        "the fixture has to have more than one edge, or it is testing nothing"
    );
    let wide: pbps_model::ModuleId = format!("{s}.wide").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let many_edges = pbps_pg::modules::dependents(&mut conn, &wide, pbps_model::ModuleKind::View)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        many_edges
            .iter()
            .map(|d| d.holds.clone())
            .collect::<Vec<_>>(),
        vec![pbps_pg::modules::Holds::Module(
            format!("{s}.reads(integer)").parse().expect("a module id")
        )],
        "one routine, one identity, however many columns of the view it reads"
    );
    for sql in [
        format!("DROP FUNCTION {s}.reads(int)"),
        format!("DROP VIEW {s}.wide"),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    // A depth is not an order. `d2` and `d3` are both direct dependents of
    // `d1`, and `d3` is a dependent of `d2` as well — so one query returns
    // them together, and a walk that only recorded how deep each was emitted
    // them in the order the catalog gave: `d2` first, which is the one that
    // cannot go first.
    for sql in [
        format!("CREATE VIEW {s}.d1 AS SELECT id FROM {s}.t"),
        format!("CREATE VIEW {s}.d2 AS SELECT id FROM {s}.d1"),
        format!("CREATE VIEW {s}.d3 AS SELECT x.id FROM {s}.d1 x JOIN {s}.d2 y ON y.id = x.id"),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let d1: pbps_model::ModuleId = format!("{s}.d1").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let diamond = pbps_pg::modules::dependents(&mut conn, &d1, pbps_model::ModuleKind::View)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        diamond
            .iter()
            .map(|d| d.described.as_str())
            .collect::<Vec<_>>(),
        vec![
            format!("rule _RETURN on view {s}.d3"),
            format!("rule _RETURN on view {s}.d2"),
        ],
        "`d3` depends on `d2`, so it goes first whatever their depths from `d1` are"
    );
    // And the order is not a preference: measured, the other one fails.
    let wrong_way = conn
        .execute(&format!("DROP VIEW {s}.d2"))
        .await
        .expect_err("`d3` depends on `d2`");
    assert_eq!(sqlstate(&wrong_way), "2BP01", "{wrong_way:?}");
    for view in ["d3", "d2", "d1"] {
        conn.execute(&format!("DROP VIEW {s}.{view}"))
            .await
            .unwrap_or_else(|e| panic!("{view}: {e}"));
    }

    // A cycle, which `CREATE OR REPLACE` can close between two `BEGIN ATOMIC`
    // routines: measured, `pg_depend` then holds both directions and neither
    // routine can be dropped first. There is no order, so the answer is to say
    // so rather than to emit one that fails.
    for sql in [
        format!("CREATE FUNCTION {s}.cg() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END"),
        format!(
            "CREATE FUNCTION {s}.cf() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT {s}.cg(); END"
        ),
        format!(
            "CREATE OR REPLACE FUNCTION {s}.cg() RETURNS int LANGUAGE sql \
             BEGIN ATOMIC SELECT {s}.cf(); END"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let cf: pbps_model::ModuleId = format!("{s}.cf()").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let looped = pbps_pg::modules::dependents(&mut conn, &cf, pbps_model::ModuleKind::Function)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    assert_eq!(
        looped
            .iter()
            .map(|d| d.described.as_str())
            .collect::<Vec<_>>(),
        vec![format!("function {s}.cg()")],
        "the module is not one of its own dependents, however the walk reached it"
    );
    assert!(
        matches!(&looped[0].holds, pbps_pg::modules::Holds::Unrepresentable(why)
                 if why.contains("cycle")),
        "a cycle is something a plan cannot order, not something it can drop: {:?}",
        looped[0].holds
    );
    let cycle_refusal = pbps_pg::modules::unmanaged_refusal(&cf, &looped, &nothing)
        .expect("a cycle refuses whatever the declarations hold");
    assert!(cycle_refusal.contains("cycle"), "{cycle_refusal}");
    // And the engine agrees that there is no order: neither one goes first.
    for routine in ["cf", "cg"] {
        let refused = conn
            .execute(&format!("DROP FUNCTION {s}.{routine}()"))
            .await
            .expect_err("each is held up by the other");
        assert_eq!(sqlstate(&refused), "2BP01", "{routine}: {refused:?}");
    }

    drop_schema(&mut conn, &s).await;
}

/// The enumeration ADR-0009 §3 obliges, checked against the engine rather than
/// against the last time somebody thought about it.
///
/// A `DROP` takes with it every row the catalog keys by the object's *address*
/// — `(classoid, objoid)` — and that list was written from memory twice and
/// was short both times: a comment first, then a security label. So it is
/// asked of the server, and the reader's list has to be the same list.
///
/// A sixth catalog arriving in a later release fails here, which is the whole
/// point: this test is the reason the list can be trusted.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn every_catalog_keyed_by_an_object_is_read() {
    let mut conn = connect().await;
    let engines: Vec<String> = conn
        .query(
            "SELECT c.relname AS name FROM pg_catalog.pg_class c
              WHERE c.relnamespace = 'pg_catalog'::regnamespace AND c.relkind = 'r'
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                             WHERE a.attrelid = c.oid AND a.attname = 'classoid'
                               AND a.attnum > 0)
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                             WHERE a.attrelid = c.oid AND a.attname = 'objoid'
                               AND a.attnum > 0)
              ORDER BY 1",
        )
        .await
        .expect("ask the engine which catalogs key a row by an object")
        .iter()
        .map(|row| {
            row.try_get::<&str>("name")
                .expect("a name")
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let mut ours = pbps_pg::modules::catalogs_read_by_address();
    ours.sort_unstable();
    assert_eq!(
        engines, ours,
        "the rebuild reads every catalog keyed by an object's address, or it is not an \
         enumeration"
    );
    assert!(
        !engines.is_empty(),
        "a query that found nothing would agree with an empty reader"
    );
}

/// The probe that decides whether there is a transaction has to be asking the
/// engine, not reading back something the session was already holding.
///
/// Both sides of it are a `set_config(…, is_local => true)` in one statement
/// and a `current_setting` in the next: inside a transaction the setting
/// survives to be read, outside one the implicit transaction ends and it does
/// not. Compared against a constant, a session that already carries
/// `SET pbps.in_a_transaction = 'yes'` answers `'yes'` on an autocommit
/// connection — and then the rebuild's reads believe they are serialized when
/// `LOCK TABLE` has already been released, and the pull refuses a connection
/// that has no transaction at all. One is a guard that no longer guards, the
/// other is a valid plan refused.
///
/// The token is invented per call, so the only way the read can equal it is if
/// this call's own `set_config` survived — which is the question.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_session_setting_left_behind_cannot_answer_the_transaction_probe() {
    let s = emit_schema("probe");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!("CREATE TABLE {s}.t (id int)"))
        .await
        .expect("a table");
    conn.execute(&format!("CREATE VIEW {s}.v AS SELECT id FROM {s}.t"))
        .await
        .expect("a view");
    let v: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");

    // The exact spoof: the value the probe used to compare against, left in
    // the session where a `SET LOCAL` cannot clear it.
    conn.execute("SET pbps.in_a_transaction = 'yes'")
        .await
        .expect("a session may set its own GUCs");
    assert_eq!(
        text(
            &mut conn,
            "SELECT current_setting('pbps.in_a_transaction', true)"
        )
        .await,
        "yes",
        "the spoof has to be in place for this test to be testing anything"
    );

    // No transaction is open, so both of these must still refuse.
    for what in ["rebuild", "dependents"] {
        let refused = match what {
            "rebuild" => {
                pbps_pg::modules::before_a_rebuild(&mut conn, &v, pbps_model::ModuleKind::View)
                    .await
                    .err()
            }
            _ => pbps_pg::modules::dependents(&mut conn, &v, pbps_model::ModuleKind::View)
                .await
                .err(),
        };
        let refused = refused.unwrap_or_else(|| {
            panic!(
                "`{what}` accepted an autocommit connection as a \
                                       transaction because the session said so"
            )
        });
        assert!(
            refused
                .to_string()
                .contains("has to run inside the transaction"),
            "{refused:?}"
        );
    }

    // And the same setting must not make the pull refuse a connection that is
    // doing nothing wrong: the pull asks the opposite question of the same
    // probe, so a constant defeats it in the opposite direction.
    // Through the suite's retry helper, because the rest of this file is
    // applying DDL while this runs — what is being asserted is that the pull
    // is *not refused*, not that nothing else is happening.
    let pulled = pull(&mut conn).await;
    assert!(
        pulled.schema.modules.contains_key(&v),
        "the pull ran and read this schema"
    );

    // Inside a real transaction it still says yes, which is the half a broken
    // probe would also get right and this pins anyway.
    in_a_transaction(&mut conn).await;
    let rebuild = pbps_pg::modules::before_a_rebuild(&mut conn, &v, pbps_model::ModuleKind::View)
        .await
        .expect("a real transaction is a transaction");
    rollback(&mut conn).await;
    assert_eq!(rebuild.refusal(), None, "{:?}", rebuild.carries);

    drop_schema(&mut conn, &s).await;
}

/// The limit of "enumerate from the catalog", measured: `pg_depend` records an
/// edge for a caller only when the calling body was **parsed at creation
/// time**.
///
/// A `BEGIN ATOMIC` body records one; a plpgsql body and a SQL string-literal
/// body record nothing. So a rebuild that changes a signature goes through, the
/// apply commits, `verify` has nothing to report — and the caller fails the
/// next time anybody calls it.
///
/// What is done about it is a **report**, never a refusal: the scan matches a
/// name, and with overloading a name is not an identity, so a managed caller of
/// `f(text)` would block every rebuild of `f(integer)` with no way to clear the
/// block. The refusals in this design are for facts, and a name is not one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_caller_only_a_name_scan_can_see_is_reported_and_the_engine_never_saw_it() {
    let s = emit_schema("scan");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE FUNCTION {s}.dep_f(a int) RETURNS int LANGUAGE sql AS $$ SELECT a $$"),
        format!(
            "CREATE FUNCTION {s}.plpgsql_caller() RETURNS int LANGUAGE plpgsql AS $$ BEGIN \
             RETURN {s}.dep_f(1); END $$"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let dep_f: pbps_model::ModuleId = format!("{s}.dep_f(integer)").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let edges = pbps_pg::modules::dependents(&mut conn, &dep_f, pbps_model::ModuleKind::Function)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    assert!(
        edges.is_empty(),
        "the catalog records nothing for a plpgsql caller: {edges:?}"
    );

    // The scan does see it, and says so without blocking anything.
    let mut declared = Schema::default();
    let caller: pbps_model::ModuleId = format!("{s}.plpgsql_caller()")
        .parse()
        .expect("a module id");
    declared.modules.insert(
        caller.clone(),
        module(
            pbps_model::ModuleKind::Function,
            &format!("() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN {s}.dep_f(1); END $$"),
        ),
    );
    declared
        .modules
        .insert(dep_f.clone(), module(pbps_model::ModuleKind::Function, "x"));
    let by_name = pbps_pg::modules::callers_by_name(&declared, &dep_f);
    assert_eq!(by_name, vec![caller.clone()]);
    let report = pbps_pg::modules::callers_report(&dep_f, &by_name).expect("a report");
    assert!(report.contains(&caller.to_string()), "{report}");
    assert!(
        !report.to_lowercase().contains("cannot be rebuilt"),
        "a name match is reported, not refused: {report}"
    );

    // And the hazard the report exists for, measured end to end: the rebuild
    // goes through and the caller is broken afterwards, with nothing in
    // between having said so.
    conn.execute(&format!("DROP FUNCTION {s}.dep_f(int)"))
        .await
        .expect("the rebuild's drop is accepted");
    conn.execute(&format!(
        "CREATE FUNCTION {s}.dep_f(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + b $$"
    ))
    .await
    .expect("the rebuild's create is accepted");
    let broken = match conn.query(&format!("SELECT {s}.plpgsql_caller()")).await {
        Ok(_) => panic!("the caller answered, and this test needs it broken"),
        Err(e) => e,
    };
    assert_eq!(sqlstate(&broken), "42883", "{broken:?}");

    drop_schema(&mut conn, &s).await;
}

/// ADR-0013 §3, and the issue's last named check: **a same-named object
/// introduced earlier on the path by the same plan must rebuild the module
/// once, rather than one plan late.**
///
/// The three states are measured here in order, and the middle one is the
/// hazard:
///
/// ```text
/// before anything arrives:                      caller() = 'shared'
/// after the shadow arrives, with no rebuild:    caller() = 'shared'
/// after the rebuild:                            caller() = 'app'
/// ```
///
/// The middle line is what "one plan late" costs: the environment goes on
/// meaning `shared.f` while the declarations now mean `app.f`, and nothing in
/// the plan that created `app.f` said so. The differ cannot find it — the
/// caller's declaration did not change — so the comparison is asked of the
/// catalog **as this plan will leave it**, and the answer rebuilds the caller
/// in the same plan.
///
/// The test is a name and a path, not a position, and deliberately so: a
/// declaration that qualified the name in full is rebuilt too, once. What an
/// unchanged declaration *would* bind to today cannot be computed without
/// parsing it, and §8.2 says this tool does not.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_shadow_this_plan_introduces_rebuilds_the_module_in_the_same_plan() {
    let s = emit_schema("binding");
    let shared = format!("{s}_shared");
    let pg = Postgres::with_write_path_extras(vec![shared.clone()]);
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &shared).await;
    conn.execute(&format!(
        "CREATE FUNCTION {shared}.f(a int) RETURNS text LANGUAGE sql AS $$ SELECT 'shared' $$"
    ))
    .await
    .expect("the shared function");

    let caller: pbps_model::ModuleId = format!("{s}.caller()").parse().expect("a module id");
    // `BEGIN ATOMIC`, because that is the body whose binding is fixed when it
    // is created — the one kind ADR-0009 §4 measured as recording its
    // dependency, and the kind this project's documentation recommends for
    // exactly that reason.
    let caller_body = "() RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT f(1); END";
    let mut a = Schema::default();
    a.modules.insert(
        caller.clone(),
        module(pbps_model::ModuleKind::Function, caller_body),
    );
    let ids = mint_ids(&a, &IdsFile::default(), &[]);
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids),
    )
    .await;
    assert_eq!(
        text(&mut conn, &format!("SELECT {s}.caller()")).await,
        "shared",
        "precondition: the caller binds through the path to the shared schema"
    );

    // B adds an overload earlier on the path, and changes nothing else.
    let shadow: pbps_model::ModuleId = format!("{s}.f(integer)").parse().expect("a module id");
    let mut b = a.clone();
    b.modules.insert(
        shadow.clone(),
        module(
            pbps_model::ModuleKind::Function,
            "(a integer) RETURNS text LANGUAGE sql AS $$ SELECT 'app' $$",
        ),
    );
    let cs = plan(&a, &ids, &b, &ids);
    // The differ finds one change, because the caller's declaration did not
    // move — which is the whole difficulty: nothing in this plan mentions the
    // module whose meaning it is about to alter.
    assert_eq!(cs.changes.len(), 1, "{cs:#?}");
    let pbps_model::Change::CreateModule { id: created, .. } = &cs.changes[0].change else {
        panic!(
            "the one change is the overload's creation: {:#?}",
            cs.changes[0]
        )
    };
    assert_eq!(created, &shadow);
    let changed: std::collections::BTreeSet<pbps_model::ModuleId> =
        [shadow.clone()].into_iter().collect();

    let rebound = pbps_pg::modules::rebound_by_this_plan(
        &b,
        pg.write_path_extras(),
        std::slice::from_ref(&shadow),
        &changed,
    );
    assert_eq!(
        rebound,
        vec![pbps_pg::modules::Rebound {
            module: caller.clone(),
            arriving: shadow.clone(),
        }],
        "the caller has to be rebuilt by this plan"
    );

    // The plan as it must be applied: the change the differ found, and the
    // rebuild this answer synthesized.
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT {s}.caller()")).await,
        "shared",
        "without the rebuild the environment still means the old binding — this is the line \
         that costs a whole plan cycle"
    );
    for stmt in pg
        .emit(
            &pbps_model::Change::AlterModule {
                id: caller.clone(),
                module: Box::new(module(pbps_model::ModuleKind::Function, caller_body)),
            },
            pbps_model::Strategy::default(),
        )
        .expect("emit the synthesized rebuild")
    {
        conn.execute(&stmt.sql)
            .await
            .unwrap_or_else(|e| panic!("{}\n{e}", stmt.sql));
    }
    assert_eq!(
        text(&mut conn, &format!("SELECT {s}.caller()")).await,
        "app",
        "the binding moved in the same plan"
    );

    // Once. The next plan introduces nothing, so nothing is rebound.
    assert!(
        pbps_pg::modules::rebound_by_this_plan(
            &b,
            pg.write_path_extras(),
            &[],
            &std::collections::BTreeSet::new()
        )
        .is_empty(),
        "a plan that introduces nothing rebuilds nothing"
    );

    for schema in [&s, &shared] {
        drop_schema(&mut conn, schema).await;
    }
}

/// Absent, empty and unreadable are three different things, on the module side
/// of the pull.
///
/// A materialized view, an aggregate and a window function are module-shaped
/// objects the model does not hold. They are **named**, not silently missing —
/// a module read as absent is a plan that creates it on top of the one that is
/// already there — and they are not read back as the ordinary kind, which would
/// make a plan that recreates a materialized view as a view.
///
/// The `prokind` filter that keeps them out is not tidiness: **measured**,
/// `pg_get_functiondef` refuses an aggregate by name, so without it the whole
/// pull fails rather than reporting one object.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_module_shaped_object_the_model_does_not_hold_is_named_and_the_pull_still_runs() {
    let s = emit_schema("unheld");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE TABLE {s}.t (id int primary key)"),
        format!("CREATE MATERIALIZED VIEW {s}.mv AS SELECT id FROM {s}.t"),
        format!("CREATE AGGREGATE {s}.agg(int) (sfunc = int4pl, stype = int)"),
        // The engine's own answer, and the reason the filter exists.
        format!("CREATE VIEW {s}.ordinary AS SELECT id FROM {s}.t"),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let refused = conn
        .query(&format!(
            "SELECT pg_catalog.pg_get_functiondef(p.oid) FROM pg_catalog.pg_proc p
               JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = '{s}' AND p.prokind = 'a'"
        ))
        .await
        .err()
        .expect("an aggregate has no function definition");
    assert_eq!(sqlstate(&refused), "42809", "{refused:?}");

    let pulled = pull(&mut conn).await;
    let ours = our_modules(&pulled, &s);
    let named: Vec<&String> = pulled
        .warnings
        .iter()
        .filter(|w| w.contains(&format!("{s}.")))
        .collect();
    drop_schema(&mut conn, &s).await;

    assert_eq!(
        ours.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec![format!("{s}.ordinary")],
        "only the kind the model holds is in the pull"
    );
    for (object, why) in [("mv", "materialized view"), ("agg", "aggregate")] {
        assert!(
            named
                .iter()
                .any(|w| w.contains(&format!("{s}.{object}")) && w.contains(why)),
            "`{s}.{object}` was not named: {named:#?}"
        );
    }
}

/// A module the catalog scan saw and the deparse could not: the third of
/// "absent, empty and unreadable", and the one the pull used to report as its
/// own bug.
///
/// The pull takes one `REPEATABLE READ` snapshot so that it cannot report half
/// of a change as a whole schema. A deparser does not read from it — it
/// resolves its oid through the syscache against a fresh snapshot — so an
/// object dropped between the scan and the deparse comes back as a row with a
/// name and no definition. Reported as an unexpected row shape it read as "the
/// query and this code have gone out of step", which is the one diagnosis that
/// is certainly wrong: nothing is out of step, the catalog moved.
///
/// Two halves. The engine's, which is what the rule rests on and is exact; and
/// the reader's, driven by dropping views out from under a pull until it
/// happens.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_module_dropped_between_the_scan_and_the_deparse_says_the_catalog_moved() {
    let mut conn = connect().await;
    // The deparsers answer `NULL` for an oid that is not there rather than
    // raising, which is why this arrives as a row and not as `XX000`. If a
    // later engine raises instead, this fails and the reader's rule is the
    // thing to revisit.
    for probe in [
        "pg_catalog.pg_get_viewdef(999999::oid, true)",
        "pg_catalog.pg_get_functiondef(999999::oid)",
    ] {
        assert!(
            truth(&mut conn, &format!("SELECT {probe} IS NULL")).await,
            "{probe} must answer NULL for an oid that is gone"
        );
    }

    // One schema of routines, dropped one at a time. Not whole schemas: a
    // `DROP SCHEMA … CASCADE` storm deadlocks against the rest of this suite
    // doing the same thing, and a test that destabilises its neighbours is not
    // paying for itself. Routines rather than views because
    // `pg_get_functiondef` is the deparser that answers `NULL` for an oid that
    // is gone — `pg_get_viewdef` more often catches the drop mid-flight and
    // raises `XX000`, which is the path that was already right.
    const ROUTINES: usize = 40;
    let s = emit_schema("vanishing");
    fresh(&mut conn, &s).await;
    // Long bodies rather than many routines: what has to be long is the
    // *deparse*, so that a drop has somewhere to land inside it, and forty
    // statements do not slow the rest of the suite down the way four hundred
    // do.
    let padding = "-- ".to_owned() + &"pad ".repeat(500);
    for n in 0..ROUTINES {
        conn.execute(&format!(
            "CREATE FUNCTION {s}.f{n}(a int) RETURNS int LANGUAGE sql AS $$\n{padding}\n\
             SELECT a $$"
        ))
        .await
        .expect("a routine");
    }

    let churn_schema = s.clone();
    let churn = tokio::spawn(async move {
        let mut churner = connect().await;
        for n in 0..ROUTINES {
            churner
                .execute(&format!("DROP FUNCTION {churn_schema}.f{n}(int)"))
                .await
                .expect("drop one routine");
        }
    });

    let mut caught = 0usize;
    let mut pulls = 0usize;
    while !churn.is_finished() {
        pulls += 1;
        if let Err(e) = pbps_pg::catalog::introspect(&mut conn).await {
            let message = e.to_string();
            assert!(
                !message.contains("gone out of step"),
                "a vanished module is not a reader out of step with its query: {message}"
            );
            assert!(
                message.contains("the catalog changed while it was being read"),
                "the pull failed for a reason this test does not cause: {message}"
            );
            caught += 1;
        }
    }
    churn.await.expect("the churner finished");
    drop_schema(&mut conn, &s).await;

    // Asserted to have fired, because a test that can pass without reaching
    // the case is one that will go on passing when the case breaks. The
    // fixture is sized for it: every pull scans all of these schemas, and the
    // churn drops them one at a time, so a pull that overlaps none of the
    // drops means the churn outran the reader and the counts say so.
    assert!(
        caught > 0,
        "{pulls} pulls ran and none of them overlapped a drop; the fixture has stopped \
         reaching the case it exists for"
    );
}

/// The routine half of "the identity and the body both carry it, and the
/// engine accepts the disagreement without a word".
///
/// The trigger's `ON` clause has had this test since the kind was added; the
/// parameter list had none, and it is the same fact: the emitter writes
/// `CREATE FUNCTION <name>` and the declaration writes the list, so a key
/// saying `f(integer)` over a body saying `(x text)` creates `f(text)` — an
/// object the key does not name, that every later plan creates again and never
/// finds.
///
/// Measured both ways round: every spelling the gate accepts creates exactly
/// the identity its key names, and the shape the gate refuses is one the
/// engine really does accept under the wrong name.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_identity_a_routine_body_creates_is_the_key_the_gate_accepts_it_under() {
    let s = emit_schema("routine_identity");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!("CREATE TYPE {s}.my_type AS ENUM ('a', 'b')"))
        .await
        .expect("a user type to qualify");
    conn.execute(&format!("CREATE TYPE {s}.\"x\u{a0}\" AS ENUM ('a')"))
        .await
        .expect("a user type whose name ends in a non-breaking space");
    let pg = Postgres::new();

    // Every one of these is a spelling a declaration may reasonably carry, and
    // the assertion is not that the gate likes it: it is that the object the
    // engine creates is the one the key names.
    for (at, (args, definition)) in [
        (
            "integer",
            "(x integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(integer) RETURNS int LANGUAGE sql AS $$ SELECT $1 $$",
        ),
        (
            "integer",
            "(a int DEFAULT 3) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        // The modifier is discarded from every routine argument, catalogued or
        // not, so the declared spelling and the identity differ on purpose.
        (
            "numeric",
            "(a numeric(10, 2)) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "character varying",
            "(a character varying(5)) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "double precision",
            "(a double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "double precision",
            "(double precision) RETURNS int LANGUAGE sql AS $$ SELECT $1 $$",
        ),
        // `OUT` is the one mode that keeps a parameter out of `proargtypes`,
        // and it may be written on either side of the name.
        ("", "(out x text) LANGUAGE sql AS $$ SELECT 'q' $$"),
        ("", "(a out integer) LANGUAGE sql AS $$ SELECT 1 $$"),
        // And a comment is whitespace inside the list, so it hides neither the
        // mode nor the type.
        (
            "",
            "(value /* note */ out integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "",
            "(out /* note */ value integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "",
            "(value out /* note */ integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "",
            "(value\n-- line comment\nout integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(in /* note */ x /* note */ int) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "",
            "(/* note */ out value integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(-- which one\n a integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(a integer /* trailing */) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(a integer, out b text) LANGUAGE sql AS $$ SELECT 'q' $$",
        ),
        (
            "integer",
            "(a inout integer) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "text[]",
            "(variadic a text[]) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer",
            "(\"a,b\" integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "integer,text",
            "(a integer, b text DEFAULT ',)') RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        ("", "() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
        (
            "integer",
            "/* the id */ (a integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        // A bare user type in the body and the qualified spelling in the key:
        // `format_type` under the empty read path always qualifies one
        // (ADR-0013 §3), and the write path is what makes the bare name find
        // it. The gate cannot know that it does, so it does not refuse — and
        // this measures that the two really are one object.
        (
            "%S%.my_type",
            "(a my_type) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "%S%.my_type",
            "(a %S%.my_type) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        // A Unicode-escaped identifier, in the key and in the body: measured,
        // `r12.U&"\006doney"` is identified as `r12.money`.
        (
            "%S%.U&\"\\006dy_type\"",
            "(a %S%.U&\"\\006dy_type\") RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "%S%.my_type",
            "(a %S%.u&\"!006dy_type\" UESCAPE '!') RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        // A non-breaking space is the last byte of the type's name, before
        // the comma and before a default alike — measured, the identity keeps
        // it and quotes the name.
        (
            "%S%.x\u{a0}, integer",
            "(a %S%.x\u{a0}, b integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        (
            "%S%.x\u{a0}",
            "(a %S%.x\u{a0} DEFAULT NULL) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
        // `$` continues a name — measured, `foo$tag$` is one parameter name,
        // and a scan that read its `$tag$` as a literal's opener refused the
        // routine the engine creates under this very key.
        (
            "integer",
            "(foo$tag$ integer) RETURNS int LANGUAGE sql AS $$ SELECT foo$tag$ $$",
        ),
        (
            "integer, text",
            "(a$ integer, b$c$ text DEFAULT $x$a$x$) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("f{at}");
        // The key is written the way a declaration writes one, with the user
        // type qualified because that is what `format_type` reads back under
        // the empty path (ADR-0013 §3).
        let args = args.replace("%S%", &s);
        let definition = definition.replace("%S%", &s);
        let definition = definition.as_str();
        let key = if args.is_empty() {
            format!("{s}.{name}()")
        } else {
            format!("{s}.{name}({args})")
        };
        let id: pbps_model::ModuleId = key.parse().unwrap_or_else(|e| panic!("{key}: {e}"));
        let module = module(pbps_model::ModuleKind::Function, definition);
        let refusals = pg.validate_module(&id, &module);
        assert!(
            refusals.is_empty(),
            "the gate refused `{key}` over `{definition}`: {:?}",
            refusals.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        for statement in pg
            .emit(
                &pbps_model::Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                },
                Strategy::default(),
            )
            .expect("emit the create")
        {
            conn.execute(&statement.sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", statement.sql));
        }
        let written = text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                  WHERE p.pronamespace = '{s}'::regnamespace AND p.proname = '{name}'"
            ),
        )
        .await;
        // Compared through the engine's own resolution of the key, not as
        // text: a name the engine quotes when it writes it back — one ending
        // in a non-breaking space — is spelled bare in a declaration.
        let resolved = text(&mut conn, &format!("SELECT '{key}'::regprocedure::text")).await;
        assert_eq!(
            written, resolved,
            "`{definition}` under the key `{key}` created another object"
        );
    }

    // A user type whose name is the second word of a built-in one. Split
    // again, `(double precision)` reads as a parameter named `double` of type
    // `precision`, and the qualification rule would then let `{s}.precision`
    // through — while the engine creates `f(double precision)`. Measured here
    // rather than argued.
    conn.execute(&format!("CREATE DOMAIN {s}.precision AS int"))
        .await
        .expect("a user type named like the second word of a built-in one");
    let ambiguous: pbps_model::ModuleId = format!("{s}.split({s}.precision)")
        .parse()
        .expect("a module id");
    assert!(
        !pg.validate_module(
            &ambiguous,
            &module(
                pbps_model::ModuleKind::Function,
                "(double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
        )
        .is_empty(),
        "a body the engine reads as `double precision` must not pass under a key that says \
         `{s}.precision`"
    );
    conn.execute(&format!(
        "CREATE FUNCTION {s}.split(double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
    ))
    .await
    .expect("and the engine takes it, under the other identity");
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                  WHERE p.pronamespace = '{s}'::regnamespace AND p.proname = 'split'"
            )
        )
        .await,
        format!("{s}.split(double precision)"),
        "the engine does not offer the `name type` reading for a built-in it knows"
    );

    // And the other direction, because a gate that never refuses is not one.
    // The engine takes this statement happily; what it creates is `g(text)`,
    // under a declaration that says `g(integer)`.
    let wrong: pbps_model::ModuleId = format!("{s}.g(integer)").parse().expect("a module id");
    let body = module(
        pbps_model::ModuleKind::Function,
        "(x text) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
    );
    assert!(
        !pg.validate_module(&wrong, &body).is_empty(),
        "a body that creates another identity has to be refused offline"
    );
    conn.execute(&format!(
        "CREATE FUNCTION {s}.g(x text) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
    ))
    .await
    .expect("the engine accepts it without a word, which is the whole problem");
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                  WHERE p.pronamespace = '{s}'::regnamespace AND p.proname = 'g'"
            )
        )
        .await,
        format!("{s}.g(text)"),
        "the identity comes from the parameter list, not from the key"
    );

    drop_schema(&mut conn, &s).await;
}

/// The one assertion that ties ADR-0009 §1's fold to the engine: a routine
/// declared in *this* spelling reads back under *that* key, and the dialect
/// turns the first into the second without a connection.
///
/// This is the test the `bit varying(4)` finding needed. A modifier the closed
/// column catalogue does not carry was left on the declared identity while the
/// catalog reported it gone, so the routine was one to create and one to drop
/// on every connected plan for ever — the cry-wolf loop ADR-0002 names as the
/// failure to avoid, and a different and worse thing from one loud mismatch.
///
/// Measured here, end to end, over every shape the fold has a rule for: a
/// modifier on a catalogued type, on an uncatalogued one and on a user type; a
/// type whose modifier decides which type it is; a field qualifier written in
/// words rather than parentheses; an array; and a quoted name whose own
/// parentheses are part of it.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_declared_argument_and_the_identity_the_engine_writes_are_one_key() {
    let s = emit_schema("identity");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!("CREATE TYPE {s}.\"odd(name)\" AS ENUM ('a')"))
        .await
        .expect("a type whose name has parentheses in it");
    conn.execute(&format!("CREATE DOMAIN {s}.money_amount AS numeric(10, 2)"))
        .await
        .expect("a domain");
    for keyword in ["select", "zone"] {
        conn.execute(&format!("CREATE TYPE {s}.\"{keyword}\" AS ENUM ('a')"))
            .await
            .expect("a type named by a keyword");
    }
    conn.execute(&format!("CREATE TYPE {s}.money$type AS ENUM ('a')"))
        .await
        .expect("a type with a dollar in its name");

    // Left: what a declaration may reasonably say. Right: nothing — the key is
    // asserted against the catalog, not against a second copy of this list.
    let declared = [
        "varchar(10)",
        "numeric(10,2)",
        "int4",
        "timestamp(3) with time zone",
        "float(24)",
        "bit varying(4)",
        "bit(3)",
        "interval hour to minute",
        "text[][]",
        // A built-in written with its schema is the built-in.
        "pg_catalog.int4",
        "pg_catalog.varbit(4)",
        "pg_catalog.timestamptz(3)",
        // The catalog's own name for a built-in's array type.
        "_int4",
        "pg_catalog._int4",
        "_varbit",
        "_numeric(10,2)",
        // Aliases the engine identifies as something else and the column
        // catalogue does not carry.
        "varbit(4)",
        "bpchar(3)",
        "nchar(2)",
        "national character",
        "national char",
        "char varying(5)",
        "nchar varying(5)",
        "national character varying(5)",
        "national char varying(5)",
        // The standard's spelling of an array, with and without a dimension,
        // and with the space the grammar allows before the bracket.
        "text ARRAY",
        "text ARRAY[4]",
        "int ARRAY [2]",
        "character varying array",
        format!("{s}.\"odd(name)\"").as_str(),
        format!("{s}.money_amount").as_str(),
        // The engine accepts a space around a qualified type's dot and never
        // writes one back. Unfolded, the declared key and the catalog key are
        // two keys for one routine, and every plan drops and creates it again.
        format!("{s} . money_amount").as_str(),
        // A quoted name is bare where the engine's `quote_identifier` leaves
        // it bare — a plain name, an unreserved keyword — and quoted where it
        // does not; and a Unicode-escaped name is the name it spells.
        format!("{s}.\"money_amount\"").as_str(),
        format!("\"{s}\".\"money_amount\"").as_str(),
        format!("{s}.\"zone\"").as_str(),
        format!("{s}.\"select\"").as_str(),
        // `$` is a name byte, and one the engine quotes.
        format!("{s}.money$type").as_str(),
        format!("{s}.U&\"\\006doney_amount\"").as_str(),
        format!("{s}.u&\"!006doney_amount\" UESCAPE '!'").as_str(),
        format!("U&\"{s}\".U&\"\\0073elect\"").as_str(),
        // And the escape character may be the punctuation an identity is
        // split on.
        format!("{s}.U&\",006doney_amount\" UESCAPE ','").as_str(),
        format!("{s}.U&\"money_amount\" UESCAPE ')'").as_str(),
    ]
    .map(str::to_owned);

    let pg = Postgres::new();
    let folded: Vec<String> = declared
        .iter()
        .map(|d| {
            let arg: pbps_model::RoutineArg = d.parse().expect("a routine argument");
            pbps_dialect::Dialect::normalize_routine_arg(&pg, &arg)
                .expect("normalize")
                .as_str()
                .to_owned()
        })
        .collect();

    conn.execute(&format!(
        "CREATE FUNCTION {s}.f({}) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        declared
            .iter()
            .enumerate()
            .map(|(i, d)| format!("a{i} {d}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .await
    .expect("the engine accepts every declared spelling");

    // The fold is ASCII, and that is the engine's rule rather than a
    // simplification of it. Measured here, before the schema goes: an unquoted
    // non-ASCII type name is neither lower-cased nor left bare — the engine
    // keeps the byte and quotes the name.
    conn.execute(&format!("CREATE TYPE {s}.\"Ätype\" AS ENUM ('a')"))
        .await
        .expect("a type whose name is not ASCII");
    conn.execute(&format!(
        "CREATE FUNCTION {s}.folded(a {s}.Ätype) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
    ))
    .await
    .expect("declared unquoted, which is what a Unicode fold would change");
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                  WHERE p.pronamespace = '{s}'::regnamespace AND p.proname = 'folded'"
            )
        )
        .await,
        format!("{s}.folded({s}.\"Ätype\")"),
        "the engine left the case alone; a fold that lower-cased it would name another type"
    );
    let ascii_only: pbps_model::RoutineArg = "Ätype".parse().expect("a routine argument");
    assert_eq!(
        pbps_dialect::Dialect::normalize_routine_arg(&pg, &ascii_only)
            .expect("normalize")
            .as_str(),
        "\"Ätype\"",
        "and so does the dialect, quoting the name as the engine does"
    );
    // And whitespace is ASCII to the engine for the same reason: a
    // non-breaking space is a name byte. Measured here, `a\u{a0}b` unquoted is
    // one identifier, kept and quoted, where `a b` with a plain space names no
    // type at all — so the fold that took Unicode's word for what a space is
    // turned a valid key into one that resolved nothing.
    conn.execute(&format!("CREATE TYPE {s}.\"a\u{a0}b\" AS ENUM ('a')"))
        .await
        .expect("a type whose name holds a non-breaking space");
    conn.execute(&format!(
        "CREATE FUNCTION {s}.nbsp(a {s}.a\u{a0}b) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
    ))
    .await
    .expect("declared unquoted, which is what a Unicode whitespace fold would change");
    let identity = format!("{s}.nbsp({s}.\"a\u{a0}b\")");
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                  WHERE p.pronamespace = '{s}'::regnamespace AND p.proname = 'nbsp'"
            )
        )
        .await,
        identity,
        "the engine kept the byte and quoted the name"
    );
    let kept: pbps_model::RoutineArg = format!("{s}.a\u{a0}b").parse().expect("a routine argument");
    let kept_folded = pbps_dialect::Dialect::normalize_routine_arg(&pg, &kept).expect("normalize");
    assert_eq!(
        kept_folded.as_str(),
        format!("{s}.\"a\u{a0}b\""),
        "the fold leaves the byte alone, and quotes the name as the engine does"
    );
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT '{s}.nbsp({kept_folded})'::regprocedure::text")
        )
        .await,
        identity,
        "and the folded key resolves to the routine the body created"
    );
    // And not the gap before the array keyword either: `a\u{a0}array` is a
    // type name, and peeled as `a[]` the key named a routine that was never
    // made.
    conn.execute(&format!(
        "CREATE TYPE {s}.\"a\u{a0}array\" AS ENUM ('a');
         CREATE FUNCTION {s}.nbsp_array(a {s}.a\u{a0}array) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
    ))
    .await
    .expect("a type whose name ends in the array keyword after a non-breaking space");
    let kept: pbps_model::RoutineArg = format!("{s}.a\u{a0}array")
        .parse()
        .expect("a routine argument");
    let kept_folded = pbps_dialect::Dialect::normalize_routine_arg(&pg, &kept).expect("normalize");
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT '{s}.nbsp_array({kept_folded})'::regprocedure::text")
        )
        .await,
        format!("{s}.nbsp_array({s}.\"a\u{a0}array\")"),
        "the folded key resolves to the routine the body created"
    );
    conn.execute(&format!(
        "DROP FUNCTION {s}.nbsp_array({s}.\"a\u{a0}array\"); DROP TYPE {s}.\"a\u{a0}array\""
    ))
    .await
    .expect("drop them too");
    conn.execute(&format!(
        "DROP FUNCTION {s}.nbsp({s}.\"a\u{a0}b\"); DROP TYPE {s}.\"a\u{a0}b\""
    ))
    .await
    .expect("drop them before the pull, which is about the other fixture");

    conn.execute(&format!("DROP FUNCTION {s}.folded({s}.\"Ätype\")"))
        .await
        .expect("drop it before the pull, which is about the other fixture");
    conn.execute(&format!("DROP TYPE {s}.\"Ätype\""))
        .await
        .expect("and its type");

    let pulled = our_modules(&pull(&mut conn).await, &s);
    drop_schema(&mut conn, &s).await;

    let expected: pbps_model::ModuleId = format!("{s}.f({})", folded.join(","))
        .parse()
        .expect("the folded identity parses");
    assert_eq!(
        pulled.keys().cloned().collect::<Vec<_>>(),
        vec![expected.clone()],
        "the dialect folded {declared:?} to {folded:?}, and the engine did not agree"
    );

    // And the fold is what makes them one key rather than two: the declared
    // spellings, unfolded, are a different identity.
    let unfolded: pbps_model::ModuleId = format!("{s}.f({})", declared.join(","))
        .parse()
        .expect("the declared identity parses");
    assert_ne!(
        unfolded, expected,
        "this fixture has to exercise the fold, and these spellings do not"
    );
}

/// The trigger check reads the name after `ON`, and this is why it has to.
///
/// **Measured**: a definition whose `ON` target is another table is accepted by
/// the engine without a word and creates the trigger there — under the key this
/// project believes points at `t`. A scan for the identity's table *anywhere*
/// in the text says yes to `AFTER UPDATE OF t ON <other>`, because the column
/// list mentions `t`, so the check that was supposed to prevent exactly this
/// waved it through.
///
/// Both halves are asserted: every spelling the validator accepts creates the
/// trigger on the table the identity names, and the one it refuses would have
/// created it somewhere else.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_triggers_on_clause_is_what_decides_where_it_lands() {
    let s = emit_schema("trigger_on");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE TABLE {s}.t (id int primary key, a text)"),
        format!("CREATE TABLE {s}.other (id int primary key, t text)"),
        format!(
            "CREATE FUNCTION {s}.trf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; \
             END $$"
        ),
    ] {
        conn.execute(&sql).await.expect("build");
    }

    let id: pbps_model::ModuleId = format!("{s}.t.audit").parse().expect("a module id");
    let pg = Postgres::new();
    let on_table = |body: &str| module(pbps_model::ModuleKind::Trigger, &body.replace("$S", &s));

    // Accepted, and each one lands on `t`.
    for body in [
        "AFTER INSERT ON $S.t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER UPDATE OF a ON $S.t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON \"$S\".\"t\" FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON $S.t FOR EACH ROW WHEN (new.a = 'on other') EXECUTE FUNCTION $S.trf()",
        // The dot is a token of its own to the engine: trivia around it is
        // trivia, and the trigger still lands on `t`.
        "AFTER INSERT ON $S . t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON $S /* schema */ .\n  t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON \"$S\" . \"t\" FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON /* c */ $S.t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON\n-- c\n$S.t FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        // A Unicode-escaped identifier, with the default escape and its own.
        "AFTER INSERT ON U&\"$S\".U&\"\\0074\" FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON u&\"$S\".\"t\" FOR EACH ROW EXECUTE FUNCTION $S.trf()",
        "AFTER INSERT ON U&\"$S\".U&\"!0074\" UESCAPE '!' FOR EACH ROW EXECUTE FUNCTION $S.trf()",
    ] {
        let m = on_table(body);
        assert!(
            pg.validate_module(&id, &m).is_empty(),
            "{body} must be accepted"
        );
        for stmt in pg
            .emit(
                &pbps_model::Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(m),
                },
                pbps_model::Strategy::default(),
            )
            .expect("emit")
        {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("{}\n{e}", stmt.sql));
        }
        assert_eq!(
            text(
                &mut conn,
                &format!(
                    "SELECT c.relname::text FROM pg_catalog.pg_trigger tg
                       JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = '{s}' AND tg.tgname = 'audit' AND NOT tg.tgisinternal"
                )
            )
            .await,
            "t",
            "{body} created the trigger on the wrong table"
        );
        conn.execute(&format!("DROP TRIGGER audit ON {s}.t"))
            .await
            .expect("drop the trigger");
    }

    // Refused — and this is what it would have done. The engine accepts it,
    // creates it on `other`, and the identity that says `t` finds nothing.
    let wrong = "AFTER UPDATE OF t ON $S.other FOR EACH ROW EXECUTE FUNCTION $S.trf()";
    let m = on_table(wrong);
    let refusal = pg.validate_module(&id, &m);
    assert_eq!(refusal.len(), 1, "{refusal:?}");
    assert!(refusal[0].to_string().contains(".other"), "{}", refusal[0]);

    conn.execute(&format!(
        "CREATE TRIGGER audit AFTER UPDATE OF t ON {s}.other FOR EACH ROW EXECUTE FUNCTION \
         {s}.trf()"
    ))
    .await
    .expect("the engine accepts it without a word");
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT c.relname::text FROM pg_catalog.pg_trigger tg
                   JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = '{s}' AND tg.tgname = 'audit' AND NOT tg.tgisinternal"
            )
        )
        .await,
        "other",
        "precondition: this is the mismatch the validator exists to refuse"
    );
    let not_there = conn
        .execute(&format!("DROP TRIGGER audit ON {s}.t"))
        .await
        .expect_err("and the key that says `t` finds nothing, a plan later");
    assert_eq!(sqlstate(&not_there), "42704", "{not_there:?}");

    drop_schema(&mut conn, &s).await;
}

/// A user rule on a view is a dependent the engine deletes with the view and
/// the rebuild does not put back — measured, `DROP VIEW` then `CREATE VIEW`
/// leaves `pg_rewrite` with only the `_RETURN` row. Its `pg_depend` edges reach
/// the view arm, whose `ev_class` is the view itself, so the reader reported
/// the view as its own dependent and the walk discarded it as the root: the
/// rebuild went ahead and the rule was silently gone.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_rule_on_the_view_is_named_and_refused_because_the_rebuild_would_lose_it() {
    let s = emit_schema("view_rule");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE TABLE {s}.t (id int primary key)"),
        format!("CREATE TABLE {s}.log (id int)"),
        format!("CREATE VIEW {s}.v AS SELECT id FROM {s}.t"),
        format!(
            "CREATE RULE ins AS ON INSERT TO {s}.v DO INSTEAD INSERT INTO {s}.log VALUES (NEW.id)"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let v: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let found = pbps_pg::modules::dependents(&mut conn, &v, pbps_model::ModuleKind::View)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    let described: Vec<&str> = found.iter().map(|d| d.described.as_str()).collect();
    assert_eq!(
        described,
        vec![format!("rule ins on view {s}.v")],
        "the rule is a dependent of its own view, and the view is not"
    );
    assert!(
        matches!(&found[0].holds, pbps_pg::modules::Holds::Unrepresentable(why) if why.contains("rewrite rule")),
        "{:?}",
        found[0].holds
    );

    // Declared or not, it refuses: the model has nothing to recreate it from.
    let mut declared = Schema::default();
    declared.modules.insert(
        v.clone(),
        module(
            pbps_model::ModuleKind::View,
            &format!("AS SELECT id FROM {s}.t"),
        ),
    );
    let refusal = pbps_pg::modules::unmanaged_refusal(&v, &found, &declared)
        .expect("a rule refuses the rebuild");
    assert!(refusal.contains("rule ins"), "{refusal}");

    // The premise, measured: the engine drops the rule with the view, and the
    // rebuild the plan would have emitted does not put it back.
    conn.execute(&format!(
        "DROP VIEW {s}.v; CREATE VIEW {s}.v AS SELECT id FROM {s}.t"
    ))
    .await
    .expect("the rebuild");
    assert_eq!(
        number(
            &mut conn,
            &format!(
                "SELECT count(*)::int FROM pg_catalog.pg_rewrite
                  WHERE ev_class = '{s}.v'::regclass AND rulename <> '_RETURN'"
            )
        )
        .await,
        0,
        "a rule that survived the rebuild would make this refusal needless"
    );

    drop_schema(&mut conn, &s).await;
}

/// A trigger on a relation the table reader leaves out — a partitioned table,
/// an `UNLOGGED` one — came back in the pull without the relation it is on, and
/// `check_names` refused the whole schema: a pull nothing could load. Measured,
/// the engine allows a trigger on both. The trigger is left out with its
/// relation now, and named beside it rather than silently gone.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_trigger_on_a_relation_the_pull_leaves_out_is_left_out_with_it_and_named() {
    let s = emit_schema("trigger_unheld");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!(
            "CREATE FUNCTION {s}.tg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; \
             END $$"
        ),
        format!("CREATE TABLE {s}.t (id int primary key)"),
        format!(
            "CREATE TRIGGER audit BEFORE INSERT ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.tg()"
        ),
        format!("CREATE TABLE {s}.part (id int, d date) PARTITION BY RANGE (d)"),
        format!(
            "CREATE TRIGGER audit BEFORE INSERT ON {s}.part FOR EACH ROW EXECUTE FUNCTION {s}.tg()"
        ),
        format!("CREATE UNLOGGED TABLE {s}.scratch (id int)"),
        format!(
            "CREATE TRIGGER audit BEFORE INSERT ON {s}.scratch FOR EACH ROW EXECUTE FUNCTION \
             {s}.tg()"
        ),
        // A view an extension owns is left out of the pull (DECISIONS 305); a
        // user's trigger on it is not extension-owned — measured — and has to
        // go with the view. An extension is database-global, and a run that
        // panicked before its `drop_schema` leaves this one installed in a
        // schema that is not this run's — so it is dropped first, not created
        // `IF NOT EXISTS`, which would keep the stale one where it is.
        "DROP EXTENSION IF EXISTS pg_buffercache".to_owned(),
        format!("CREATE EXTENSION pg_buffercache WITH SCHEMA {s}"),
        format!(
            "CREATE TRIGGER user_tg INSTEAD OF INSERT ON {s}.pg_buffercache FOR EACH ROW EXECUTE \
             FUNCTION {s}.tg()"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let pulled = pull(&mut conn).await;
    let modules = our_modules(&pulled, &s);
    let triggers: Vec<String> = modules
        .keys()
        .filter(|id| matches!(id, pbps_model::ModuleId::Trigger { .. }))
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        triggers,
        vec![format!("{s}.t.audit")],
        "only the trigger whose table the pull holds"
    );

    // What the trigger without its table broke: the pull's own schema is one
    // the model refuses to name-check. Scoped to this schema, because the
    // container is shared.
    let ours = Schema {
        tables: pulled
            .schema
            .tables
            .iter()
            .filter(|(name, _)| name.schema == s)
            .map(|(name, table)| (name.clone(), table.clone()))
            .collect(),
        modules,
        ..Schema::default()
    };
    let problems = pbps_model::module::check_names(&ours);
    assert!(problems.is_empty(), "{problems:?}");

    // Named, beside the relations they are on, which are named already.
    let named: Vec<&str> = ours_limitations(&pulled, &s)
        .iter()
        .map(|l| l.table.name.as_str())
        .collect();
    for left_out in [
        "part",
        "part.audit",
        "scratch",
        "scratch.audit",
        "pg_buffercache.user_tg",
    ] {
        assert!(
            named.contains(&left_out),
            "{left_out} is not named in {named:?}"
        );
    }
    assert!(!named.contains(&"t.audit"), "{named:?}");
    assert!(
        !named.contains(&"pg_buffercache"),
        "the extension's view is left out silently, and only the user's trigger on it is named: \
         {named:?}"
    );
    let detail = &ours_limitations(&pulled, &s)
        .iter()
        .find(|l| l.table.name == "part.audit")
        .expect("named")
        .detail;
    assert!(
        detail.contains(&format!("a trigger on `{s}.part`")),
        "{detail}"
    );

    drop_schema(&mut conn, &s).await;
}

/// A routine that takes the view's row type, or an array of it, or returns
/// it, depends on `type v` — and the type depends on the view internally, an
/// edge the walk rightly does not follow. Measured, `DROP VIEW v` names all
/// three routines. Asked only about edges to the view itself, the walk saw
/// none of them and called the rebuild unblocked.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_routine_that_takes_or_returns_the_views_row_type_is_a_dependent_of_the_view() {
    let s = emit_schema("row_type");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    for sql in [
        format!("CREATE TABLE {s}.t (id int primary key)"),
        format!("CREATE VIEW {s}.v AS SELECT id FROM {s}.t"),
        format!("CREATE FUNCTION {s}.takes(x {s}.v) RETURNS int LANGUAGE sql AS $$ SELECT x.id $$"),
        format!(
            "CREATE FUNCTION {s}.takes_many(x {s}.v[]) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"
        ),
        format!(
            "CREATE FUNCTION {s}.gives() RETURNS {s}.v LANGUAGE sql AS $$ SELECT * FROM {s}.v \
             LIMIT 1 $$"
        ),
    ] {
        conn.execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let v: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");
    in_a_transaction(&mut conn).await;
    let found = pbps_pg::modules::dependents(&mut conn, &v, pbps_model::ModuleKind::View)
        .await
        .expect("read the dependents");
    rollback(&mut conn).await;
    let described: Vec<&str> = found.iter().map(|d| d.described.as_str()).collect();
    assert_eq!(
        described,
        vec![
            format!("function {s}.gives()"),
            format!("function {s}.takes({s}.v)"),
            format!("function {s}.takes_many({s}.v[])"),
        ],
        "every routine the engine would name"
    );
    // With their identities whole: a routine the dependents query returns and
    // the argument query does not is keyed `f()`, a different object.
    let holds: Vec<pbps_pg::modules::Holds> = found.iter().map(|d| d.holds.clone()).collect();
    for id in [
        format!("{s}.gives()"),
        format!("{s}.takes({s}.v)"),
        format!("{s}.takes_many({s}.v[])"),
    ] {
        let id: pbps_model::ModuleId = id.parse().expect("a module id");
        assert!(
            holds.contains(&pbps_pg::modules::Holds::Module(id.clone())),
            "{id} is not held as itself in {holds:?}"
        );
    }

    // The engine's own refusal names the same three.
    let engine = conn
        .execute(&format!("DROP VIEW {s}.v"))
        .await
        .expect_err("the engine refuses too");
    assert_eq!(sqlstate(&engine), "2BP01", "{engine:?}");

    drop_schema(&mut conn, &s).await;
}

/// A module body reaches the engine byte for byte, ASCII whitespace at its
/// ends excepted — and only ASCII.
///
/// **Measured**: a non-breaking space is an identifier byte to this engine, so
/// `SELECT 1 AS x\u{a0}` names a two-character column. A trim that took
/// Unicode's word for what whitespace is cut the byte off the end of the body,
/// and the view the plan created had a column the declaration does not name
/// (DECISIONS 313).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_bodys_trailing_non_ascii_byte_reaches_the_engine() {
    let s = emit_schema("nbsp_body");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let id: pbps_model::ModuleId = format!("{s}.v").parse().expect("a module id");
    let view = module(pbps_model::ModuleKind::View, " \n SELECT 1 AS x\u{a0}\n ");
    for statement in pg
        .emit(
            &pbps_model::Change::CreateModule {
                id: id.clone(),
                module: Box::new(view),
            },
            Strategy::default(),
        )
        .expect("emit the create")
    {
        conn.execute(&statement.sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", statement.sql));
    }
    let column = text(
        &mut conn,
        &format!(
            "SELECT a.attname FROM pg_catalog.pg_attribute a
              WHERE a.attrelid = '{s}.v'::regclass AND a.attnum > 0"
        ),
    )
    .await;
    drop_schema(&mut conn, &s).await;
    assert_eq!(
        column, "x\u{a0}",
        "the byte the declaration ends with is the byte the engine read"
    );
}

/// A plan that creates a routine and a view over `ARRAY[routine()]` creates
/// the routine first.
///
/// **Measured**: `CREATE VIEW ao.a AS SELECT (ARRAY[ao.z()])[1]` is refused
/// with `function ao.z() does not exist` until the function is there. The
/// dependency scan is the one that orders the two, and it read `[` as the
/// quote it is on the other engine — dropped it, glued `ARRAY[ao.z` into one
/// word, and saw no edge (DECISIONS 239).
/// Measured: `FROM select` is a syntax error and `FROM "select"` names the
/// view, so the keyword that opens every view mentions no view named
/// `select`. Read as a mention, it drew an edge from `z` to `select`; with
/// `select` selecting from `z`, a cycle, and `select` was created first —
/// over a view that did not exist yet.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_view_named_by_a_reserved_word_is_created_after_the_view_it_selects_from() {
    let s = emit_schema("reserved_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.select").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {s}.z"),
        ),
    );
    declared.modules.insert(
        format!("{s}.z").parse().expect("a module id"),
        module(pbps_model::ModuleKind::View, "select 1 AS x"),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z"), format!("{s}.select")],
        "the view that is selected from comes first, whatever its keywords spell"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT x::text FROM {s}.\"select\"")).await,
        "1"
    );
    drop_schema(&mut conn, &s).await;
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_view_over_an_array_of_a_routine_is_created_after_the_routine() {
    let s = emit_schema("array_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.a").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT (ARRAY[{s}.z()])[1] AS v"),
        ),
    );
    declared.modules.insert(
        format!("{s}.z()").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        ),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z()"), format!("{s}.a")],
        "the routine the view's array holds comes first"
    );
    // And the engine agrees with the order: the plan applies as written.
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT v::text FROM {s}.a")).await,
        "1"
    );
    drop_schema(&mut conn, &s).await;
}

/// A type name that ends in a literal's prefix letter is a name, and the
/// literal after it is a plain one: measured, with a domain `s.code`, `(a
/// s.code DEFAULT s.code'x\', b integer DEFAULT 1)` is accepted and the
/// default reads back as `'x\'::text`. Read from the `e'`, the scan took an
/// escape string, the `\'` never closed it, and the gate refused a routine
/// the engine creates under exactly the declared key.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_type_name_ending_in_a_prefix_letter_is_not_a_literals_prefix() {
    let s = emit_schema("prefix_letter");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    conn.execute(&format!("CREATE DOMAIN {s}.code AS text"))
        .await
        .expect("a domain whose name ends in the escape prefix");
    let pg = Postgres::new();
    let mut declared = Schema::default();
    let id: pbps_model::ModuleId = format!("{s}.f({s}.code,integer)")
        .parse()
        .expect("a module id");
    let definition = format!(
        "(a {s}.code DEFAULT {s}.code'x\\', b integer DEFAULT 1) RETURNS int LANGUAGE sql AS $$ \
         SELECT 1 $$"
    );
    let module = module(pbps_model::ModuleKind::Function, &definition);
    assert!(
        pbps_dialect::Dialect::validate_module(&pg, &id, &module).is_empty(),
        "the gate refused a declaration the engine accepts under this key"
    );
    declared.modules.insert(id.clone(), module);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.oid::regprocedure::text FROM pg_proc p WHERE p.pronamespace = \
                 '{s}'::regnamespace"
            )
        )
        .await,
        format!("{s}.f({s}.code,integer)")
    );
    drop_schema(&mut conn, &s).await;
}

/// A bare name resolves in the *first* schema of the write path that holds
/// it: measured, with `z.p` and `a.p` both present, a bare `p` under `SET
/// search_path = "z", "a"` binds `z.p`. Counting `a.p` as a candidate too
/// invented an edge that closed a cycle with the real one, and name order
/// then created `a.p` before the view it selects from.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_bare_name_means_the_first_schema_of_the_path_that_has_it() {
    let base = emit_schema("path_precedence");
    // The extra sorts first, so a scan that invents the edge orders the plan
    // the wrong way round rather than getting it right by accident.
    let own = format!("{base}_z");
    let extra = format!("{base}_a");
    let mut conn = connect().await;
    fresh(&mut conn, &own).await;
    fresh(&mut conn, &extra).await;
    let pg = Postgres::with_write_path_extras(vec![extra.clone()]);
    let mut declared = Schema::default();
    for (id, definition) in [
        (format!("{own}.x"), "SELECT * FROM p".to_owned()),
        (format!("{own}.p"), "SELECT 1 AS v".to_owned()),
        (format!("{extra}.p"), format!("SELECT * FROM {own}.x")),
    ] {
        declared.modules.insert(
            id.parse().expect("a module id"),
            module(pbps_model::ModuleKind::View, &definition),
        );
    }
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: &declared,
            ids: &ids,
        },
        &pg,
        &pbps_model::Hints::default(),
    )
    .expect("diff");
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{own}.p"), format!("{own}.x"), format!("{extra}.p")],
        "the bare `p` means the nearer one, and only that edge is real"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT v::text FROM {own}.x")).await,
        "1"
    );
    drop_schema(&mut conn, &extra).await;
    drop_schema(&mut conn, &own).await;
}

/// A quote doubled inside a name is one character of the name: measured,
/// `s."z""q"` names the view `z"q`, and a view over it is written `FROM
/// s."z""q"`. Read as two delimiters the scan found no edge at all, and with
/// `a` sorting first its `CREATE` came before the view it selects from.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_quote_inside_a_name_is_read_as_part_of_it() {
    let s = emit_schema("quoted_name_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.a").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {s}.\"z\"\"q\""),
        ),
    );
    declared.modules.insert(
        format!("{s}.z\"q").parse().expect("a module id"),
        module(pbps_model::ModuleKind::View, "SELECT 1 AS x"),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z\"q"), format!("{s}.a")],
        "the view that is selected from comes first, quote in its name and all"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT x::text FROM {s}.a")).await,
        "1"
    );
    drop_schema(&mut conn, &s).await;
}

/// A `UESCAPE` clause is part of the literal it follows: measured,
/// `U&'d!0061ta' uescape '!'` is the string `data`, in either case. Read as
/// code, the word
/// mentioned the view named `uescape`, closed a cycle with the real edge, and
/// `uescape` sorts first — so it was created over a view that did not exist.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_uescape_clause_does_not_order_the_view_that_holds_it() {
    let s = emit_schema("uescape_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.uescape").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {s}.z"),
        ),
    );
    declared.modules.insert(
        format!("{s}.z").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            "SELECT U&'d!0061ta' uescape '!' AS s, U&'a''b' uescape '!' AS t",
        ),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z"), format!("{s}.uescape")],
        "the view that is selected from comes first, whatever its literal spells"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT s FROM {s}.uescape")).await,
        "data"
    );
    drop_schema(&mut conn, &s).await;
}

/// A bare name resolves through the write path — the view's own schema and
/// the configured extras — and nowhere else. Measured: with no extras,
/// `a.x AS SELECT * FROM b.z` over `b.z AS SELECT 1 AS x` is a valid plan,
/// and the alias `x` read as a mention of `a.x` closed a cycle that created
/// `a.x` over a view that did not exist yet.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_bare_name_in_another_schema_is_not_an_edge() {
    let s = emit_schema("path_order");
    let other = format!("{s}_b");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &other).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.x").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {other}.z"),
        ),
    );
    declared.modules.insert(
        format!("{other}.z").parse().expect("a module id"),
        module(pbps_model::ModuleKind::View, "SELECT 1 AS x"),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{other}.z"), format!("{s}.x")],
        "the view that is selected from comes first, whatever its columns are called"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT x::text FROM {s}.x")).await,
        "1"
    );
    drop_schema(&mut conn, &other).await;
    drop_schema(&mut conn, &s).await;
}

/// A body's dollar tags are delimiters: a view named `$a$` over a routine
/// whose body is delimited by `$a$` is created after it. Read as code, the
/// tags mentioned the view from inside the routine, the cycle was broken by
/// name order, and `$` sorts before `z`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_bodys_dollar_tags_are_not_the_name_of_a_module() {
    let s = emit_schema("tag_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.$a$").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT {s}.z() AS v"),
        ),
    );
    declared.modules.insert(
        format!("{s}.z()").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::Function,
            "() RETURNS int LANGUAGE sql AS $a$ SELECT 1 $a$",
        ),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z()"), format!("{s}.$a$")],
        "the routine the view calls comes first, whatever delimits its body"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT v::text FROM {s}.\"$a$\"")).await,
        "1"
    );
    drop_schema(&mut conn, &s).await;
}

/// Measured: `FROM dq.U&"\007a"` selects from `dq.z`. The scan that read
/// the spelling on the page found no `z` in it, and with `a` sorting first,
/// created `a` over a view that did not exist yet.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_view_named_by_a_unicode_escape_is_created_after_the_view_it_names() {
    let s = emit_schema("unicode_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.a").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {s}.U&\"\\007a\""),
        ),
    );
    declared.modules.insert(
        format!("{s}.b").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM U&\"{s}\".U&\"!007a\" UESCAPE '!'"),
        ),
    );
    declared.modules.insert(
        format!("{s}.z").parse().expect("a module id"),
        module(pbps_model::ModuleKind::View, "SELECT 1 AS x"),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z"), format!("{s}.a"), format!("{s}.b")],
        "the view that is selected from comes first, however its name is spelled"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT x::text FROM {s}.b")).await,
        "1"
    );
    drop_schema(&mut conn, &s).await;
}

/// A plan that creates two views orders them by what each really selects
/// from, not by a name the other engine's lexer would read out of a literal.
///
/// **Measured**: `SELECT E'x\' , es.a' AS s` is one literal to this engine.
/// Read with the shared scanner's rules it closed at the `\'`, `, es.a` was
/// code, and the scan drew an edge from `b` to `a`; with `a` selecting from
/// `b`, that closed a cycle, the two were emitted in name order, and
/// `CREATE VIEW a` failed inside the plan's transaction (DECISIONS 315).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_name_inside_an_escape_string_does_not_order_the_view_that_holds_it() {
    let s = emit_schema("escape_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    // A type whose name ends in the byte a Unicode trim discards, applied to
    // a dollar-quoted string: a datum after what only looks like `AS`.
    conn.execute(&format!("CREATE DOMAIN {s}.\"as\u{a0}\" AS text"))
        .await
        .expect("a domain named as-nbsp");
    let pg = Postgres::new();
    let mut declared = Schema::default();
    declared.modules.insert(
        format!("{s}.a").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!("SELECT * FROM {s}.b"),
        ),
    );
    declared.modules.insert(
        format!("{s}.b").parse().expect("a module id"),
        module(
            pbps_model::ModuleKind::View,
            &format!(
                "SELECT E'x\\' , {s}.a' AS s, $$ {s}.a $$ AS t, {s}.as\u{a0} $$ {s}.a $$ AS u"
            ),
        ),
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.b"), format!("{s}.a")],
        "the view that is selected from comes first, whatever its literal says"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT s FROM {s}.a")).await,
        format!("x' , {s}.a"),
        "and the literal reached the engine as the one string it is"
    );
    drop_schema(&mut conn, &s).await;
}

/// Two views over a third are ordered after it whatever the third's text
/// looks like to the other engine's lexer: the `E` of an escape string is not
/// a name, and a non-breaking space does not end one.
///
/// **Measured**: `SELECT E'x' AS s, 1 AS x\u{a0}y` has the columns `s` and
/// the three-character `x\u{a0}y`. Read with the shared scanner's rules, the
/// `E` matched a view named `e` and the gap in `x\u{a0}y` exposed a word `y`
/// — an edge from `z` to each, a cycle with each one's real edge to `z`, and
/// name order put the dependent first (DECISIONS 315).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_literals_prefix_and_a_non_ascii_byte_are_not_names_the_order_is_decided_by() {
    let s = emit_schema("prefix_order");
    let mut conn = connect().await;
    fresh(&mut conn, &s).await;
    let pg = Postgres::new();
    let mut declared = Schema::default();
    for (name, body) in [
        ("e", format!("SELECT * FROM {}.Z", s.to_uppercase())),
        ("y", format!("SELECT * FROM {s}.z")),
        ("z", "SELECT E'x' AS s, 1 AS x\u{a0}y".to_owned()),
    ] {
        declared.modules.insert(
            format!("{s}.{name}").parse().expect("a module id"),
            module(pbps_model::ModuleKind::View, &body),
        );
    }
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec![format!("{s}.z"), format!("{s}.e"), format!("{s}.y")],
        "the view both select from comes first"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT string_agg(a.attname, ',' ORDER BY a.attnum) FROM pg_catalog.pg_attribute a
                  WHERE a.attrelid = '{s}.y'::regclass AND a.attnum > 0"
            ),
        )
        .await,
        "s,x\u{a0}y",
        "and the alias reached the engine as the one name it is"
    );
    drop_schema(&mut conn, &s).await;
}
// Reference data (Phase 5 step 7; ADR-0004, ADR-0013 §1–§3, §5)
// ---------------------------------------------------------------------------

use pbps_model::{DataMode, Row, RowKey, TableData, Value};

fn data_schema(test: &str) -> String {
    format!("pbps_data_{}_{test}", std::process::id())
}

/// One declared row.
fn row(cells: &[(&str, Value)]) -> Row {
    Row(cells
        .iter()
        .map(|(c, v)| ((*c).to_owned(), v.clone()))
        .collect())
}

fn with_data(table: &mut Table, mode: DataMode, rows: &[(&str, Row)]) {
    table.data = Some(TableData {
        mode,
        rows: rows
            .iter()
            .map(|(k, r)| (RowKey::from(*k), r.clone()))
            .collect(),
    });
}

/// The first column of the first row, as an `int4` — what a probe returns,
/// and the width the probe runner reads it at (`deploy::preflight` asks for
/// an `i32`, and the driver does not widen an `int8` into one; DECISIONS 342).
async fn counted(conn: &mut Conn, sql: &str) -> i64 {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    i64::from(
        rows.first()
            .expect("one row")
            .try_get_at::<i32>(0)
            .unwrap_or_else(|e| panic!("an int4 column, as the runner reads it: {e}\n{sql}"))
            .expect("not null"),
    )
}

/// The base a connected plan is made against: the live schema with the rows
/// the read-back found, resolved the way `plan --db` resolves them.
async fn connected_base(conn: &mut Conn, declared: &Schema, schema: &str) -> Schema {
    let live = ours_only(&pull(conn).await, schema);
    let scopes = declared.data_scopes();
    let rows = pbps_pg::catalog::read_rows(
        conn,
        &live,
        &pbps_model::data::read_scopes(&Default::default(), &scopes),
    )
    .await
    .expect("read the declared rows back");
    pbps_model::data::plan_base(&live, &rows, &Default::default(), declared)
        .expect("no two declared keys are one row")
}

/// The rows of one table as the engine holds them, keyed by its own spelling.
async fn observed(
    conn: &mut Conn,
    declared: &Schema,
    schema: &str,
    name: &TableName,
) -> pbps_model::ObservedTable {
    let live = ours_only(&pull(conn).await, schema);
    let scopes = declared.data_scopes();
    let mut rows = pbps_pg::catalog::read_rows(
        conn,
        &live,
        &pbps_model::data::read_scopes(&Default::default(), &scopes),
    )
    .await
    .expect("read the declared rows back");
    rows.remove(name).expect("the table was read")
}

/// The whole reference-data path on this engine: the DML reaches the server in
/// an order it accepts, every declared value reads back as it was written, and
/// the plan taken straight afterwards is empty.
///
/// The empty plan is the assertion that matters. A value the engine stores in
/// its own spelling — `1.5` into a `numeric(5,2)`, a padded `character(5)` —
/// would otherwise be restated by every connected plan for ever, which is the
/// failure DECISIONS 101 exists for and the one a read-back written against the
/// wrong rendering produces silently.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn declared_rows_reach_the_engine_and_read_back_as_declared() {
    let mut conn = connect().await;
    let s = data_schema("roundtrip");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("varchar(20)")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table
        .columns
        .insert("rank".into(), Column::new(ty("integer")));
    table
        .columns
        .insert("pct".into(), Column::new(ty("numeric(5,2)")));
    table
        .columns
        .insert("live".into(), Column::new(ty("boolean")));
    table
        .columns
        .insert("since".into(), Column::new(ty("date")));
    table
        .columns
        .insert("blob".into(), Column::new(ty("bytea")));
    let mut noted = Column::new(ty("text"));
    noted.default = Some("'unnamed'::text".into());
    table.columns.insert("note".into(), noted);
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[
            (
                "new",
                row(&[
                    ("label", Value::Text("New".into())),
                    ("rank", Value::Int(1)),
                    // The engine's own spelling, which is what the read-back
                    // produces: a declaration of `1.5` is refused by the
                    // spelling check, not silently stored.
                    ("pct", Value::Text("1.50".into())),
                    ("live", Value::Bool(true)),
                    ("since", Value::Text("2026-01-02".into())),
                    ("blob", Value::Text("\\x0102".into())),
                    ("note", Value::Text("spelled".into())),
                ]),
            ),
            (
                "old",
                // Every other cell left to the table: NULL where there is no
                // default, the default where there is one.
                row(&[("label", Value::Text("Old".into()))]),
            ),
        ],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);

    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    let creation = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    // The rows are in the plan as typed DML, not as part of the `CREATE`.
    assert_eq!(
        creation
            .changes
            .iter()
            .filter(|p| matches!(p.change, pbps_model::Change::InsertRow { .. }))
            .count(),
        2,
        "{creation:#?}"
    );
    apply(&mut conn, &pg, &creation).await;

    let seen = observed(&mut conn, &declared, &s, &name).await;
    assert_eq!(seen.rows.len(), 2, "{seen:#?}");
    let new = &seen.rows[&RowKey::from("new")];
    assert_eq!(new.cells.get("pct"), Some(&Value::Text("1.50".into())));
    assert_eq!(new.cells.get("live"), Some(&Value::Bool(true)));
    assert_eq!(new.cells.get("rank"), Some(&Value::Int(1)));
    assert_eq!(
        new.cells.get("blob"),
        Some(&Value::Text("\\x0102".into())),
        "a bytea reads back in the spelling the declaration writes"
    );
    // The cell that holds the column's default is reported as such, so the
    // declaration that omits it and the one that spells it both compare equal.
    let old = &seen.rows[&RowKey::from("old")];
    assert!(old.at_default.contains("note"), "{old:#?}");
    assert!(!new.at_default.contains("note"), "{new:#?}");

    let base = connected_base(&mut conn, &declared, &s).await;
    let again = plan(&base, &ids, &declared, &ids);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    assert!(again.is_empty(), "the plan after the apply: {again:#?}");
}

/// A hand edit to a declared row is seen, and the row is put back — and the
/// statement that puts it back refuses to run if the row moved again after the
/// plan was made (DECISIONS 122).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_hand_edited_row_is_seen_and_the_update_holds_what_the_plan_recorded() {
    let mut conn = connect().await;
    let s = data_schema("drift");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("a", row(&[("label", Value::Text("One".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    // Somebody edits the row by hand.
    conn.execute(&format!(
        "UPDATE {s}.status SET label = 'Edited' WHERE code = 'a'"
    ))
    .await
    .expect("the hand edit");

    let base = connected_base(&mut conn, &declared, &s).await;
    let fix = plan(&base, &ids, &declared, &ids);
    assert_eq!(fix.changes.len(), 1, "{fix:#?}");
    assert!(
        matches!(fix.changes[0].change, pbps_model::Change::UpdateRow { .. }),
        "{fix:#?}"
    );

    // And the row moves again before the plan runs: the statement holds itself
    // to what it recorded rather than overwriting whatever is there now.
    conn.execute(&format!(
        "UPDATE {s}.status SET label = 'Moved again' WHERE code = 'a'"
    ))
    .await
    .expect("the second edit");
    let stmt = pg
        .emit(&fix.changes[0].change, fix.changes[0].strategy)
        .expect("emit")
        .remove(0);
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the row is not as the plan recorded it");
    // `P0001`: a `RAISE EXCEPTION` with no condition name of its own.
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    let held = text(&mut conn, &format!("SELECT label FROM {s}.status")).await;
    assert_eq!(held, "Moved again", "the refused update wrote nothing");

    // With the row back where the plan recorded it, the same statement runs.
    conn.execute(&format!(
        "UPDATE {s}.status SET label = 'Edited' WHERE code = 'a'"
    ))
    .await
    .expect("put it back");
    conn.execute(&stmt.sql).await.expect("the recorded row");
    assert_eq!(
        text(&mut conn, &format!("SELECT label FROM {s}.status")).await,
        "One"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// ADR-0013 §1: `NOT VALID` is not `NOCHECK`, and the probe that assumed it
/// was would let a delete through that this engine refuses.
///
/// The measurement is in the test, not only in the document: the child row is
/// behind a `NOT VALID` foreign key, so `convalidated` is `f`, and the engine
/// still refuses the delete. A probe that skipped the key — which is what the
/// SQL Server rule one crate away does with its own flag — would count zero and
/// report the delete safe.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn the_pre_delete_probe_counts_a_foreign_key_this_engine_never_stopped_enforcing() {
    let mut conn = connect().await;
    let s = data_schema("notvalid");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, parent text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');
         ALTER TABLE {s}.child ADD CONSTRAINT fk_child
             FOREIGN KEY (parent) REFERENCES {s}.parent(code) NOT VALID;"
    ))
    .await
    .expect("the fixture");
    // The flag that looks like SQL Server's, and does not mean what it does.
    assert!(
        !truth(
            &mut conn,
            &format!(
                "SELECT convalidated FROM pg_constraint \
                 WHERE conname = 'fk_child' AND connamespace = '{s}'::regnamespace"
            )
        )
        .await,
        "the fixture's key is NOT VALID"
    );

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    with_data(
        declared.tables.get_mut(&name).expect("the table"),
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert_eq!(cs.changes.len(), 1, "{cs:#?}");
    assert!(
        matches!(cs.changes[0].change, pbps_model::Change::DeleteRow { .. }),
        "{cs:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    // The count, and beside it the refusal that says whether this session
    // could complete it at all (DECISIONS 333); nothing here has a policy, so
    // that one counts nothing.
    assert_eq!(probes.len(), 2, "{probes:#?}");
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        1,
        "the child behind the NOT VALID key is counted: {}",
        probes[0].sql
    );
    assert_eq!(counted(&mut conn, &probes[1].sql).await, 0);

    // And the engine agrees, which is the half that makes the count matter: a
    // `NOT VALID` key enforces the delete action in full.
    let engine = conn
        .execute(&format!("DELETE FROM {s}.parent WHERE code = 'old'"))
        .await
        .expect_err("a NOT VALID key still refuses the delete");
    assert_eq!(sqlstate(&engine), "23503", "{engine:?}");

    // The statement pbps emits stops earlier than that, on its own guard —
    // which is the point of the guard: the count is taken inside the delete's
    // own block, so a child that arrived after the probe is refused rather
    // than cascaded away (DECISIONS 129).
    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the guard counts the child the probe counted");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");

    // With the child gone, the same probe counts nothing and the same
    // statement runs — so the count is about the rows, not about the key.
    conn.execute(&format!("DELETE FROM {s}.child"))
        .await
        .expect("clear the child");
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 0);
    conn.execute(&stmt.sql).await.expect("the delete");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// ADR-0013 §2, measured rather than asserted: the construct SQL Server
/// supports is refused here because this engine's equivalent leaves the
/// sequence behind, and the failure lands in the *application* after a
/// deployment that verified clean.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_identity_keyed_data_block_is_refused_and_the_hazard_is_measured() {
    let mut conn = connect().await;
    let s = data_schema("pinned");
    fresh(&mut conn, &s).await;

    // What `OVERRIDING SYSTEM VALUE` would do, if pbps offered it.
    conn.execute(&format!(
        "CREATE TABLE {s}.pinned (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
         INSERT INTO {s}.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (1, 'one'), (2, 'two');"
    ))
    .await
    .expect("pin two rows");
    let collision = conn
        .execute(&format!(
            "INSERT INTO {s}.pinned (v) VALUES ('the next one')"
        ))
        .await
        .expect_err("the sequence never learned that 1 and 2 were used");
    assert_eq!(sqlstate(&collision), "23505", "{collision:?}");

    // So the declaration is refused, offline, naming the sequence and both
    // ways forward.
    let name = TableName::new(&s, "pinned");
    let mut table = Table::default();
    let mut id = Column::new(ty("integer")).not_null();
    id.identity = Some(Identity {
        seed: 1,
        increment: 1,
    });
    table.columns.insert("id".into(), id);
    table.columns.insert("v".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("1", row(&[("v", Value::Text("one".into()))]))],
    );
    let problems = Postgres::new().validate_table(&name, &table);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    let message = problems[0].to_string();
    assert!(message.contains(&format!("{s}.pinned_id_seq")), "{message}");
    assert!(message.contains("pbps baseline"), "{message}");
    // And the sequence the message names is the one this engine made.
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT pg_get_serial_sequence('{s}.pinned', 'id')")
        )
        .await,
        format!("{s}.pinned_id_seq")
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// ADR-0013 §3, the half that fails silently: what pbps renders has to mean
/// one thing under either `standard_conforming_strings`, because the setting
/// reaches the *write* and the write takes no scope of its own.
///
/// The `bytea` line is why this cannot be a refusal list: canonical hex under
/// `off` is accepted and stores different bytes, with no error anywhere.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_backslash_and_a_bytea_mean_one_thing_under_either_string_setting() {
    let mut conn = connect().await;
    let s = data_schema("escapes");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "payload");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table.columns.insert("note".into(), Column::new(ty("text")));
    table
        .columns
        .insert("blob".into(), Column::new(ty("bytea")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    // Two characters, a backslash and an `n`, which `off` would fold into one
    // newline; and two bytes, which `off` would store as three.
    with_data(
        &mut table,
        DataMode::Exact,
        &[(
            "a",
            row(&[
                ("note", Value::Text("a\\nb".into())),
                ("blob", Value::Text("\\x0102".into())),
            ]),
        )],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    let creation = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);

    // Nothing pbps renders carries a bare backslash into a plain literal: the
    // value is an `E'…'` with the backslash doubled, and the `bytea` is a
    // `decode`, which has none at all.
    let sql: String = creation
        .changes
        .iter()
        .flat_map(|p| pg.emit(&p.change, p.strategy).expect("emit"))
        .map(|st| st.sql)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(sql.contains(r"E'a\\nb'"), "{sql}");
    assert!(sql.contains("decode(E'0102', 'hex')"), "{sql}");

    for setting in ["on", "off"] {
        conn.execute(&format!("SET standard_conforming_strings = {setting}"))
            .await
            .expect("the setting");
        apply(&mut conn, &pg, &creation).await;
        assert_eq!(
            number(
                &mut conn,
                &format!("SELECT length(note)::int FROM {s}.payload WHERE code = 'a'")
            )
            .await,
            4,
            "the two characters survived under standard_conforming_strings = {setting}"
        );
        assert_eq!(
            number(
                &mut conn,
                &format!("SELECT length(blob)::int FROM {s}.payload WHERE code = 'a'")
            )
            .await,
            2,
            "the two bytes survived under standard_conforming_strings = {setting}"
        );
        conn.execute(&format!("DROP TABLE {s}.payload"))
            .await
            .expect("clear for the next setting");
    }
    conn.execute("SET standard_conforming_strings = on")
        .await
        .expect("put it back");
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// ADR-0013 §5: whether two declared keys are one row is the engine's
/// question, and this engine answers it from the key column's own collation —
/// which is not in the declarations, so offline `validate` says it did not ask
/// rather than reporting clean.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn two_keys_a_collation_calls_one_row_are_found_by_the_engine_and_not_offline() {
    let mut conn = connect().await;
    let s = data_schema("collation");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE COLLATION {s}.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE {s}.keys (code varchar(20) COLLATE {s}.ci PRIMARY KEY, label text);
         CREATE TABLE {s}.plain (code varchar(20) PRIMARY KEY, label text);"
    ))
    .await
    .expect("the fixture");

    let mut declared = Schema::default();
    for name in ["keys", "plain"] {
        let mut table = Table::default();
        table
            .columns
            .insert("code".into(), Column::new(ty("varchar(20)")).not_null());
        table
            .columns
            .insert("label".into(), Column::new(ty("text")));
        table.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["code".into()],
        });
        with_data(
            &mut table,
            DataMode::Exact,
            &[
                ("New", row(&[("label", Value::Text("a".into()))])),
                ("new", row(&[("label", Value::Text("b".into()))])),
            ],
        );
        declared.tables.insert(TableName::new(&s, name), table);
    }

    // Offline: nothing is refused, and a note says why that is not a clean
    // bill of health.
    let pg = Postgres::new();
    for (name, table) in &declared.tables {
        assert!(
            pg.validate_table(name, table).is_empty(),
            "offline validation cannot judge the keys, so it must not try"
        );
    }
    let notes = pg.declaration_notes(&declared);
    assert_eq!(notes.len(), 2, "{notes:#?}");
    assert!(notes.iter().all(|n| n.contains("collation")), "{notes:#?}");

    // Connected: the engine is asked, under each key column's own collation,
    // and answers differently for the two tables.
    let mut at = pbps_pg::rows::CatalogNames::new();
    pbps_pg::catalog::key_collations(&mut conn, &mut at, &declared)
        .await
        .expect("read the key collations");
    assert_eq!(
        at[&TableName::new(&s, "keys")].key_collation,
        Some((s.clone(), "ci".to_owned()))
    );
    let found = pbps_pg::catalog::misspelt(&mut conn, &declared, &at)
        .await
        .expect("ask the engine");
    assert_eq!(found.conflicts.len(), 1, "{found:#?}");
    assert!(
        found.conflicts[0].contains(&format!("{s}.keys")),
        "{found:#?}"
    );
    assert!(found.conflicts[0].contains("`New`"), "{found:#?}");
    assert!(found.conflicts[0].contains("`new`"), "{found:#?}");
    // And the second insert is what that refusal is about.
    conn.execute(&format!("INSERT INTO {s}.keys VALUES ('New', 'a')"))
        .await
        .expect("the first key");
    let collision = conn
        .execute(&format!("INSERT INTO {s}.keys VALUES ('new', 'b')"))
        .await
        .expect_err("the collation makes them one row");
    assert_eq!(sqlstate(&collision), "23505", "{collision:?}");
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// DECISIONS 101 on this engine: a value the engine would not read back as
/// written is refused before anything is written, with the spelling to write.
///
/// Without this the declaration disagrees with its own database on every plan:
/// `1.5` into a `numeric(5,2)` is stored and read back as `1.50`, and the
/// update that changes nothing is proposed for ever.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_value_the_engine_spells_differently_is_refused_before_it_is_written() {
    let mut conn = connect().await;
    let s = data_schema("spelling");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "amounts");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("varchar(4)")).not_null());
    table
        .columns
        .insert("pct".into(), Column::new(ty("numeric(5,2)")));
    table
        .columns
        .insert("when_".into(), Column::new(ty("date")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[
            (
                "a",
                row(&[
                    ("pct", Value::Text("1.5".into())),
                    ("when_", Value::Text("2026-01-02".into())),
                ]),
            ),
            // A key the column cannot hold at all: the engine reads it as
            // nothing, which is a different answer from "reads it differently".
            ("toolong", row(&[])),
        ],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);

    // And a table whose column query holds exactly **one** literal, which the
    // type cannot read. That is the case the optimization fence is for.
    // **Measured**: with two rows in the `VALUES` the `CASE` protects the cast
    // and the answer is NULL; with one, the planner folds the list into a
    // `Result` node and evaluates the cast while planning —
    //
    // ```text
    // one row, no fence, numeric:  ERROR: invalid input syntax for type numeric: "oops"
    // one row, fenced:             NULL, which is the finding this query exists to make
    // ```
    //
    // The type decides whether it folds at all: `numeric`, `integer` and
    // `uuid` read their text through an immutable input function and fold,
    // while `date` and `character varying` do not — so a fence tested on a
    // `date` would have proved nothing.
    let alone = TableName::new(&s, "alone");
    let mut single = Table::default();
    single
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    single
        .columns
        .insert("pct".into(), Column::new(ty("numeric(5,2)")));
    single.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut single,
        DataMode::Exact,
        &[("a", row(&[("pct", Value::Text("oops".into()))]))],
    );
    declared.tables.insert(alone.clone(), single);

    let found =
        pbps_pg::catalog::misspelt(&mut conn, &declared, &pbps_pg::rows::CatalogNames::new())
            .await
            .expect("ask the engine");
    let misspelt: Vec<String> = found
        .misspelt
        .iter()
        .filter(|m| m.table == name)
        .map(|m| format!("{:?} {:?} -> {:?}", m.column, m.declared, m.canonical))
        .collect();
    assert_eq!(misspelt.len(), 2, "{misspelt:#?}");
    // The single bad cell is reported, not raised.
    let single: Vec<&pbps_pg::rows::Misspelt> =
        found.misspelt.iter().filter(|m| m.table == alone).collect();
    assert_eq!(single.len(), 1, "{single:#?}");
    assert_eq!(single[0].declared, "oops");
    assert_eq!(single[0].canonical, None);
    // The value the engine respells, with the spelling to write…
    assert!(
        misspelt.iter().any(|m| m.contains("\"pct\"")
            && m.contains("\"1.5\"")
            && m.contains("Some(\"1.50\")")),
        "{misspelt:#?}"
    );
    // …and the key it cannot read at all, which is not the same finding.
    assert!(
        misspelt
            .iter()
            .any(|m| m.contains("None") && m.contains("\"toolong\"") && m.contains("-> None")),
        "{misspelt:#?}"
    );
    // The unambiguous date is not a finding: refusing it would refuse a valid
    // declaration.
    assert!(
        !misspelt.iter().any(|m| m.contains("when_")),
        "{misspelt:#?}"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A trigger that rewrites the row inside the write's own statement is caught
/// and the statement rolls back — rather than the apply reading the rewrite
/// back and recording it as the plan's own result (DECISIONS 132).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_trigger_that_undoes_a_row_write_rolls_the_statement_back() {
    let mut conn = connect().await;
    let s = data_schema("trigger");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("a", row(&[("label", Value::Text("One".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    let creation = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);

    // The table first, then the trigger, then the row: the trigger has to be
    // there before the write it rewrites.
    let mut statements = Vec::new();
    for p in &creation.changes {
        statements.push((
            matches!(p.change, pbps_model::Change::InsertRow { .. }),
            pg.emit(&p.change, p.strategy).expect("emit"),
        ));
    }
    for (is_row, stmts) in &statements {
        if *is_row {
            continue;
        }
        for st in stmts {
            conn.execute(&st.sql).await.expect("the structure");
        }
    }
    conn.execute(&format!(
        "CREATE FUNCTION {s}.rewrite() RETURNS trigger LANGUAGE plpgsql AS $body$
           BEGIN UPDATE {s}.status SET label = 'Rewritten' WHERE code = NEW.code; RETURN NULL; END
         $body$;
         CREATE TRIGGER rewrite AFTER INSERT ON {s}.status
           FOR EACH ROW EXECUTE FUNCTION {s}.rewrite();"
    ))
    .await
    .expect("the trigger");

    for (is_row, stmts) in &statements {
        if !*is_row {
            continue;
        }
        for st in stmts {
            let refusal = conn
                .execute(&st.sql)
                .await
                .expect_err("the trigger rewrote the row the plan wrote");
            assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
        }
    }
    // Nothing was left behind: the check and the write are one statement.
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.status")).await,
        0,
        "the refused insert wrote nothing"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The probe reports what is true before the plan runs; a child row that
/// arrives afterwards is caught by the delete's own guard, inside the
/// statement, rather than being cascaded away unseen (DECISIONS 129).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_child_row_that_arrives_after_the_probe_is_not_cascaded_away() {
    let mut conn = connect().await;
    let s = data_schema("arrival");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, parent text
             REFERENCES {s}.parent(code) ON DELETE CASCADE);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');"
    ))
    .await
    .expect("the fixture");

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    // Nothing references the row when the human approves the plan.
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 0);

    // Another session inserts a child between the probe and the apply. With
    // `ON DELETE CASCADE` the engine would take it away silently.
    let mut other = connect().await;
    other
        .execute(&format!("INSERT INTO {s}.child VALUES (1, 'old')"))
        .await
        .expect("the arrival");

    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the guard counts what arrived after the probe");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.child")).await,
        1,
        "the child was not cascaded away"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A cell left to a *typed* default, and one the engine deparses without a
/// cast: both are compared against the column as the column stores them.
///
/// The declared expression and the stored value are compared as text, and only
/// one of them went through the column's type on the way in. **Measured**,
/// `numeric(5,2) DEFAULT 1` stores `1.00` and deparses as `1`, so the insert's
/// own postcondition rejected the row the engine had just written correctly —
/// a valid plan refused, by the guard that exists to catch a trigger.
///
/// The boolean is the other half: this engine deparses `DEFAULT true` as the
/// bare word, which a literal test written for strings and numbers calls an
/// expression. The cell is then read back as one nobody can tell from its
/// default, and a hand edit that flips it is projected as an omission and
/// never planned away.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_cell_left_to_a_default_is_compared_as_the_column_stores_it() {
    let mut conn = connect().await;
    let s = data_schema("defaults");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut rank = Column::new(ty("numeric(5,2)"));
    rank.default = Some("1".into());
    table.columns.insert("rank".into(), rank);
    let mut flag = Column::new(ty("boolean"));
    flag.default = Some("true".into());
    table.columns.insert("flag".into(), flag);
    // And the canonical empty `bytea`, which `decode('', 'hex')` writes and
    // this engine reads back as `\x`.
    table
        .columns
        .insert("blob".into(), Column::new(ty("bytea")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("a", row(&[("blob", Value::Text("\\x".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    // The insert's own postcondition holds the two defaulted cells to their
    // defaults, so this apply is the assertion.
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let seen = observed(&mut conn, &declared, &s, &name).await;
    let a = &seen.rows[&RowKey::from("a")];
    assert!(a.at_default.contains("rank"), "{a:#?}");
    assert!(a.at_default.contains("flag"), "{a:#?}");
    assert_eq!(a.cells.get("blob"), Some(&Value::Text("\\x".into())));
    let base = connected_base(&mut conn, &declared, &s).await;
    assert!(
        plan(&base, &ids, &declared, &ids).is_empty(),
        "the plan after the apply"
    );

    // A hand edit to the boolean is drift, not an omission — which it is only
    // because the engine was asked about that default at all.
    conn.execute(&format!("UPDATE {s}.status SET flag = false"))
        .await
        .expect("the hand edit");
    let after = connected_base(&mut conn, &declared, &s).await;
    let fix = plan(&after, &ids, &declared, &ids);
    assert_eq!(fix.changes.len(), 1, "{fix:#?}");
    assert!(
        matches!(fix.changes[0].change, pbps_model::Change::UpdateRow { .. }),
        "{fix:#?}"
    );
    apply(&mut conn, &pg, &fix).await;
    assert!(
        truth(&mut conn, &format!("SELECT flag FROM {s}.status")).await,
        "the update put the default back"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A plan that unpicks a child's reference and then deletes the parent is the
/// ordinary shape, and an explicit NULL is what unpicks it.
///
/// The update runs before the delete, so by the time the delete runs the child
/// points at nothing. A probe that could not compare a NULL generated no
/// exclusion for that row at all, counted the child's *stored* reference, and
/// refused the plan.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_child_this_plan_sets_to_null_is_not_counted_against_its_parents_delete() {
    let mut conn = connect().await;
    let s = data_schema("tonull");
    fresh(&mut conn, &s).await;

    let parent = TableName::new(&s, "parent");
    let child = TableName::new(&s, "child");
    let mut p = Table::default();
    p.columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    p.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    let mut c = Table::default();
    c.columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    c.columns.insert("parent".into(), Column::new(ty("text")));
    c.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    c.foreign_keys.insert(
        "fk_child".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );

    // Before: the parent has both rows and the child points at the one that
    // is about to go.
    let mut before = Schema::default();
    let mut p0 = p.clone();
    with_data(
        &mut p0,
        DataMode::Exact,
        &[("old", row(&[])), ("keep", row(&[]))],
    );
    let mut c0 = c.clone();
    with_data(
        &mut c0,
        DataMode::Exact,
        &[("c1", row(&[("parent", Value::Text("old".into()))]))],
    );
    before.tables.insert(parent.clone(), p0);
    before.tables.insert(child.clone(), c0);

    // After: the child references nothing and `old` is gone.
    let mut after = Schema::default();
    let mut p1 = p.clone();
    with_data(&mut p1, DataMode::Exact, &[("keep", row(&[]))]);
    let mut c1 = c.clone();
    with_data(
        &mut c1,
        DataMode::Exact,
        &[("c1", row(&[("parent", Value::Null)]))],
    );
    after.tables.insert(parent.clone(), p1);
    after.tables.insert(child.clone(), c1);

    let ids0 = mint_ids(&before, &IdsFile::default(), &[]);
    let ids1 = mint_ids(&after, &ids0, &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &before, &ids0),
    )
    .await;

    let base = connected_base(&mut conn, &before, &s).await;
    let cs = plan(&base, &ids0, &after, &ids1);
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "the row this plan sets to NULL is not counted: {}",
        probes[0].sql
    );
    assert_eq!(counted(&mut conn, &probes[1].sql).await, 0);
    // And the plan runs, in the order that makes it true.
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(
            &mut conn,
            &format!("SELECT count(*)::int FROM {s}.parent WHERE code = 'old'")
        )
        .await,
        0
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The same connection string, as another role.
///
/// A `password=` is appended rather than substituted: libpq takes the last
/// value of a repeated keyword, so this works whether or not the original
/// carries one.
fn as_role(role: &str, password: &str) -> String {
    format!("{} user={role} password={password}", conn_str())
}

/// A child table whose rows the deploying session cannot see is not a child
/// table with no rows, and the delete refuses rather than cascading into it.
///
/// **Measured**: referential actions bypass row-level security and a
/// `SELECT count(*)` does not, so the guard counted zero and the
/// `ON DELETE CASCADE` took the hidden row anyway — a silent loss by the
/// statement written to prevent one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_referencing_row_the_session_cannot_see_refuses_the_delete() {
    let mut conn = connect().await;
    let s = data_schema("rls");
    fresh(&mut conn, &s).await;
    let role = format!("{s}_dep");
    // A run that failed part-way leaves the role behind — it does not live in
    // the schema `fresh` just dropped — and `CREATE ROLE` would then fail with
    // something far less informative than this test's own assertions.
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             parent text REFERENCES {s}.parent(code) ON DELETE CASCADE, owner text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old', 'somebody else');
         ALTER TABLE {s}.child ENABLE ROW LEVEL SECURITY;
         CREATE POLICY only_mine ON {s}.child USING (owner = current_user);
         GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.parent, {s}.child TO {role};"
    ))
    .await
    .expect("the fixture");

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);

    // As the deploying role, the count is filtered to nothing…
    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    assert_eq!(
        number(
            &mut deployer,
            &format!("SELECT count(*)::int FROM {s}.child")
        )
        .await,
        0,
        "the policy hides the child from this session"
    );
    assert!(
        truth(
            &mut deployer,
            &format!("SELECT pg_catalog.row_security_active('{s}.child'::regclass)")
        )
        .await
    );

    // …so preflight says so before the first statement of the plan runs,
    // which is the only place a refusal can still stop a staged apply from
    // committing everything ahead of the delete.
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    assert_eq!(
        counted(&mut deployer, &probes[0].sql).await,
        0,
        "the count this session can take is not the count: {}",
        probes[0].sql
    );
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        1,
        "one referencing table this session cannot see in full: {}",
        probes[1].sql
    );
    assert!(
        probes[1].description.contains("cannot count"),
        "{probes:#?}"
    );
    // And the negative half, from a session the policy does not filter: the
    // same table, the same switch, and nothing refused — the probe asks about
    // the session, not about the table.
    assert!(
        truth(
            &mut conn,
            &format!("SELECT relrowsecurity FROM pg_class WHERE oid = '{s}.child'::regclass")
        )
        .await,
        "the switch is on for both sessions"
    );
    assert_eq!(
        counted(&mut conn, &probes[1].sql).await,
        0,
        "the owner sees every row, so its count is complete: {}",
        probes[1].sql
    );

    // …and the guard refuses too, for a plan that got past preflight anyway.
    let refusal = deployer
        .execute(&stmt.sql)
        .await
        .expect_err("a filtered count is not a complete one");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.child")).await,
        1,
        "the hidden child was not cascaded away"
    );

    // And as a session the policy does not apply to, the same statement runs:
    // that count *is* complete, and this is what makes the refusal a
    // statement about the session rather than about the table.
    assert!(
        !truth(
            &mut conn,
            &format!("SELECT pg_catalog.row_security_active('{s}.child'::regclass)")
        )
        .await
    );
    conn.execute(&stmt.sql)
        .await
        .expect_err("the child still references it, so the ordinary count refuses");

    drop(deployer);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    // `DROP OWNED BY`, not `REVOKE ... ON SCHEMA {s}`: the schema is already
    // gone, and naming it here fails with 3F000 *after* every assertion has
    // passed — a teardown that reports the test as broken.
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A cell the column's own collation calls equal to its default is still a
/// cell that is not at its default.
///
/// The read-back asks the engine whether the stored value equals the declared
/// default. **Measured**, a `text` column under `und-u-ks-level2` answers yes
/// for a stored `New` against `DEFAULT 'new'` — so the cell was marked at its
/// default, `ObservedRow::as_seen_by` left it out of the read-back entirely,
/// and the drift was invisible to every connected plan and to `verify`. The
/// write path already compared as text under `COLLATE "C"`; this call site was
/// not swept with it.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_cell_a_collation_calls_equal_to_its_default_is_still_drift() {
    let mut conn = connect().await;
    let s = data_schema("cidefault");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE COLLATION {s}.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE {s}.status (code text PRIMARY KEY, label text COLLATE {s}.ci DEFAULT 'new');
         INSERT INTO {s}.status (code, label) VALUES ('a', 'New');"
    ))
    .await
    .expect("the fixture");
    // The engine's own answer, which is the one the read-back used to take.
    assert!(
        truth(
            &mut conn,
            &format!("SELECT label = ('new') FROM {s}.status WHERE code = 'a'")
        )
        .await,
        "this column's collation calls the two spellings equal"
    );

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut label = Column::new(ty("text"));
    // The engine's own rendering of `DEFAULT 'new'`, which is what `pull`
    // writes and what a declaration therefore holds; declaring the bare
    // literal would plan an `AlterColumnDefault` beside the row change and
    // say nothing about the row.
    label.default = Some("'new'::text".into());
    table.columns.insert("label".into(), label);
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    // The declaration omits `label`, which is what "leave it at its default"
    // is spelled as. The stored `New` is therefore drift.
    with_data(&mut table, DataMode::Exact, &[("a", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let seen = observed(&mut conn, &declared, &s, &name).await;
    let a = &seen.rows[&RowKey::from("a")];
    assert!(
        !a.at_default.contains("label"),
        "a different spelling is not the default: {a:#?}"
    );
    assert_eq!(a.cells.get("label"), Some(&Value::Text("New".into())));

    // And the drift is therefore something a plan can settle.
    let base = connected_base(&mut conn, &declared, &s).await;
    let fix = plan(&base, &ids, &declared, &ids);
    assert_eq!(fix.changes.len(), 1, "{fix:#?}");
    assert!(
        matches!(fix.changes[0].change, pbps_model::Change::UpdateRow { .. }),
        "{fix:#?}"
    );
    let pg = Postgres::new();
    apply(&mut conn, &pg, &fix).await;
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT label FROM {s}.status WHERE code = 'a'")
        )
        .await,
        "new",
        "the update put the default back, in the default's own spelling"
    );
    let after = connected_base(&mut conn, &declared, &s).await;
    assert!(
        plan(&after, &ids, &declared, &ids).is_empty(),
        "the plan after the apply"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A row that references itself is not a child that survives its own delete.
///
/// The guard counts the rows still referencing the parent, and it runs before
/// the `DELETE` in the same block — so a self-referencing row still points at
/// itself when it is counted. **Measured**, this engine takes that delete
/// without complaint, because the one statement removes both sides of the
/// reference; the guard refused a plan the engine accepts.
///
/// The negative half is the same table: another row pointing at the doomed one
/// through the same self-reference is a real child, and is still counted.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_row_that_references_only_itself_can_be_deleted() {
    let mut conn = connect().await;
    let s = data_schema("selfref");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.node (code text PRIMARY KEY, parent text REFERENCES {s}.node(code));
         INSERT INTO {s}.node VALUES ('keep', NULL), ('loop', 'loop');"
    ))
    .await
    .expect("the fixture");

    let name = TableName::new(&s, "node");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("parent".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    table.foreign_keys.insert(
        "node_parent_fkey".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(&mut table, DataMode::Exact, &[("keep", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert_eq!(cs.changes.len(), 1, "{cs:#?}");
    assert!(
        matches!(cs.changes[0].change, pbps_model::Change::DeleteRow { .. }),
        "{cs:#?}"
    );

    let pg = Postgres::new();
    // The probe already leaves the doomed row out on its own table, so the
    // plan is offered at all…
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 0);
    assert_eq!(counted(&mut conn, &probes[1].sql).await, 0);
    // …and the statement's own guard has to agree, or a plan the probe passed
    // dies at apply time.
    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);
    conn.execute(&stmt.sql)
        .await
        .expect("the engine removes both sides of the reference at once");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.node")).await,
        1
    );

    // The negative case: a *different* row pointing at the doomed one is a
    // child, and neither the probe nor the guard may lose it.
    conn.execute(&format!(
        "INSERT INTO {s}.node VALUES ('loop', 'loop'), ('leaf', 'loop')"
    ))
    .await
    .expect("the second fixture");
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let doomed = cs
        .changes
        .iter()
        .find(|c| {
            matches!(&c.change, pbps_model::Change::DeleteRow { key, .. } if key.as_str() == "loop")
        })
        .expect("the plan deletes `loop`");
    let stmt = pg
        .emit(&doomed.change, doomed.strategy)
        .expect("emit")
        .remove(0);
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("`leaf` still references `loop`");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.node")).await,
        3
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// DECISIONS 124 on this engine: a write left to a default the probe cannot
/// evaluate, on a column a live foreign key into the deleted row's table
/// spans, is refused before the first statement.
///
/// Which key of the parent such a default names is decided when it runs. Read
/// as absent, the write passes preflight, commits — for good, in a staged
/// apply — and the delete's own guard then finds the reference and aborts,
/// leaving the deployment half applied. The remedy costs one spelled value.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_write_to_a_default_no_probe_can_evaluate_is_refused_where_a_key_spans_it() {
    let mut conn = connect().await;
    let s = data_schema("unprobeable");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             parent text DEFAULT lower('OLD') REFERENCES {s}.parent(code));
         CREATE TABLE {s}.loose (id integer PRIMARY KEY, parent text DEFAULT lower('OLD'));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');"
    ))
    .await
    .expect("the fixture");
    // The default really does name the doomed row, which is what makes the
    // refusal about something rather than about a shape.
    assert_eq!(text(&mut conn, "SELECT lower('OLD')").await, "old");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );

    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    for (name, keyed) in [("child", true), ("loose", false)] {
        let mut t = Table::default();
        t.columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        let mut fk_column = Column::new(ty("text"));
        fk_column.default = Some("lower('OLD'::text)".into());
        t.columns.insert("parent".into(), fk_column);
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        if keyed {
            t.foreign_keys.insert(
                format!("{name}_parent_fkey"),
                ForeignKey {
                    columns: vec!["parent".into()],
                    references_table: parent_name.clone(),
                    references_columns: vec!["code".into()],
                    on_delete: ReferentialAction::NoAction,
                    on_update: ReferentialAction::NoAction,
                },
            );
        }
        // The row omits `parent`, which is what leaves it to the default.
        with_data(&mut t, DataMode::Exact, &[("1", row(&[]))]);
        declared.tables.insert(TableName::new(&s, name), t);
    }
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert!(
        cs.changes.iter().any(|c| matches!(
            &c.change,
            pbps_model::Change::DeleteRow { key, .. } if key.as_str() == "old"
        )),
        "{cs:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    // The count, the refusal about whether it could be complete, then one
    // refusal per table this plan writes to such a default: the keyed child
    // and the loose one.
    assert_eq!(probes.len(), 4, "{probes:#?}");
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "nothing stored references the doomed row yet: {}",
        probes[0].sql
    );

    let refusal = probes
        .iter()
        .find(|p| p.description.contains(&format!("{s}.child")))
        .expect("a refusal for the keyed child");
    assert!(
        refusal.description.contains("parent (row `1`)"),
        "{refusal:#?}"
    );
    assert!(
        refusal.description.contains("spell the value"),
        "{refusal:#?}"
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "a live foreign key spans the column: {}",
        refusal.sql
    );

    // The negative half, and the reason this is a count over the catalog
    // rather than a rule about defaults: the same unevaluable default on a
    // column no foreign key spans refuses nothing.
    let loose = probes
        .iter()
        .find(|p| p.description.contains(&format!("{s}.loose")))
        .expect("a refusal for the loose table");
    assert_eq!(
        counted(&mut conn, &loose.sql).await,
        0,
        "no key into the parent spans it: {}",
        loose.sql
    );

    // And the hazard the refusal is about: run the plan without heeding it and
    // the insert commits, the delete then finds the reference, and a staged
    // apply is left half done.
    conn.execute(&format!("INSERT INTO {s}.child (id) VALUES (1)"))
        .await
        .expect("the write the probe could not evaluate");
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT parent FROM {s}.child WHERE id = 1")
        )
        .await,
        "old",
        "the default did name the doomed row"
    );
    let delete = cs
        .changes
        .iter()
        .find(|c| {
            matches!(&c.change, pbps_model::Change::DeleteRow { key, .. } if key.as_str() == "old")
        })
        .expect("the delete");
    let stmt = pg
        .emit(&delete.change, delete.strategy)
        .expect("emit")
        .remove(0);
    let aborted = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the guard finds what the probe could not");
    assert_eq!(sqlstate(&aborted), "P0001", "{aborted:?}");

    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A `json` cell left to its default is compared like any other, because the
/// comparison is made as text and needs no `json = json`.
///
/// **Measured**, this engine has no such operator — it is the one type in this
/// dialect's catalogue without one — and the guard that skipped `json`
/// outlived the native comparison it was written for. A skipped cell is read
/// back as one nobody can tell from its default, so a hand-edited document was
/// dropped from the read-back and no plan ever proposed to put it back.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_json_cell_at_its_default_is_told_from_a_hand_edited_one() {
    let mut conn = connect().await;
    let s = data_schema("jsondefault");
    fresh(&mut conn, &s).await;

    let name = TableName::new(&s, "doc");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut body = Column::new(ty("json"));
    body.default = Some("'{\"a\": 1}'::json".into());
    table.columns.insert("body".into(), body);
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(&mut table, DataMode::Exact, &[("a", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let pg = Postgres::new();
    apply(
        &mut conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    // The type has no `=` at all, and the read-back does not need one.
    let no_operator = match conn
        .query(&format!("SELECT body = body FROM {s}.doc"))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("json has no equality operator"),
    };
    assert_eq!(sqlstate(&no_operator), "42883", "{no_operator:?}");
    let seen = observed(&mut conn, &declared, &s, &name).await;
    let a = &seen.rows[&RowKey::from("a")];
    assert!(a.at_default.contains("body"), "{a:#?}");
    assert!(!a.unknown.contains("body"), "{a:#?}");

    // And a hand edit is drift, not an omission.
    conn.execute(&format!("UPDATE {s}.doc SET body = '{{\"a\": 2}}'"))
        .await
        .expect("the hand edit");
    let after = connected_base(&mut conn, &declared, &s).await;
    let fix = plan(&after, &ids, &declared, &ids);
    assert_eq!(fix.changes.len(), 1, "{fix:#?}");
    assert!(
        matches!(fix.changes[0].change, pbps_model::Change::UpdateRow { .. }),
        "{fix:#?}"
    );
    apply(&mut conn, &pg, &fix).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT body::text FROM {s}.doc")).await,
        "{\"a\": 1}",
        "the update put the default back"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The key a write puts there is held to its exact spelling, like every other
/// cell the plan spells.
///
/// **Measured**, a trigger that lowercases a newly inserted key leaves the row
/// under a spelling nobody declared, and a postcondition using the key
/// column's own `=` — case-insensitive here — calls that a success. The alias
/// read then maps the declaration's `New` onto the stored `new`, so `verify`
/// agrees with it for ever after and no plan proposes to put the spelling
/// back.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_trigger_that_respells_the_written_key_rolls_the_statement_back() {
    let mut conn = connect().await;
    let s = data_schema("keycase");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE COLLATION {s}.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE {s}.status (code text COLLATE {s}.ci PRIMARY KEY, label text);
         CREATE FUNCTION {s}.lower_it() RETURNS trigger LANGUAGE plpgsql AS $fn$
         BEGIN UPDATE {s}.status SET code = lower(NEW.code) WHERE code = NEW.code; RETURN NULL; END
         $fn$;"
    ))
    .await
    .expect("the fixture");

    let name = TableName::new(&s, "status");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("New", row(&[("label", Value::Text("a".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert_eq!(cs.changes.len(), 1, "{cs:#?}");
    let pg = Postgres::new();
    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);

    // Without the trigger the same statement runs and the key is what was
    // declared — so the refusal below is about the trigger, not about the
    // collation.
    conn.execute(&stmt.sql).await.expect("the ordinary insert");
    assert_eq!(
        text(&mut conn, &format!("SELECT code FROM {s}.status")).await,
        "New"
    );
    conn.execute(&format!("DELETE FROM {s}.status")).await.ok();

    conn.execute(&format!(
        "CREATE TRIGGER lower_it AFTER INSERT ON {s}.status
             FOR EACH ROW EXECUTE FUNCTION {s}.lower_it();"
    ))
    .await
    .expect("arm the trigger");
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the trigger respelled the key this plan wrote");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    // The engine calls the two spellings one key, which is exactly why the
    // column's own `=` could not tell that anything had happened.
    assert!(
        truth(
            &mut conn,
            &format!("SELECT E'New' = E'new' COLLATE \"{s}\".\"ci\"")
        )
        .await
    );
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.status")).await,
        0,
        "the whole statement rolled back"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A relation is scanned the way its foreign key covers it: `ONLY` for an
/// ordinary table, whose key does not reach its inheritance children, and in
/// full for a partitioned one, whose key does.
///
/// **Measured on 18.6**, both halves. A foreign key is not inherited, so a row
/// in an inheritance child holding the deleted key is not constrained and the
/// parent deletes — while an unqualified `FROM schema.table` scans that child
/// and counted it, refusing a delete the engine performs. And a partitioned
/// referencing table carries the key *twice* in the catalog, once on the
/// partitioned table and once on each partition, so counting both scanned the
/// same row twice.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_referencing_relation_is_counted_only_where_its_key_reaches() {
    let mut conn = connect().await;
    let s = data_schema("inherit");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.ref (id integer PRIMARY KEY, parent text REFERENCES {s}.parent(code));
         CREATE TABLE {s}.ref_kid (extra text) INHERITS ({s}.ref);
         CREATE TABLE {s}.part (id integer, parent text REFERENCES {s}.parent(code))
             PARTITION BY RANGE (id);
         CREATE TABLE {s}.part_1 PARTITION OF {s}.part FOR VALUES FROM (0) TO (100);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.ref_kid VALUES (1, 'old', 'x');"
    ))
    .await
    .expect("the fixture");
    // The catalog holds one key for the inheritance pair and two for the
    // partitioned one, which is what the count has to be told apart.
    assert_eq!(
        number(
            &mut conn,
            &format!(
                "SELECT count(*)::int FROM pg_constraint \
                 WHERE contype = 'f' AND confrelid = '{s}.parent'::regclass"
            )
        )
        .await,
        3
    );

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    // The row lives in the inheritance child, which no key covers.
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "an unconstrained descendant row is not a child: {}",
        probes[0].sql
    );
    // And the engine agrees, which is what makes the count right rather than
    // merely smaller.
    let stmt = pg
        .emit(&cs.changes[0].change, cs.changes[0].strategy)
        .expect("emit")
        .remove(0);
    conn.execute(&stmt.sql).await.expect("the delete");
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.ref")).await,
        1,
        "the descendant row is still there, orphaned and unconstrained"
    );

    // The negative half, on the same schema: a row a key *does* cover is
    // counted, once, through the partitioned table and not again through its
    // partition.
    conn.execute(&format!(
        "INSERT INTO {s}.part VALUES (1, 'keep');
         DELETE FROM {s}.ref_kid;"
    ))
    .await
    .expect("the second fixture");
    // The same declaration with no rows at all, so `keep` is undeclared and
    // the plan deletes it.
    with_data(
        declared.tables.get_mut(&name).expect("the table"),
        DataMode::Exact,
        &[],
    );
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let doomed = cs
        .changes
        .iter()
        .find(|c| {
            matches!(&c.change, pbps_model::Change::DeleteRow { key, .. } if key.as_str() == "keep")
        })
        .expect("the plan deletes `keep`");
    let probes = pg.preflight(&cs);
    let count = probes
        .iter()
        .find(|p| p.description.contains("row `keep`"))
        .expect("the count for `keep`");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        1,
        "the partitioned row is counted exactly once: {}",
        count.sql
    );
    let stmt = pg
        .emit(&doomed.change, doomed.strategy)
        .expect("emit")
        .remove(0);
    let refusal = conn
        .execute(&stmt.sql)
        .await
        .expect_err("the partitioned child still references it");
    assert_eq!(sqlstate(&refusal), "P0001", "{refusal:?}");
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A key is a tuple, and so is the question the unprobeable-default refusal
/// asks: a default nobody can evaluate says nothing about a tuple that already
/// holds a NULL.
///
/// **Measured on 18.6**, `MATCH SIMPLE` is this engine's default, so a
/// composite foreign key with a NULL in any of its columns is not checked at
/// all — the child is accepted against a parent row that does not exist, and
/// the parent deletes with that child sitting there. Refusing on the
/// unprobeable column alone therefore refused a plan the engine accepts
/// however the default evaluates.
///
/// The foreign key references a *unique* key and not the primary key, because
/// it has to: rows are keyed by one column (ADR-0004), so a table with a
/// composite primary key can carry no `data:` block and can never be the
/// parent of a `DeleteRow`. DECISIONS 116's shape is the only way a composite
/// foreign key and declared rows meet.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_unprobeable_default_beside_a_null_in_the_same_key_refuses_nothing() {
    let mut conn = connect().await;
    let s = data_schema("tuplenull");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
             CONSTRAINT parent_xy UNIQUE (x, y));
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             a text DEFAULT lower('OX'), b text,
             CONSTRAINT child_ab FOREIGN KEY (a, b) REFERENCES {s}.parent(x, y));
         INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');"
    ))
    .await
    .expect("the fixture");
    // The engine's own rule, which is what the refusal has to agree with.
    conn.execute(&format!(
        "INSERT INTO {s}.child (id, a, b) VALUES (99, 'nosuch', NULL)"
    ))
    .await
    .expect("MATCH SIMPLE does not check a tuple holding a NULL");
    conn.execute(&format!("DELETE FROM {s}.child WHERE id = 99"))
        .await
        .expect("clear it");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    // No rows declared, so both stored rows are undeclared and the plan
    // deletes them — which is what puts a `DeleteRow` in front of the refusal.
    with_data(&mut parent, DataMode::Exact, &[]);

    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('OX'::text)".into());
    child.columns.insert("a".into(), a);
    child.columns.insert("b".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // `a` is left to the unevaluable default, which names the doomed row'"'"'s
    // `x`; `b` is spelled NULL. The tuple cannot reference anything.
    with_data(
        &mut child,
        DataMode::Exact,
        &[("1", row(&[("b", Value::Null)]))],
    );

    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the refusal is still offered, and answers nothing");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        0,
        "the same key holds a NULL this plan writes: {}",
        refusal.sql
    );

    // And the whole plan runs — which is the assertion, since the refusal
    // would have stopped it before its first statement.
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        0,
        "both parent rows are undeclared and went"
    );
    assert_eq!(
        text(&mut conn, &format!("SELECT a FROM {s}.child WHERE id = 1")).await,
        "ox",
        "the default did name a row the plan deleted, and it did not matter"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A referencing table this session cannot read is not a referencing table
/// with no rows.
///
/// **Measured**, a role with `DELETE` on the parent and no `SELECT` on the
/// child gets `permission denied` from the count — which the probe runner
/// reports as *unchecked*, and `apply` proceeds — while the engine's own key
/// still sees the child and refuses the delete. So the probe asks the catalog
/// first, and refuses on the answer rather than on the error.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_referencing_table_the_session_cannot_read_refuses_the_delete() {
    let mut conn = connect().await;
    let s = data_schema("unreadable");
    fresh(&mut conn, &s).await;
    let role = format!("{s}_dep");
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, parent text REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');
         GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.parent TO {role};"
    ))
    .await
    .expect("the fixture");

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");

    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    // The count itself is an error to this session — which `apply` would read
    // as "unchecked" and walk past…
    let denied = match deployer.query(&probes[0].sql).await {
        Err(e) => e,
        Ok(_) => panic!("the role cannot read the child"),
    };
    assert_eq!(sqlstate(&denied), "42501", "{denied:?}");
    // …so the refusal beside it answers from the catalog instead.
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        1,
        "one referencing table this session cannot read: {}",
        probes[1].sql
    );
    // And the owner, who can read it, is refused by nothing here — the
    // ordinary count does that job for it.
    assert_eq!(counted(&mut conn, &probes[1].sql).await, 0);
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 1);

    drop(deployer);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A `DEFAULT` whose default is NULL is a NULL the probe compares, exactly as
/// a cell spelled `null:` is (DECISIONS 329).
///
/// The ordinary shape — unpick a child's reference, then delete the parent —
/// written by leaving the child's column out so it goes back to its default.
/// Mapping that default to "cannot compare" kept the child's stored reference
/// in the count and refused the plan, while the same plan spelled with an
/// explicit NULL was allowed.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_child_left_to_a_null_default_is_not_counted_against_its_parents_delete() {
    let mut conn = connect().await;
    let s = data_schema("nulldefault");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY,
             parent text DEFAULT NULL REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c1', 'old');"
    ))
    .await
    .expect("the fixture");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut fk = Column::new(ty("text"));
    fk.default = Some("NULL::text".into());
    child.columns.insert("parent".into(), fk);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_parent_fkey".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // `parent` is left out, which is "back to the default" — NULL.
    with_data(&mut child, DataMode::Exact, &[("c1", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(parent_name, parent);
    declared.tables.insert(child_name, child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert!(
        cs.changes.iter().any(|c| matches!(
            &c.change,
            pbps_model::Change::UpdateRow { columns, .. }
                if matches!(columns.get("parent"), Some((_, pbps_model::Cell::Default(_))))
        )),
        "the child goes back to its default: {cs:#?}"
    );
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "the row this plan sends back to a NULL default is not counted: {}",
        probes[0].sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1
    );
    assert!(
        truth(
            &mut conn,
            &format!("SELECT parent IS NULL FROM {s}.child WHERE code = 'c1'")
        )
        .await
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A foreign key this plan adds is a foreign key the probe counts through.
///
/// `DeleteRow` runs at rank 12 and `AddForeignKey` at 13, so the delete runs
/// against a catalog that does not hold the key and the `ALTER` that follows
/// validates every stored child row — **measured**, it fails on the one the
/// delete just orphaned, and in a staged apply the delete has committed by
/// then. The planned key is built as a synthetic `pg_constraint` row beside
/// the stored ones, so every rule written for a catalog row reaches it: the
/// negative half is the same plan with the child row undeclared, where the
/// exclusion for rows the plan deletes takes it out of the count exactly as it
/// would for a stored key.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_foreign_key_this_plan_adds_is_counted_before_the_delete_that_would_break_it() {
    let mut conn = connect().await;
    let s = data_schema("plannedkey");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, parent text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c1', 'old');"
    ))
    .await
    .expect("the fixture");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("parent".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    // The key is declared and not in the database: this plan adds it.
    child.foreign_keys.insert(
        "child_parent_fkey".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c1", row(&[("parent", Value::Text("old".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert_eq!(cs.changes.len(), 2, "{cs:#?}");
    assert!(
        matches!(cs.changes[0].change, pbps_model::Change::DeleteRow { .. }),
        "the delete runs first: {cs:#?}"
    );
    assert!(
        matches!(
            cs.changes[1].change,
            pbps_model::Change::AddForeignKey { .. }
        ),
        "and the key is added after it: {cs:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    // The catalog's count sees no key — it is not there yet — and the
    // planned key is asked about from the plan (DECISIONS 345).
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 0);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        1,
        "the child is counted through the key this plan adds: {}",
        planned.sql
    );
    // The hazard, by hand and in the plan's own order: the delete goes
    // through, and the key cannot then be added.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!("DELETE FROM {s}.parent WHERE code = 'old'"))
        .await
        .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_parent_fkey \
             FOREIGN KEY (parent) REFERENCES {s}.parent(code)"
        ))
        .await
        .expect_err("the orphaned child fails the key's validation");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    // The negative half: the same plan with the child row undeclared, so the
    // plan deletes it first — and the exclusion for rows this plan deletes
    // reaches the planned key exactly as it reaches a stored one.
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[],
    );
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let count = probes
        .iter()
        .find(|p| {
            p.description.contains("row `old`")
                && p.description.contains("on a column it also adds")
        })
        .expect("the planned key's count for `old`");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        0,
        "a child this plan deletes is not counted through the planned key either: {}",
        count.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(
            &mut conn,
            &format!(
                "SELECT count(*)::int FROM pg_constraint \
                 WHERE conname = 'child_parent_fkey' AND connamespace = '{s}'::regnamespace"
            )
        )
        .await,
        1,
        "the key was added once the delete had nothing left to orphan"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A foreign key this plan adds on a column it also adds is counted through
/// the value that column is added with.
///
/// The synthetic constraint row that stands in for a planned key is built
/// from `pg_attribute`, and a column the database does not have has no row
/// there — so the key vanished, and the probe counted nothing. But `ADD
/// COLUMN … DEFAULT 'old'` backfills every stored row — **measured** — and the
/// plan runs it at rank 8, the delete at 12, and the key at 13: the delete
/// goes through and the key then fails validation on every backfilled row.
/// The count is asked from the plan alone. The negative halves are the same
/// plan with a default that names a row that stays, and with no default —
/// where a NULL in the tuple references nothing (`MATCH SIMPLE`); and the
/// refusal DECISIONS 124 asks for when the default is not one the probe can
/// evaluate.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_this_plan_adds_on_a_column_it_adds_counts_the_backfilled_rows() {
    let mut conn = connect().await;
    let s = data_schema("backfilled");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c1'), ('c2');"
    ))
    .await
    .expect("the fixture");
    // The hazard, by hand and in the plan's own order.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.child ADD COLUMN parent text DEFAULT 'old';
         DELETE FROM {s}.parent WHERE code = 'old'"
    ))
    .await
    .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_parent_fkey \
             FOREIGN KEY (parent) REFERENCES {s}.parent(code)"
        ))
        .await
        .expect_err("the backfilled rows fail the key's validation");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut added = Column::new(ty("text"));
    added.default = Some("'old'".into());
    child.columns.insert("parent".into(), added);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_parent_fkey".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let pg = Postgres::new();

    let planned_count = |probes: &[pbps_dialect::Probe]| -> String {
        probes
            .iter()
            .find(|p| p.description.contains("on a column it also adds"))
            .expect("the planned key is asked about")
            .sql
            .clone()
    };
    // The base's ids first, and the declaration's minted on top of them, so
    // the column the base does not have is an addition and not a rename.
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    assert_eq!(cs.changes.len(), 3, "{cs:#?}");
    assert!(
        matches!(cs.changes[0].change, pbps_model::Change::AddColumn { .. }),
        "the column comes first: {cs:#?}"
    );
    let probes = pg.preflight(&cs);
    assert_eq!(
        counted(&mut conn, &planned_count(&probes)).await,
        2,
        "both stored rows will hold the default, which is the doomed row: {probes:#?}"
    );
    assert!(
        !probes
            .iter()
            .any(|p| p.description.contains("cannot evaluate")),
        "a literal default is one the probe evaluates: {probes:#?}"
    );

    // The same key, backfilled with a row that stays.
    let set_default = |declared: &mut Schema, default: Option<&str>| {
        declared
            .tables
            .get_mut(&child_name)
            .expect("the child")
            .columns
            .get_mut("parent")
            .expect("the column")
            .default = default.map(Into::into);
    };
    set_default(&mut declared, Some("'keep'"));
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    assert_eq!(
        counted(&mut conn, &planned_count(&probes)).await,
        0,
        "'keep' is not the deleted row: {probes:#?}"
    );
    // Added with no default: every stored row holds NULL there, which
    // references nothing.
    set_default(&mut declared, None);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    assert_eq!(
        counted(&mut conn, &planned_count(&probes)).await,
        0,
        "a NULL tuple references no row: {probes:#?}"
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan with no default runs whole"
    );
    conn.execute(&format!(
        "ALTER TABLE {s}.child DROP CONSTRAINT child_parent_fkey;
         ALTER TABLE {s}.child DROP COLUMN parent;
         INSERT INTO {s}.parent VALUES ('old', 'Old')"
    ))
    .await
    .expect("back to the fixture");

    // And a default the probe cannot evaluate, backfilled into every stored
    // row: refused, counted as the rows it reaches.
    set_default(&mut declared, Some("lower('OLD'::text)"));
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the backfill is refused");
    assert!(
        refusal.description.contains("parent of ")
            && refusal
                .description
                .contains(".child (its default, backfilled"),
        "{}",
        refusal.description
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        2,
        "every stored row gets the default: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A stored child row this plan updates to a NULL in one column of a key and
/// to an unevaluable default in another leaves the count, as the engine's
/// rule says it does.
///
/// The refusal over unevaluable defaults already asks per row whether the
/// same key holds a NULL the same row writes. The count's own exclusion did
/// not: its guard gave up on the unevaluable column first, the row stayed
/// counted with its stored tuple, and the ordinary update-then-delete plan
/// was refused for a row that will reference nothing (DECISIONS 336).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_row_updated_to_a_null_beside_an_unprobeable_default_leaves_the_count() {
    let mut conn = connect().await;
    let s = data_schema("updnull");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
             CONSTRAINT parent_xy UNIQUE (x, y));
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             a text DEFAULT 'ox', b text,
             CONSTRAINT child_ab FOREIGN KEY (a, b) REFERENCES {s}.parent(x, y));
         INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'ox', 'oy');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('ZZ'::text)".into());
    child.columns.insert("a".into(), a);
    child.columns.insert("b".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // The stored row references `old` and holds `a` at its default, which
    // the declaration changes to one no probe can evaluate: the plan sets `b`
    // to NULL and `a` to the new default, which is the update the count has
    // to see through.
    with_data(
        &mut child,
        DataMode::Exact,
        &[("1", row(&[("b", Value::Null)]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let update = cs
        .changes
        .iter()
        .find_map(|c| {
            if let pbps_model::Change::UpdateRow { columns, .. } = &c.change {
                Some(columns)
            } else {
                None
            }
        })
        .expect("the child row is updated, not rewritten");
    assert!(
        update.contains_key("a") && update.contains_key("b"),
        "both columns of the key are written: {update:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`old`")
        })
        .expect("the count for the row the child references");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        0,
        "the row's tuple after the update holds a NULL: {}",
        count.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        0,
        "both parent rows went"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A key this plan adds into a column it adds to the *parent* is counted
/// through the value that column is backfilled with — and not against the
/// parent rows the plan leaves in place.
///
/// The referenced side is backfilled exactly as the child side is, so the
/// deleted row's value in the new column is the default, which every other
/// parent row holds too. **Measured**: with every parent row deleted the key
/// fails validation on the child (23503); with one row left it is added,
/// because the child references that row as well as it ever referenced the
/// deleted one. The negative half declares one parent row and keeps it.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_into_a_column_this_plan_adds_to_the_parent_counts_against_its_backfill() {
    let mut conn = connect().await;
    let s = data_schema("parentfill");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, ref text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c1', 'x');"
    ))
    .await
    .expect("the fixture");
    // The hazard by hand, in the plan's order: every parent row deleted.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.parent ADD COLUMN alt text DEFAULT 'x';
         DELETE FROM {s}.parent;
         ALTER TABLE {s}.parent ADD CONSTRAINT parent_alt UNIQUE (alt)"
    ))
    .await
    .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_ref \
             FOREIGN KEY (ref) REFERENCES {s}.parent(alt)"
        ))
        .await
        .expect_err("the child references the backfilled value of no row");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut alt = Column::new(ty("text"));
    alt.default = Some("'x'".into());
    parent.columns.insert("alt".into(), alt);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_alt".into(),
        UniqueConstraint {
            columns: vec!["alt".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child.columns.insert("ref".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["alt".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned: Vec<_> = probes
        .iter()
        .filter(|p| p.description.contains("on a column it also adds"))
        .collect();
    assert_eq!(planned.len(), 2, "one per deleted parent row: {probes:#?}");
    for p in &planned {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            1,
            "no parent row survives to be the one the child references: {}",
            p.sql
        );
    }

    // The negative half: `keep` is declared and stays, holding the same
    // backfilled value, so the child references it and the delete is valid.
    with_data(
        declared.tables.get_mut(&parent_name).expect("the parent"),
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned: Vec<_> = probes
        .iter()
        .filter(|p| p.description.contains("on a column it also adds"))
        .collect();
    assert_eq!(planned.len(), 1, "{probes:#?}");
    assert_eq!(
        counted(&mut conn, &planned[0].sql).await,
        0,
        "a surviving parent row holds the tuple: {}",
        planned[0].sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan runs whole, key and all"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// An inserted row that omits a column of a key the table gives no default
/// for puts a NULL there, and the probe knows it.
///
/// The plan carries every such column in the insert's `types`, and a NULL in
/// any column of a key is the value that makes the whole tuple reference
/// nothing (`MATCH SIMPLE`). Left out of what the probe knew about the row,
/// it was neither compared nor seen by the refusal over unevaluable defaults,
/// which then refused an insert leaving a sibling column of the key to such a
/// default — for a tuple the engine never checks.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_omitted_cell_with_no_default_is_a_null_the_probe_knows() {
    let mut conn = connect().await;
    let s = data_schema("omitnull");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
             CONSTRAINT parent_xy UNIQUE (x, y));
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             a text DEFAULT lower('OX'), b text,
             CONSTRAINT child_ab FOREIGN KEY (a, b) REFERENCES {s}.parent(x, y));
         INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('OX'::text)".into());
    child.columns.insert("a".into(), a);
    child.columns.insert("b".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // The row spells neither `a` nor `b`: `a` takes the default the probe
    // cannot evaluate, `b` has no default and is NULL.
    with_data(&mut child, DataMode::Exact, &[("1", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the refusal is still offered");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        0,
        "the omitted column is a NULL in the same key: {}",
        refusal.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert!(
        truth(
            &mut conn,
            &format!("SELECT b IS NULL AND a = 'ox' FROM {s}.child WHERE id = 1")
        )
        .await,
        "the default named a deleted row's column, beside a NULL"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A planned column backfilled with a typed NULL — `NULL::text` — is a NULL.
///
/// Beside a sibling column of the same planned key whose default the probe
/// cannot evaluate, the NULL makes every backfilled tuple reference nothing;
/// read as a value, it did not, and every stored child row was refused for a
/// key the engine will never check.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_typed_null_backfill_is_a_null_beside_an_unprobeable_one() {
    let mut conn = connect().await;
    let s = data_schema("typednull");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
             CONSTRAINT parent_xy UNIQUE (x, y));
         CREATE TABLE {s}.child (id integer PRIMARY KEY);
         INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');
         INSERT INTO {s}.child VALUES (1), (2);"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('OX'::text)".into());
    child.columns.insert("a".into(), a);
    let mut b = Column::new(ty("text"));
    b.default = Some("NULL::text".into());
    child.columns.insert("b".into(), b);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert!(
        !probes
            .iter()
            .any(|p| p.description.contains("cannot evaluate")),
        "a typed NULL beside the unevaluable default refuses nothing: {probes:#?}"
    );
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is still counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "every backfilled tuple holds a NULL: {}",
        planned.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        0,
        "the plan runs whole"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The survivors a planned key's backfilled parent side is checked against
/// are the parent rows as this plan leaves them, not as they stand.
///
/// An update that moves the surviving parent row off the backfilled value
/// runs before the delete, so a child holding that value then references no
/// row and the key fails; a parent row this plan inserts with the value is a
/// survivor the engine will accept. Both by the plan's own apply, which is
/// the measurement: the first is refused before it, the second runs whole.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_surviving_parent_row_is_the_row_this_plan_leaves_there() {
    let mut conn = connect().await;
    let s = data_schema("survivor");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, ref text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c1', 'x');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut alt = Column::new(ty("text"));
    alt.default = Some("'x'".into());
    parent.columns.insert("alt".into(), alt);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_alt".into(),
        UniqueConstraint {
            columns: vec!["alt".into()],
        },
    );
    // `keep` survives, moved off the backfilled value: nothing is left for
    // the child to reference once `old` is gone.
    with_data(
        &mut parent,
        DataMode::Exact,
        &[(
            "keep",
            row(&[
                ("label", Value::Text("Keep".into())),
                ("alt", Value::Text("y".into())),
            ]),
        )],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child.columns.insert("ref".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["alt".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::UpdateRow { .. })),
        "the survivor is updated: {cs:#?}"
    );
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        1,
        "the survivor no longer holds the value: {}",
        planned.sql
    );
    // The hazard by hand, in the plan's order.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.parent ADD COLUMN alt text DEFAULT 'x';
         UPDATE {s}.parent SET alt = 'y' WHERE code = 'keep';
         DELETE FROM {s}.parent WHERE code = 'old';
         ALTER TABLE {s}.parent ADD CONSTRAINT parent_alt UNIQUE (alt)"
    ))
    .await
    .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_ref \
             FOREIGN KEY (ref) REFERENCES {s}.parent(alt)"
        ))
        .await
        .expect_err("the child references the value no row holds any more");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    // The inverse: a parent row this plan inserts holds the value, and the
    // delete is valid because the child references it.
    declared
        .tables
        .get_mut(&parent_name)
        .expect("the parent")
        .data
        .as_mut()
        .expect("declared rows")
        .rows
        .insert(
            RowKey::from("fresh"),
            row(&[
                ("label", Value::Text("Fresh".into())),
                ("alt", Value::Text("x".into())),
            ]),
        );
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "the inserted parent row is a survivor holding the value: {}",
        planned.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(
            &mut conn,
            &format!("SELECT code FROM {s}.parent WHERE alt = 'x'")
        )
        .await,
        "fresh",
        "the plan runs whole, key and all"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A backfilled literal is compared through its column's type, as every other
/// default is.
///
/// Two planned columns, one on each side of a planned key, both `date`: the
/// parent is added with `'2026-01-02'` and the child with `'01/02/2026'`,
/// which the engine stores as one value. **Measured**, `'2026-01-02' =
/// '01/02/2026'` is false — two unknown literals compare as text — and the
/// same through `CAST(… AS date)` is true; a probe comparing the spellings
/// reported no reference and let the delete through. The negative half is a
/// child default naming another day.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_backfilled_literal_is_compared_through_its_columns_type() {
    let mut conn = connect().await;
    let s = data_schema("typedfill");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY);
         INSERT INTO {s}.parent VALUES ('old', 'Old');
         INSERT INTO {s}.child VALUES ('c1'), ('c2');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut on = Column::new(ty("date"));
    on.default = Some("'2026-01-02'".into());
    parent.columns.insert("on_day".into(), on);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_on_day".into(),
        UniqueConstraint {
            columns: vec!["on_day".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut day = Column::new(ty("date"));
    day.default = Some("'01/02/2026'".into());
    child.columns.insert("day".into(), day);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_day".into(),
        ForeignKey {
            columns: vec!["day".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["on_day".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        2,
        "two spellings of one day are one value: {}",
        planned.sql
    );
    // Another day: no reference.
    declared
        .tables
        .get_mut(&child_name)
        .expect("the child")
        .columns
        .get_mut("day")
        .expect("the column")
        .default = Some("'2026-01-03'".into());
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "a different day references nothing: {}",
        planned.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A column added as an identity is backfilled by the sequence, and no probe
/// can say with what.
///
/// **Measured**: `ADD COLUMN pid integer GENERATED BY DEFAULT AS IDENTITY`
/// hands every stored row a value — `1`, `2`, … — and a key from it into a
/// parent whose row `1` this plan deletes fails validation after the delete
/// (23503). Read as "no default, so NULL", the probe counted nothing; it is
/// a backfill the probe cannot evaluate, and refused as one.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_identity_column_this_plan_adds_is_a_backfill_no_probe_can_evaluate() {
    let mut conn = connect().await;
    let s = data_schema("identfill");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code integer PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY);
         INSERT INTO {s}.parent VALUES (1, 'One'), (2, 'Two');
         INSERT INTO {s}.child VALUES ('a'), ('b');"
    ))
    .await
    .expect("the fixture");
    // The hazard by hand, in the plan's order.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.child ADD COLUMN pid integer GENERATED BY DEFAULT AS IDENTITY;
         DELETE FROM {s}.parent WHERE code = 1"
    ))
    .await
    .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_pid \
             FOREIGN KEY (pid) REFERENCES {s}.parent(code)"
        ))
        .await
        .expect_err("the sequence handed a stored row the deleted key");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("integer")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("2", row(&[("label", Value::Text("Two".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut pid = Column::new(ty("integer"));
    pid.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    child.columns.insert("pid".into(), pid);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_pid".into(),
        ForeignKey {
            columns: vec!["pid".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::AddColumn { .. })),
        "the identity column is added: {cs:#?}"
    );
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the identity backfill is refused");
    assert!(
        refusal
            .description
            .contains("its identity, assigned to every stored row"),
        "{}",
        refusal.description
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        2,
        "every stored row is handed a value: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A key this plan adds on columns it retypes compares the values the retype
/// leaves, not the stored ones.
///
/// `ALTER COLUMN … TYPE` runs at rank 9, the delete at 12, the key at 13.
/// **Measured**: a child `numeric(5,2)` holding `1.04` and the doomed parent's
/// `1.00`, both narrowed to `numeric(5,1)`, are one value `1.0` by the time
/// the key is validated — and the key fails on it (23503) — while as stored
/// they are two. The negative half is a child value that rounds elsewhere.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_this_plan_adds_on_a_column_it_retypes_compares_the_converted_values() {
    let mut conn = connect().await;
    let s = data_schema("retyped");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, amount numeric(5,2),
             CONSTRAINT parent_amount UNIQUE (amount));
         CREATE TABLE {s}.child (code text PRIMARY KEY, amount numeric(5,2));
         INSERT INTO {s}.parent VALUES ('old', 1.00), ('keep', 2.00);
         INSERT INTO {s}.child VALUES ('c1', 1.04);"
    ))
    .await
    .expect("the fixture");
    // The hazard by hand, in the plan's order.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.parent ALTER COLUMN amount TYPE numeric(5,1);
         ALTER TABLE {s}.child ALTER COLUMN amount TYPE numeric(5,1);
         DELETE FROM {s}.parent WHERE code = 'old'"
    ))
    .await
    .expect("nothing in the catalog stops the delete");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_amount \
             FOREIGN KEY (amount) REFERENCES {s}.parent(amount)"
        ))
        .await
        .expect_err("the converted child value is the deleted row's");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("amount".into(), Column::new(ty("numeric(5,1)")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_amount".into(),
        UniqueConstraint {
            columns: vec!["amount".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("amount", Value::Text("2.0".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("amount".into(), Column::new(ty("numeric(5,1)")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_amount".into(),
        ForeignKey {
            columns: vec!["amount".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["amount".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    assert_eq!(
        cs.changes
            .iter()
            .filter(|c| matches!(c.change, pbps_model::Change::AlterColumnType { .. }))
            .count(),
        2,
        "both columns are retyped: {cs:#?}"
    );
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the key on retyped columns is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        1,
        "1.04 and 1.00 are one value once narrowed: {}",
        planned.sql
    );
    // A child value that narrows elsewhere references nothing.
    conn.execute(&format!(
        "UPDATE {s}.child SET amount = 1.06 WHERE code = 'c1'"
    ))
    .await
    .expect("move the child");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "1.06 narrows to 1.1: {}",
        planned.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A session that can read the columns the count reads can count, and is
/// not refused for lacking a grant on the whole table.
///
/// **Measured**: with `SELECT (parent)` on the child and no table-level
/// `SELECT`, `has_table_privilege` is false while the count — which reads
/// only the key's columns — runs and answers. The refusal asked the table
/// question and refused a valid delete. It now asks about the columns the
/// generated count reads; a role with neither is still refused, and one
/// whose plan also names the child's rows by key needs that column too.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_column_grant_that_covers_the_count_is_enough_to_count() {
    let mut conn = connect().await;
    let s = data_schema("colgrant");
    fresh(&mut conn, &s).await;
    let role = format!("{s}_dep");
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, parent text REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');
         GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.parent TO {role};
         GRANT SELECT (parent) ON {s}.child TO {role};"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    assert_eq!(
        counted(&mut deployer, &probes[0].sql).await,
        1,
        "the count reads only the key's column, which the role can read: {}",
        probes[0].sql
    );
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        0,
        "and so the count is complete, and nothing is refused: {}",
        probes[1].sql
    );
    // The negative half: without the column grant the count is an error and
    // the refusal says so.
    conn.execute(&format!("REVOKE SELECT (parent) ON {s}.child FROM {role}"))
        .await
        .expect("revoke");
    let denied = match deployer.query(&probes[0].sql).await {
        Err(e) => e,
        Ok(_) => panic!("the role cannot read the child now"),
    };
    assert_eq!(sqlstate(&denied), "42501", "{denied:?}");
    assert_eq!(counted(&mut deployer, &probes[1].sql).await, 1);
    drop(deployer);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A surviving parent row that holds the converted value is a survivor.
///
/// Two parent rows, `1.04` and `1.00`, both narrowed to `numeric(5,1)` by
/// the plan, are one value `1.0`; the plan deletes the first and adds the
/// unique key and the foreign key after. The child's converted `1.0`
/// references the survivor, and the engine takes the plan whole. The probe
/// converted the doomed row's side and never asked the converted survivor,
/// so it attributed the child to the doomed row and refused. The negative
/// half deletes both.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_retyped_survivor_that_holds_the_converted_value_is_a_survivor() {
    let mut conn = connect().await;
    let s = data_schema("retypedsurv");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, amount numeric(5,2));
         CREATE TABLE {s}.child (code text PRIMARY KEY, amount numeric(5,2));
         INSERT INTO {s}.parent VALUES ('old', 1.04), ('keep', 1.00);
         INSERT INTO {s}.child VALUES ('c1', 1.04);"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("amount".into(), Column::new(ty("numeric(5,1)")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_amount".into(),
        UniqueConstraint {
            columns: vec!["amount".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("amount", Value::Text("1.0".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("amount".into(), Column::new(ty("numeric(5,1)")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_amount".into(),
        ForeignKey {
            columns: vec!["amount".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["amount".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the key on retyped columns is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "`keep` narrows to the same 1.0 and survives: {}",
        planned.sql
    );
    // The negative half: nothing survives.
    with_data(
        declared.tables.get_mut(&parent_name).expect("the parent"),
        DataMode::Exact,
        &[],
    );
    let cs_none = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs_none);
    let refused: Vec<_> = probes
        .iter()
        .filter(|p| p.description.contains("on a column it also adds"))
        .collect();
    assert_eq!(refused.len(), 2, "{probes:#?}");
    let mut total = 0;
    for p in refused {
        total += counted(&mut conn, &p.sql).await;
    }
    assert_eq!(total, 2, "with both rows gone the child references nothing");
    // And the valid plan runs whole.
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT amount::text FROM {s}.child")).await,
        "1.0",
        "narrowed, keyed to the survivor"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A parent row this plan inserts and a child row it inserts, spelling one
/// `numeric` two ways, meet as one value.
///
/// `1.00` and `1.0` are one number to the engine — a key from the second
/// into the first holds — and two strings to a comparison of unknown
/// literals; both spellings read back as written, so both are values a plan
/// can carry. The doomed parent row holds the planned column's backfill;
/// `keep` is moved off it, `fresh` is inserted with `1.00`, and the child
/// arrives with `1.0`. The child references `fresh`, and the plan is valid.
/// Compared as the literals they are, the child was attributed to the
/// doomed row and to no survivor, and the plan refused.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_inserted_survivor_and_an_arriving_child_meet_through_the_columns_type() {
    let mut conn = connect().await;
    let s = data_schema("typedsurv");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut amt = Column::new(ty("numeric"));
    amt.default = Some("1.0".into());
    parent.columns.insert("amt".into(), amt);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_amt".into(),
        UniqueConstraint {
            columns: vec!["amt".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[
            (
                "keep",
                row(&[
                    ("label", Value::Text("Keep".into())),
                    ("amt", Value::Text("7".into())),
                ]),
            ),
            (
                "fresh",
                row(&[
                    ("label", Value::Text("Fresh".into())),
                    ("amt", Value::Text("1.00".into())),
                ]),
            ),
        ],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("amt".into(), Column::new(ty("numeric")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_amt".into(),
        ForeignKey {
            columns: vec!["amt".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["amt".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c1", row(&[("amt", Value::Text("1.0".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "the arriving child references the inserted survivor: {}",
        planned.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(
            &mut conn,
            &format!(
                "SELECT p.code FROM {s}.child c JOIN {s}.parent p ON p.amt = c.amt \
                 WHERE c.code = 'c1'"
            )
        )
        .await,
        "fresh",
        "the plan runs whole, and the child is keyed to the inserted row"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A referencing table in a schema the session may not use is one it cannot
/// count, whatever it holds on the table itself.
///
/// **Measured**: with `SELECT` on the child and no `USAGE` on its schema,
/// `has_table_privilege(oid, 'SELECT')` is true and the count fails with
/// `permission denied for schema` — which the probe runner reads as
/// unchecked and walks past. The refusal asked only the table question and
/// answered `0`. It now asks `has_schema_privilege` first; granting `USAGE`
/// is the negative half.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_referencing_table_in_a_schema_the_session_cannot_use_refuses_the_delete() {
    let mut conn = connect().await;
    let s = data_schema("nousage");
    let other = format!("{s}_other");
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &other).await;
    let role = format!("{s}_dep");
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {other}.child (id integer PRIMARY KEY, parent text REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {other}.child VALUES (1, 'old');
         GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.parent TO {role};
         GRANT SELECT ON {other}.child TO {role};"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    let denied = match deployer.query(&probes[0].sql).await {
        Err(e) => e,
        Ok(_) => panic!("the role cannot use the child's schema"),
    };
    assert_eq!(sqlstate(&denied), "42501", "{denied:?}");
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        1,
        "the table grant does not reach through the schema: {}",
        probes[1].sql
    );
    // With `USAGE` the count runs, and the refusal has nothing to say.
    conn.execute(&format!("GRANT USAGE ON SCHEMA {other} TO {role}"))
        .await
        .expect("grant usage");
    assert_eq!(counted(&mut deployer, &probes[0].sql).await, 1);
    assert_eq!(counted(&mut deployer, &probes[1].sql).await, 0);
    drop(deployer);
    conn.execute(&format!(
        "DROP SCHEMA {other} CASCADE; DROP SCHEMA {s} CASCADE"
    ))
    .await
    .expect("drop");
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A child table this plan creates, with a key into an existing parent and
/// rows of its own, is a child whose arrivals are counted.
///
/// The differ splits a new table's foreign keys out of the `CREATE` into
/// `AddForeignKey`, which sorts after the deletes (rank 13), and its rows
/// into `InsertRow`, which sorts before them (rank 11). **Measured**, in
/// that order: the table is created, the row inserted, the parent row
/// deleted, and the key then fails on the orphan (23503) — DECISIONS 335
/// had the key created with the table, and skipped such a child. The
/// negative half inserts a row that references the row that stays.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_child_this_plan_creates_arrives_on_the_parent_before_its_key_exists() {
    let mut conn = connect().await;
    let s = data_schema("createdchild");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');"
    ))
    .await
    .expect("the fixture");
    // The hazard by hand, in the plan's order.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "CREATE TABLE {s}.child (code text PRIMARY KEY, parent text);
         INSERT INTO {s}.child VALUES ('c1', 'old');
         DELETE FROM {s}.parent WHERE code = 'old'"
    ))
    .await
    .expect("nothing stops the delete before the key exists");
    let broken = conn
        .execute(&format!(
            "ALTER TABLE {s}.child ADD CONSTRAINT child_parent_fkey \
             FOREIGN KEY (parent) REFERENCES {s}.parent(code)"
        ))
        .await
        .expect_err("the inserted child is an orphan");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("parent".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_parent_fkey".into(),
        ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c1", row(&[("parent", Value::Text("old".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::AddForeignKey { .. })),
        "the key is its own change, after the delete: {cs:#?}"
    );
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the created child's key is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        1,
        "the inserted row arrives on the doomed parent: {}",
        planned.sql
    );
    // The negative half: the row references the parent row that stays.
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("c1", row(&[("parent", Value::Text("keep".into()))]))],
    );
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the created child's key is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "the row references `keep`: {}",
        planned.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT parent FROM {s}.child")).await,
        "keep",
        "the plan runs whole, key and all"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// An inserted row that leaves a key column to an identity the table already
/// has is refused before the first statement, as a write to any other value
/// the probe cannot evaluate is.
///
/// `InsertRow::types` leaves identity columns out on purpose — the engine
/// owns them — so the column was neither a value the probe compared nor a
/// default it refused, and a key reaching it was simply not asked about.
/// **Measured**: the sequence hands the row `1`, the parent row `1` is the
/// one this plan deletes, and the delete's own guard is what stops it — after
/// the insert has committed under a staged apply. The negative half spells
/// the sibling column of the key NULL.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_insert_leaving_a_key_column_to_an_identity_is_refused() {
    let mut conn = connect().await;
    let s = data_schema("identinsert");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code integer PRIMARY KEY, x text, label text,
             CONSTRAINT parent_code_x UNIQUE (code, x));
         CREATE TABLE {s}.child (code text PRIMARY KEY,
             seq integer GENERATED BY DEFAULT AS IDENTITY, x text,
             CONSTRAINT child_seq_x FOREIGN KEY (seq, x) REFERENCES {s}.parent(code, x));
         INSERT INTO {s}.parent VALUES (1, 'a', 'One'), (2, 'b', 'Two');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("integer")).not_null());
    parent.columns.insert("x".into(), Column::new(ty("text")));
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_code_x".into(),
        UniqueConstraint {
            columns: vec!["code".into(), "x".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[(
            "2",
            row(&[
                ("x", Value::Text("b".into())),
                ("label", Value::Text("Two".into())),
            ]),
        )],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut seq = Column::new(ty("integer")).not_null();
    seq.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    child.columns.insert("seq".into(), seq);
    child.columns.insert("x".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_seq_x".into(),
        ForeignKey {
            columns: vec!["seq".into(), "x".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into(), "x".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // `seq` is the engine's; `x` says `a`, which with `seq = 1` is the row
    // this plan deletes.
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c1", row(&[("x", Value::Text("a".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the identity is refused");
    assert!(
        refusal
            .description
            .contains("leaves to the engine to assign"),
        "{}",
        refusal.description
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "the key reaches the identity column: {}",
        refusal.sql
    );
    // And the hazard it refuses, by hand: the sequence names the doomed row.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "INSERT INTO {s}.child (code, x) VALUES ('c1', 'a')"
    ))
    .await
    .expect("the sequence hands the row 1, which references parent 1");
    let broken = conn
        .execute(&format!("DELETE FROM {s}.parent WHERE code = 1"))
        .await
        .expect_err("the parent row is now referenced");
    assert_eq!(sqlstate(&broken), "23503", "{broken:?}");
    conn.execute("ROLLBACK").await.expect("rollback");

    // The negative half: with `x` NULL the tuple references nothing,
    // whatever the sequence assigns.
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("c1", row(&[("x", Value::Null)]))],
    );
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the refusal is still offered");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        0,
        "a NULL in the key: {}",
        refusal.sql
    );
    apply(&mut conn, &pg, &cs).await;
    // The sequence has moved on past the rolled-back insert above — a
    // sequence is not transactional — so only the shape is asserted.
    assert!(
        truth(
            &mut conn,
            &format!("SELECT x IS NULL AND seq > 0 FROM {s}.child WHERE code = 'c1'")
        )
        .await,
        "the plan ran whole, whatever the sequence handed the row"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A foreign key whose delete action is switched off is not one the delete
/// meets.
///
/// **Measured**: `ALTER TABLE parent DISABLE TRIGGER ALL` leaves the
/// constraint row saying validated and enforced, and the parent row then
/// deletes with its child sitting there; the same under
/// `session_replication_role = replica`. Introspection already leaves such a
/// key out of the model. The probe and the delete's guard counted through it
/// and refused a delete the engine takes; both now ask whether the
/// parent-side delete trigger will fire. The negative half is the key with
/// its triggers back on.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_key_whose_delete_action_is_switched_off_is_not_counted() {
    let mut conn = connect().await;
    let s = data_schema("triggeroff");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, parent text REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');
         ALTER TABLE {s}.parent DISABLE TRIGGER ALL;"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "the delete action will not run, so the key is not one the delete meets: {}",
        probes[0].sql
    );
    // The key with its triggers on is counted, and so is the same key under
    // the replica role, where they will not fire either.
    conn.execute(&format!("ALTER TABLE {s}.parent ENABLE TRIGGER ALL"))
        .await
        .expect("enable");
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 1);
    conn.execute("SET session_replication_role = replica")
        .await
        .expect("replica role (superuser)");
    assert_eq!(
        counted(&mut conn, &probes[0].sql).await,
        0,
        "origin-mode triggers do not fire under the replica role: {}",
        probes[0].sql
    );
    conn.execute("SET session_replication_role = origin")
        .await
        .expect("back");
    // And the whole plan runs with the triggers off — the guard is the same
    // count, and it takes the delete the engine takes.
    conn.execute(&format!("ALTER TABLE {s}.parent DISABLE TRIGGER ALL"))
        .await
        .expect("disable");
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "`old` went, with its child left where the operator left the key"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A key this plan adds on columns it leaves alone is asked about with the
/// same survivor check as one on columns it changes.
///
/// Two parent rows share the referenced value today, because nothing yet
/// says it is unique; the plan deletes one, adds the unique key, and adds
/// the foreign key after — the child references the survivor, and the
/// engine takes the plan whole. Counted through a constraint row that could
/// only say "this child's value is the deleted row's", the plan was refused.
/// Every planned key now takes the one path that knows about survivors. The
/// negative half deletes both rows.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_planned_key_on_unchanged_columns_sees_the_survivor_too() {
    let mut conn = connect().await;
    let s = data_schema("plainsurv");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, grp text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, grp text);
         INSERT INTO {s}.parent VALUES ('old', 'g'), ('keep', 'g');
         INSERT INTO {s}.child VALUES ('c1', 'g');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent.columns.insert("grp".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_grp".into(),
        UniqueConstraint {
            columns: vec!["grp".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("grp", Value::Text("g".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child.columns.insert("grp".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_grp".into(),
        ForeignKey {
            columns: vec!["grp".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["grp".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is asked about from the plan");
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        0,
        "`keep` holds the value and survives: {}",
        planned.sql
    );
    // The catalog's own count sees no key: it is not there yet.
    assert_eq!(counted(&mut conn, &probes[0].sql).await, 0);
    // The negative half: nothing survives.
    with_data(
        declared.tables.get_mut(&parent_name).expect("the parent"),
        DataMode::Exact,
        &[],
    );
    let cs_none = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs_none);
    let mut total = 0;
    for p in probes
        .iter()
        .filter(|p| p.description.contains("on a column it also adds"))
    {
        total += counted(&mut conn, &p.sql).await;
    }
    assert_eq!(total, 2, "with both rows gone the child references nothing");
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        text(&mut conn, &format!("SELECT code FROM {s}.parent")).await,
        "keep",
        "the plan runs whole, unique key, foreign key and all"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A child row arriving against a parent backfill the probe cannot evaluate
/// is refused with the stored rows that backfill reaches.
///
/// The parent gains an identity column the plan then keys the child on; the
/// deleted parent row is handed a value by the sequence, and a child row the
/// plan inserts may spell exactly that value. The stored children were
/// refused for it; the arriving one was not counted anywhere. The negative
/// half is an arriving row that spells the column NULL.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_row_arriving_against_an_unprobeable_parent_backfill_is_refused() {
    let mut conn = connect().await;
    let s = data_schema("arrivefill");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, ref integer);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut alt = Column::new(ty("integer")).not_null();
    alt.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    parent.columns.insert("alt".into(), alt);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_alt".into(),
        UniqueConstraint {
            columns: vec!["alt".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("ref".into(), Column::new(ty("integer")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["alt".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c1", row(&[("ref", Value::Int(1))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the identity backfill is refused");
    assert!(
        refusal
            .description
            .contains("written against that backfill"),
        "{}",
        refusal.description
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "no stored child rows, one arriving: {}",
        refusal.sql
    );
    // The negative half: the arriving row spells the column NULL, and
    // references nothing whatever the sequence hands the parent.
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("c1", row(&[("ref", Value::Null)]))],
    );
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the refusal is still offered");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        0,
        "a NULL arriving references nothing: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A policy on one partition of a referencing table does not make the count
/// through the partitioned relation incomplete, and the delete's guard does
/// not refuse for it.
///
/// **Measured**: a role the leaf's policy hides every row from still counts
/// the row through the partitioned parent — partition policies do not apply
/// to a scan of the parent — while `row_security_active` is true for the
/// leaf. The guard asked every constraint row, the copied one for the leaf
/// included, and refused a delete whose count was complete; the probe and
/// the count had filtered the copies out (DECISIONS 334). The negative half
/// puts the policy on the partitioned table itself.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_policy_on_a_partition_does_not_refuse_a_delete_counted_through_its_parent() {
    let mut conn = connect().await;
    let s = data_schema("leafrls");
    fresh(&mut conn, &s).await;
    let role = format!("{s}_dep");
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer, parent text REFERENCES {s}.parent(code),
             PRIMARY KEY (id, parent)) PARTITION BY LIST (parent);
         CREATE TABLE {s}.child_old PARTITION OF {s}.child FOR VALUES IN ('old');
         CREATE TABLE {s}.child_rest PARTITION OF {s}.child DEFAULT;
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'keep');
         ALTER TABLE {s}.child_old ENABLE ROW LEVEL SECURITY;
         CREATE POLICY hide ON {s}.child_old USING (false);
         GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.parent TO {role};
         GRANT SELECT ON {s}.child, {s}.child_old, {s}.child_rest TO {role};"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    assert_eq!(probes.len(), 2, "{probes:#?}");
    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    assert_eq!(counted(&mut deployer, &probes[0].sql).await, 0);
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        0,
        "the leaf's policy does not filter the count through the parent: {}",
        probes[1].sql
    );
    // And the guard, which is the finding: the delete goes through as the
    // deploying role.
    apply(&mut deployer, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "`old` went"
    );
    // The negative half: the policy on the partitioned table itself filters
    // the count, and the probe refuses.
    conn.execute(&format!(
        "INSERT INTO {s}.parent VALUES ('old', 'Old');
         ALTER TABLE {s}.child ENABLE ROW LEVEL SECURITY;
         CREATE POLICY hide ON {s}.child USING (false);"
    ))
    .await
    .expect("policy on the parent relation");
    assert_eq!(
        counted(&mut deployer, &probes[1].sql).await,
        1,
        "the partitioned relation's own policy is the one that filters: {}",
        probes[1].sql
    );
    drop(deployer);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A row that spells a value into a column whose backfill is NULL is a row
/// whose tuple holds no NULL, whatever every stored row holds.
///
/// The child gains a column `DEFAULT NULL` the plan keys on a parent column
/// the sequence backfills; every stored child row references nothing, and
/// the table-wide answer said so. The row the plan inserts spells `1`, which
/// may be what the sequence hands the deleted parent row — and it was let
/// through on the stored rows' answer. The negative half leaves the column to
/// its NULL default.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_row_spelling_a_value_over_a_null_backfill_is_refused_on_its_own_tuple() {
    let mut conn = connect().await;
    let s = data_schema("overnull");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY);
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES ('c0');"
    ))
    .await
    .expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut alt = Column::new(ty("integer")).not_null();
    alt.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    parent.columns.insert("alt".into(), alt);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_alt".into(),
        UniqueConstraint {
            columns: vec!["alt".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    let mut r = Column::new(ty("integer"));
    r.default = Some("NULL".into());
    child.columns.insert("ref".into(), r);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["alt".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // `c0` is stored and takes the NULL backfill; `c1` arrives spelling `1`.
    with_data(
        &mut child,
        DataMode::Exact,
        &[("c0", row(&[])), ("c1", row(&[("ref", Value::Int(1))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the arriving row is refused on its own tuple");
    assert!(
        refusal.description.contains("`c1`") && !refusal.description.contains("`c0`"),
        "{}",
        refusal.description
    );
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "the stored row holds the NULL backfill; the arriving one spells a value: {}",
        refusal.sql
    );
    // The negative half: the arriving row leaves the column to its NULL.
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("c0", row(&[])), ("c1", row(&[]))],
    );
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    for p in probes
        .iter()
        .filter(|p| p.description.contains("cannot evaluate"))
    {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            0,
            "every tuple holds the NULL: {}",
            p.description
        );
    }
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A stored child row holding NULL in a column of the key references nothing,
/// whatever backfill the plan hands the parent side.
///
/// **Measured**: parent gains an identity column, every child `ref` is NULL,
/// the parent row is deleted, the key is added — and the engine accepts it
/// under `MATCH SIMPLE`. A refusal over every surviving stored row would
/// refuse that valid plan; the refusal reaches only rows whose stored
/// columns of the key hold a value (DECISIONS 348).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_stored_row_holding_null_in_the_key_is_not_refused_for_a_backfill_it_never_meets() {
    let mut conn = connect().await;
    let s = data_schema("nullstored");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code integer PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, ref integer);
         INSERT INTO {s}.parent VALUES (1, 'One'), (2, 'Two');
         INSERT INTO {s}.child VALUES ('a', NULL), ('b', NULL);"
    ))
    .await
    .expect("the fixture");
    // The plan by hand, in its order: the engine accepts the key.
    conn.execute("BEGIN").await.expect("begin");
    conn.execute(&format!(
        "ALTER TABLE {s}.parent ADD COLUMN pid integer GENERATED BY DEFAULT AS IDENTITY;
         ALTER TABLE {s}.parent ADD CONSTRAINT parent_pid UNIQUE (pid);
         DELETE FROM {s}.parent WHERE code = 1;
         ALTER TABLE {s}.child ADD CONSTRAINT child_ref \
          FOREIGN KEY (ref) REFERENCES {s}.parent(pid)"
    ))
    .await
    .expect("a NULL tuple references nothing, so the key is added");
    conn.execute("ROLLBACK").await.expect("rollback");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("integer")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    let mut pid = Column::new(ty("integer")).not_null();
    pid.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    parent.columns.insert("pid".into(), pid);
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_pid".into(),
        UniqueConstraint {
            columns: vec!["pid".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("2", row(&[("label", Value::Text("Two".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    child
        .columns
        .insert("ref".into(), Column::new(ty("integer")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["pid".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name, child);
    let pg = Postgres::new();
    let base = connected_base(&mut conn, &declared, &s).await;
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let ids = mint_ids(&declared, &base_ids, &[]);
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusals: Vec<_> = probes
        .iter()
        .filter(|p| p.description.contains("cannot evaluate"))
        .collect();
    assert!(
        !refusals.is_empty(),
        "the identity backfill is asked about: {probes:#?}"
    );
    for p in &refusals {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            0,
            "a stored NULL tuple meets no backfill: {}",
            p.sql
        );
    }
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(counted(&mut conn, &planned.sql).await, 0, "{}", planned.sql);
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan runs whole, key and all"
    );

    // The negative half: a stored row holding a value in the key meets the
    // backfill, and is refused.
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code integer PRIMARY KEY, label text);
         CREATE TABLE {s}.child (code text PRIMARY KEY, ref integer);
         INSERT INTO {s}.parent VALUES (1, 'One'), (2, 'Two');
         INSERT INTO {s}.child VALUES ('a', NULL), ('b', 7);"
    ))
    .await
    .expect("the fixture");
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &base_ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the identity backfill is refused");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "only the row holding a value is refused: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A NULL an update leaves alone in one column of a key is a NULL of the
/// row's tuple after the update, and the unevaluable default the same update
/// writes into another column of that key has nothing to reference.
///
/// The statement holds the row to its unchanged declared cells before and
/// after it runs, so the NULL is as sure as one the update writes. **Measured**:
/// the update to the default, the delete, and the key are accepted with the
/// NULL sitting there — `MATCH SIMPLE`. The refusal read only the cells the
/// update writes and refused that valid plan (DECISIONS 349).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_null_an_update_leaves_alone_is_a_null_of_the_tuple_it_writes() {
    let mut conn = connect().await;
    let s = data_schema("heldnull");
    fresh(&mut conn, &s).await;
    let fixture = |b: &str| {
        format!(
            "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
                 CONSTRAINT parent_xy UNIQUE (x, y));
             CREATE TABLE {s}.child (id integer PRIMARY KEY,
                 a text DEFAULT 'ox', b text,
                 CONSTRAINT child_ab FOREIGN KEY (a, b) REFERENCES {s}.parent(x, y));
             INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');
             INSERT INTO {s}.child VALUES (1, 'ox', {b});"
        )
    };
    conn.execute(&fixture("NULL")).await.expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    with_data(&mut parent, DataMode::Exact, &[]);
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('ZZ'::text)".into());
    child.columns.insert("a".into(), a);
    child.columns.insert("b".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // `b` is declared as stored — NULL — and left alone; `a` goes to the new
    // default, which no probe can evaluate.
    with_data(
        &mut child,
        DataMode::Exact,
        &[("1", row(&[("b", Value::Null)]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let (columns, unchanged) = cs
        .changes
        .iter()
        .find_map(|c| {
            if let pbps_model::Change::UpdateRow {
                columns, unchanged, ..
            } = &c.change
            {
                Some((columns, unchanged))
            } else {
                None
            }
        })
        .expect("the child row is updated");
    assert!(
        columns.contains_key("a") && !columns.contains_key("b") && unchanged.contains_key("b"),
        "`a` is written and `b` left alone: {columns:#?} {unchanged:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let refusals: Vec<_> = probes
        .iter()
        .filter(|p| p.description.contains("cannot evaluate"))
        .collect();
    assert!(
        !refusals.is_empty(),
        "the default is asked about: {probes:#?}"
    );
    for p in &refusals {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            0,
            "the NULL the update leaves alone is the tuple's: {}",
            p.sql
        );
    }
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        0,
        "both parent rows went, the row's tuple referencing nothing"
    );

    // The negative half: `b` left alone at a value is no NULL, and the
    // unevaluable `a` is refused.
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    fresh(&mut conn, &s).await;
    conn.execute(&fixture("'oy'")).await.expect("the fixture");
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("1", row(&[("b", Value::Text("oy".into()))]))],
    );
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the default is refused");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "a value left alone holds the tuple to the default: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// The same NULL, under a key this plan adds: the update's row is not one the
/// planned key can refuse for the default it cannot evaluate.
///
/// The planned key's own per-row decision reads the written cells, else the
/// column's backfill — and `b` has none, it is stored; the NULL the update
/// leaves the row holding is the one that decides (DECISIONS 349).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_null_an_update_leaves_alone_decides_for_a_key_this_plan_adds_too() {
    let mut conn = connect().await;
    let s = data_schema("heldnullkey");
    fresh(&mut conn, &s).await;
    let fixture = |b: &str| {
        format!(
            "CREATE TABLE {s}.parent (code text PRIMARY KEY, x text, y text, label text,
                 CONSTRAINT parent_xy UNIQUE (x, y));
             CREATE TABLE {s}.child (id integer PRIMARY KEY, a text DEFAULT 'ox', b text);
             INSERT INTO {s}.parent VALUES ('old', 'ox', 'oy', 'Old'), ('keep', 'kx', 'ky', 'Keep');
             INSERT INTO {s}.child VALUES (1, 'ox', {b});"
        )
    };
    conn.execute(&fixture("NULL")).await.expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["x", "y", "label"] {
        parent.columns.insert(c.into(), Column::new(ty("text")));
    }
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    parent.unique.insert(
        "parent_xy".into(),
        UniqueConstraint {
            columns: vec!["x".into(), "y".into()],
        },
    );
    with_data(
        &mut parent,
        DataMode::Exact,
        &[(
            "keep",
            row(&[
                ("x", Value::Text("kx".into())),
                ("y", Value::Text("ky".into())),
                ("label", Value::Text("Keep".into())),
            ]),
        )],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut a = Column::new(ty("text"));
    a.default = Some("lower('ZZ'::text)".into());
    child.columns.insert("a".into(), a);
    child.columns.insert("b".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ab".into(),
        ForeignKey {
            columns: vec!["a".into(), "b".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["x".into(), "y".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    with_data(
        &mut child,
        DataMode::Exact,
        &[("1", row(&[("b", Value::Null)]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::AddForeignKey { .. })),
        "the key is this plan's to add: {cs:#?}"
    );
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let planned: Vec<_> = probes
        .iter()
        .filter(|p| {
            p.description.contains("this plan adds") && p.description.contains("cannot evaluate")
        })
        .collect();
    for p in &planned {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            0,
            "the NULL the update leaves alone decides: {}",
            p.sql
        );
    }
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan runs whole, key and all"
    );

    // The negative half: `b` left alone at a value, and the planned key
    // refuses the row for the default it cannot evaluate.
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    fresh(&mut conn, &s).await;
    conn.execute(&fixture("'oy'")).await.expect("the fixture");
    with_data(
        declared.tables.get_mut(&child_name).expect("the child"),
        DataMode::Exact,
        &[("1", row(&[("b", Value::Text("oy".into()))]))],
    );
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| {
            p.description.contains("this plan adds") && p.description.contains("cannot evaluate")
        })
        .expect("the planned key refuses the row");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "a value left alone holds the tuple to the default: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A default spelled `CAST(NULL AS text)` is the NULL it is, and a row left
/// to it references nothing.
///
/// The engine deparses every cast as `…::type` — and a NULL default it does
/// not store at all (**measured**: no `pg_attrdef` row) — so this spelling
/// reaches the plan only from a declaration, verbatim. Read as an expression
/// no probe can evaluate, the update to it was refused, and with it the
/// ordinary plan that unpicks a reference before deleting its parent
/// (DECISIONS 350).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_default_cast_from_null_is_the_null_the_row_is_left_to() {
    let mut conn = connect().await;
    let s = data_schema("castnull");
    fresh(&mut conn, &s).await;
    let fixture = format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             ref text DEFAULT 'old',
             CONSTRAINT child_ref FOREIGN KEY (ref) REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');"
    );
    conn.execute(&fixture).await.expect("the fixture");
    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    let mut r = Column::new(ty("text"));
    r.default = Some("CAST(NULL AS text)".into());
    child.columns.insert("ref".into(), r);
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    // The row omits `ref`, so it goes to the declared default.
    with_data(&mut child, DataMode::Exact, &[("1", row(&[]))]);
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let update = cs
        .changes
        .iter()
        .find_map(|c| {
            if let pbps_model::Change::UpdateRow { columns, .. } = &c.change {
                Some(columns)
            } else {
                None
            }
        })
        .expect("the child row is updated to the default");
    assert!(
        matches!(update.get("ref"), Some((_, pbps_model::Cell::Default(d))) if d.contains("CAST")),
        "left to the declared spelling: {update:#?}"
    );

    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    for p in probes
        .iter()
        .filter(|p| p.description.contains("cannot evaluate"))
    {
        assert_eq!(
            counted(&mut conn, &p.sql).await,
            0,
            "a NULL, however spelled, is nothing to refuse: {}",
            p.sql
        );
    }
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`old`")
        })
        .expect("the count for the row the child references");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        0,
        "the row leaves the count through the NULL it is left to: {}",
        count.sql
    );
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(
            &mut conn,
            &format!("SELECT count(*)::int FROM {s}.child WHERE ref IS NULL")
        )
        .await,
        1,
        "the row is left at NULL and the parent row went"
    );

    // The negative half: a cast around an expression is still an expression,
    // and the row left to it is refused.
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    fresh(&mut conn, &s).await;
    conn.execute(&fixture).await.expect("the fixture");
    declared
        .tables
        .get_mut(&child_name)
        .expect("the child")
        .columns
        .get_mut("ref")
        .expect("ref")
        .default = Some("CAST(lower('ZZ') AS text)".into());
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let refusal = probes
        .iter()
        .find(|p| p.description.contains("cannot evaluate"))
        .expect("the expression is refused");
    assert_eq!(
        counted(&mut conn, &refusal.sql).await,
        1,
        "a cast around an expression is no literal: {}",
        refusal.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A default that is NULL is one this engine does not keep, in any spelling,
/// and the declaration is refused before a plan can restate it forever.
///
/// **Measured**: `SET DEFAULT NULL`, `NULL::text` and `CAST(NULL AS text)`
/// are all accepted, and every one of the three columns reads back with no
/// default at all — so the plan that set them is proposed again by the next
/// one, and the deploy's check of what came back, which compares whether a
/// default is there, refuses the column. `validate_table` names each such
/// column and what to declare instead (DECISIONS 351).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_null_default_is_one_this_engine_does_not_keep() {
    let mut conn = connect().await;
    let s = data_schema("nulldef");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.t (code text PRIMARY KEY,
             a text DEFAULT 'x', b text, c text DEFAULT 'y', d text DEFAULT 'z',
             e text DEFAULT 'w', kept text DEFAULT 'k',
             z timestamptz, i integer DEFAULT 7, p float8 DEFAULT 1.5,
             q text DEFAULT 'q', r text DEFAULT 'r', u integer DEFAULT 8,
             v text DEFAULT 'v', w integer DEFAULT 9);"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "t");
    let mut t = Table::default();
    t.columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for (column, default) in [
        ("a", "NULL"),
        ("b", "NULL::text"),
        ("c", "CAST(NULL AS text)"),
        ("d", "NULL /* note */::text"),
        ("e", "CAST(NULL AS text /* note */)"),
        ("kept", "'k'::text"),
        // A NULL of another type, or of the column's type with a modifier the
        // column lacks, is a default this engine keeps (DECISIONS 361).
        ("z", "NULL::timestamp(3) with time zone"),
        // A NULL of the column's own type under an alias is erased, and so
        // is one with a comment between the words of its type (DECISIONS 362).
        ("i", "NULL::int"),
        ("p", "CAST(NULL AS double /* note */ precision)"),
        // Comments as the gaps around `AS`, and a line comment ending at a
        // carriage return (DECISIONS 363).
        ("q", "CAST(NULL/**/AS -- note\r text)"),
        // The column's own type qualified, quoted, spaced and folded is
        // still its own type (DECISIONS 364).
        ("r", "CAST(NULL AS \"pg_catalog\" . \"text\")"),
        ("u", "NULL::PG_CATALOG.INT4"),
        // A Unicode-escaped type name is the name it spells (DECISIONS 369).
        ("v", "NULL::U&\"te!0078t\" UESCAPE '!'"),
        ("w", "CAST(NULL AS U&\"int\\0034\")"),
    ] {
        let mut c = Column::new(ty(match column {
            "z" => "timestamptz",
            "i" | "u" | "w" => "integer",
            "p" => "double precision",
            _ => "text",
        }));
        c.default = Some(default.into());
        t.columns.insert(column.into(), c);
    }
    t.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    let pg = Postgres::new();
    let mut refused: Vec<String> = pg
        .validate_table(&name, &t)
        .iter()
        .map(|e| {
            let m = e.to_string();
            assert!(m.contains("declare no default"), "{m}");
            m.split('`').nth(1).expect("a column name").to_owned()
        })
        .collect();
    refused.sort();
    assert_eq!(
        refused,
        ["a", "b", "c", "d", "e", "i", "p", "q", "r", "u", "v", "w"],
        "each erased spelling, and only those"
    );

    // The measurement the refusal rests on: past the validator, the plan
    // sets all three, the engine accepts all three, and none is there.
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let set: Vec<&String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::AlterColumnDefault {
                column,
                to: Some(_),
                ..
            } = &c.change
            {
                Some(&column.name)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        set.len(),
        13,
        "the twelve erased NULL defaults and the kept one are set: {cs:#?}"
    );
    apply(&mut conn, &pg, &cs).await;
    let base = connected_base(&mut conn, &declared, &s).await;
    let after = base.tables.get(&name).expect("the table");
    for column in ["a", "b", "c", "d", "e", "i", "p", "q", "r", "u", "v", "w"] {
        assert_eq!(
            after.columns[column].default, None,
            "`{column}` reads back with no default at all"
        );
    }
    assert_eq!(
        after.columns["kept"].default.as_deref(),
        Some("'k'::text"),
        "a default that is a value is kept"
    );
    assert_eq!(
        after.columns["z"].default.as_deref(),
        Some("NULL::timestamp(3) with time zone"),
        "a NULL of a type the column is not is kept, as declared"
    );
    let again = plan(&base, &ids, &declared, &ids);
    assert_eq!(
        again
            .changes
            .iter()
            .filter(|c| matches!(c.change, pbps_model::Change::AlterColumnDefault { .. }))
            .count(),
        12,
        "and the next plan sets the erased ones again, and only those: {again:#?}"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A child collated differently from the column it references is counted
/// under the referenced column's collation, as the engine's own check
/// compares it.
///
/// **Measured**: two columns each collated explicitly and differently — the
/// parent `"C"`, the child `"en_US.utf8"` — are a key the engine accepts and
/// enforces, the delete refused; and `p.code = ch.ref` between them fails as
/// soon as a row is compared, `could not determine which collation to use`,
/// which the probe runner reads as unchecked. (A default-collated side takes
/// the other's collation and never conflicts, so a managed parent has to be
/// collated itself, which the pull notes and still manages.) The count now spells the parent side
/// `COLLATE` the referenced column's collation and compares through the
/// operator the constraint records (DECISIONS 352).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_child_collated_differently_from_its_parent_is_still_counted() {
    let mut conn = connect().await;
    let s = data_schema("collated");
    let other = format!("{s}_other");
    fresh(&mut conn, &s).await;
    fresh(&mut conn, &other).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text COLLATE \"C\" PRIMARY KEY, label text);
         CREATE TABLE {other}.child (id integer PRIMARY KEY,
             ref text COLLATE \"en_US.utf8\" REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {other}.child VALUES (1, 'old');"
    ))
    .await
    .expect("the fixture: the engine accepts the key across collations");
    // The hazard by hand: the plain comparison is refused, the delete is not
    // accepted.
    let plain = match conn
        .query(&format!(
            "SELECT count(*) FROM {other}.child ch, {s}.parent p WHERE p.code = ch.ref"
        ))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("two implicit collations cannot be compared"),
    };
    assert_eq!(sqlstate(&plain), "42P22", "{plain:?}");
    let refused = conn
        .execute(&format!("DELETE FROM {s}.parent WHERE code = 'old'"))
        .await
        .expect_err("the key is enforced");
    assert_eq!(sqlstate(&refused), "23503", "{refused:?}");

    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    table
        .columns
        .insert("label".into(), Column::new(ty("text")));
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut table,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`old`")
        })
        .expect("the count for the row the child references");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        1,
        "the child is counted under the referenced column's collation: {}",
        count.sql
    );

    // The negative half: the child moved to the surviving parent, the same
    // comparison finds nothing and the delete is valid.
    conn.execute(&format!("UPDATE {other}.child SET ref = 'keep'"))
        .await
        .expect("move the child");
    assert_eq!(counted(&mut conn, &count.sql).await, 0, "{}", count.sql);
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan runs whole"
    );
    conn.execute(&format!(
        "DROP SCHEMA {other} CASCADE; DROP SCHEMA {s} CASCADE"
    ))
    .await
    .expect("drop");
}

/// A parent row this plan inserts and a child row it inserts meet under the
/// referenced column's collation, as the engine's own check compares them.
///
/// **Measured** on 18.6: with both columns under one nondeterministic,
/// case-insensitive collation, a child `'A'` inserted beside a parent `'a'`
/// references it, and the key is added once the stored `'A'` parent is gone;
/// `'a'::text = 'A'::text` on its own is false, the two literals comparing
/// under the database's collation. Compared as the literals they were, the
/// arriving child was attributed to the doomed row and the plan refused
/// (DECISIONS 366). Under `"C"`, the same plan's child references nothing,
/// and is counted.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn an_inserted_survivor_meets_an_arriving_child_under_the_referenced_collation() {
    let mut conn = connect().await;
    let s = data_schema("cisurv");
    let declare = |s: &str| {
        let parent_name = TableName::new(s, "parent");
        let mut parent = Table::default();
        parent
            .columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        parent
            .columns
            .insert("code".into(), Column::new(ty("text")));
        parent.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        parent.unique.insert(
            "parent_code".into(),
            UniqueConstraint {
                columns: vec!["code".into()],
            },
        );
        with_data(
            &mut parent,
            DataMode::Exact,
            &[("2", row(&[("code", Value::Text("a".into()))]))],
        );
        let child_name = TableName::new(s, "child");
        let mut child = Table::default();
        child
            .columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        child.columns.insert("ref".into(), Column::new(ty("text")));
        child.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        child.foreign_keys.insert(
            "child_ref".into(),
            ForeignKey {
                columns: vec!["ref".into()],
                references_table: parent_name.clone(),
                references_columns: vec!["code".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        with_data(
            &mut child,
            DataMode::Exact,
            &[("2", row(&[("ref", Value::Text("A".into()))]))],
        );
        let mut declared = Schema::default();
        declared.tables.insert(parent_name, parent);
        declared.tables.insert(child_name, child);
        declared
    };
    let pg = Postgres::new();
    let ci = format!("{s}.ci");
    for (collation, references_the_survivor) in [(ci.as_str(), true), ("\"C\"", false)] {
        fresh(&mut conn, &s).await;
        conn.execute(&format!(
            "CREATE COLLATION {s}.ci (provider = icu, locale = 'und-u-ks-level2', \
                 deterministic = false);
             CREATE TABLE {s}.parent (id integer PRIMARY KEY, code text COLLATE {collation});
             CREATE TABLE {s}.child (id integer PRIMARY KEY, ref text COLLATE {collation});
             INSERT INTO {s}.parent VALUES (1, 'A');"
        ))
        .await
        .expect("the fixture");
        let declared = declare(&s);
        let ids = mint_ids(&declared, &IdsFile::default(), &[]);
        let base = connected_base(&mut conn, &declared, &s).await;
        let cs = plan(&base, &ids, &declared, &ids);
        assert!(
            cs.changes
                .iter()
                .any(|c| matches!(c.change, pbps_model::Change::AddForeignKey { .. })),
            "the key is this plan's to add: {cs:#?}"
        );
        let probes = pg.preflight(&cs);
        let planned = probes
            .iter()
            .find(|p| p.description.contains("on a column it also adds"))
            .expect("the planned key is counted");
        assert!(
            planned.sql.contains("a.attcollation <> 0") && !planned.sql.contains('\u{1}'),
            "{}",
            planned.sql
        );
        assert_eq!(
            counted(&mut conn, &planned.sql).await,
            if references_the_survivor { 0 } else { 1 },
            "under {collation}: {}",
            planned.sql
        );
        if references_the_survivor {
            apply(&mut conn, &pg, &cs).await;
            assert_eq!(
                number(
                    &mut conn,
                    &format!(
                        "SELECT p.id FROM {s}.child c JOIN {s}.parent p ON p.code = c.ref \
                         WHERE c.id = 2"
                    )
                )
                .await,
                2,
                "the plan runs whole, and the child is keyed to the inserted row"
            );
        }
        conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
            .await
            .expect("drop");
    }
}

/// A key this plan adds between two stored columns collated differently is
/// counted under the referenced column's collation, as the engine will
/// enforce it.
///
/// **Measured**: `ADD CONSTRAINT … FOREIGN KEY` between a parent `"C"` and a
/// child `"en_US.utf8"` is accepted, and the plain comparison between the two
/// fails the moment a row is compared. The planned key's count is assembled
/// by the engine, and the referenced column's collation is spliced in from
/// `pg_attribute` where a stored parent column meets a stored child column
/// (DECISIONS 353).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_planned_key_across_collations_is_counted_under_the_referenced_collation() {
    let mut conn = connect().await;
    let s = data_schema("plancoll");
    fresh(&mut conn, &s).await;
    conn.execute(&format!(
        "CREATE TABLE {s}.parent (code text COLLATE \"C\" PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY, ref text COLLATE \"en_US.utf8\");
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'old');"
    ))
    .await
    .expect("the fixture");
    let plain = match conn
        .query(&format!(
            "SELECT count(*) FROM {s}.child ch, {s}.parent p WHERE p.code = ch.ref"
        ))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("two implicit collations cannot be compared"),
    };
    assert_eq!(sqlstate(&plain), "42P22", "{plain:?}");

    let parent_name = TableName::new(&s, "parent");
    let mut parent = Table::default();
    parent
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    parent
        .columns
        .insert("label".into(), Column::new(ty("text")));
    parent.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    with_data(
        &mut parent,
        DataMode::Exact,
        &[("keep", row(&[("label", Value::Text("Keep".into()))]))],
    );
    let child_name = TableName::new(&s, "child");
    let mut child = Table::default();
    child
        .columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    child.columns.insert("ref".into(), Column::new(ty("text")));
    child.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".into()],
    });
    child.foreign_keys.insert(
        "child_ref".into(),
        ForeignKey {
            columns: vec!["ref".into()],
            references_table: parent_name.clone(),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut declared = Schema::default();
    declared.tables.insert(parent_name.clone(), parent);
    declared.tables.insert(child_name.clone(), child);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    assert!(
        cs.changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::AddForeignKey { .. })),
        "the key is this plan's to add: {cs:#?}"
    );
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert!(
        planned.sql.contains("a.attcollation <> 0") && !planned.sql.contains('\u{1}'),
        "{}",
        planned.sql
    );
    assert_eq!(
        counted(&mut conn, &planned.sql).await,
        1,
        "the child is counted under the referenced column's collation: {}",
        planned.sql
    );

    // The negative half: the child moved to the survivor, and the plan runs
    // whole, key and all.
    conn.execute(&format!("UPDATE {s}.child SET ref = 'keep'"))
        .await
        .expect("move the child");
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    let planned = probes
        .iter()
        .find(|p| p.description.contains("on a column it also adds"))
        .expect("the planned key is counted");
    assert_eq!(counted(&mut conn, &planned.sql).await, 0, "{}", planned.sql);
    apply(&mut conn, &pg, &cs).await;
    assert_eq!(
        number(&mut conn, &format!("SELECT count(*)::int FROM {s}.parent")).await,
        1,
        "the plan runs whole"
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A parent whose referenced columns the session cannot read is one whose
/// children it cannot count, whatever it may read on the children.
///
/// **Measured**: with `DELETE` on the parent and `SELECT` on its key column
/// alone, the count of children through a key into another column fails
/// `permission denied for table`, the delete runs, and `ON DELETE CASCADE`
/// takes the child the count never saw. The refusal now asks the parent's
/// side too; granting the read is the negative half (DECISIONS 354).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_parent_whose_referenced_columns_the_session_cannot_read_refuses_the_delete() {
    let mut conn = connect().await;
    let s = data_schema("noparentread");
    fresh(&mut conn, &s).await;
    let role = format!("{s}_dep");
    conn.execute(&format!("DROP OWNED BY {role}")).await.ok();
    conn.execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .ok();
    conn.execute(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'x';
         GRANT USAGE ON SCHEMA {s} TO {role};
         CREATE TABLE {s}.parent (code text PRIMARY KEY, alt text UNIQUE, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             ref text REFERENCES {s}.parent(alt) ON DELETE CASCADE);
         INSERT INTO {s}.parent VALUES ('old', 'A', 'Old'), ('keep', 'B', 'Keep');
         INSERT INTO {s}.child VALUES (1, 'A');
         GRANT DELETE ON {s}.parent TO {role};
         GRANT SELECT (code) ON {s}.parent TO {role};
         GRANT SELECT ON {s}.child TO {role};"
    ))
    .await
    .expect("the fixture");
    let name = TableName::new(&s, "parent");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty("text")).not_null());
    for c in ["alt", "label"] {
        table.columns.insert(c.into(), Column::new(ty("text")));
    }
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
    });
    table.unique.insert(
        "parent_alt_key".into(),
        UniqueConstraint {
            columns: vec!["alt".into()],
        },
    );
    with_data(
        &mut table,
        DataMode::Exact,
        &[(
            "keep",
            row(&[
                ("alt", Value::Text("B".into())),
                ("label", Value::Text("Keep".into())),
            ]),
        )],
    );
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), table);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let pg = Postgres::new();
    let probes = pg.preflight(&cs);
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`old`")
        })
        .expect("the count");
    let refusal = probes
        .iter()
        .find(|p| p.description.starts_with("tables with a foreign key into"))
        .expect("the refusal");
    let mut deployer = Conn::connect(Driver::Postgres, &as_role(&role, "x"))
        .await
        .expect("connect as the deploying role");
    let denied = match deployer.query(&count.sql).await {
        Err(e) => e,
        Ok(_) => panic!("the role cannot read the referenced column"),
    };
    assert_eq!(sqlstate(&denied), "42501", "{denied:?}");
    assert_eq!(
        counted(&mut deployer, &refusal.sql).await,
        1,
        "the parent's own side is asked: {}",
        refusal.sql
    );
    // With the referenced column readable the count runs and finds the
    // child, and the refusal has nothing to say.
    conn.execute(&format!("GRANT SELECT (alt) ON {s}.parent TO {role}"))
        .await
        .expect("grant the read");
    assert_eq!(counted(&mut deployer, &count.sql).await, 1);
    assert_eq!(counted(&mut deployer, &refusal.sql).await, 0);
    drop(deployer);
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
    conn.execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .expect("drop the role");
}

/// A default spelled as an escape string, a Unicode string or a dollar-quoted
/// string is the literal it is, and a row left to it is compared by it.
///
/// The catalog deparses every one of them to `'…'::text`, so the spelling
/// reaches the plan only from a declaration, verbatim. Read as an expression
/// no probe can evaluate, the update to it was refused; read as the literal,
/// the row's tuple after the update is compared like any other
/// (DECISIONS 355).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_default_spelled_as_an_escape_string_is_the_literal_it_is() {
    let mut conn = connect().await;
    let s = data_schema("escdef");
    fresh(&mut conn, &s).await;
    let fixture = format!(
        "CREATE TABLE {s}.parent (code text PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             ref text DEFAULT 'old',
             CONSTRAINT child_ref FOREIGN KEY (ref) REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES ('old', 'Old'), ('keep', 'Keep'), ('it''s', 'Its');
         INSERT INTO {s}.child VALUES (1, 'old');"
    );
    let parent_name = TableName::new(&s, "parent");
    let child_name = TableName::new(&s, "child");
    let declare = |default: &str| {
        let mut parent = Table::default();
        parent
            .columns
            .insert("code".into(), Column::new(ty("text")).not_null());
        parent
            .columns
            .insert("label".into(), Column::new(ty("text")));
        parent.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["code".into()],
        });
        with_data(
            &mut parent,
            DataMode::Exact,
            &[
                ("keep", row(&[("label", Value::Text("Keep".into()))])),
                ("it's", row(&[("label", Value::Text("Its".into()))])),
            ],
        );
        let mut child = Table::default();
        child
            .columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        let mut r = Column::new(ty("text"));
        r.default = Some(default.into());
        child.columns.insert("ref".into(), r);
        child.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        child.foreign_keys.insert(
            "child_ref".into(),
            ForeignKey {
                columns: vec!["ref".into()],
                references_table: parent_name.clone(),
                references_columns: vec!["code".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        // The row omits `ref`, so it goes to the declared default.
        with_data(&mut child, DataMode::Exact, &[("1", row(&[]))]);
        let mut declared = Schema::default();
        declared.tables.insert(parent_name.clone(), parent);
        declared.tables.insert(child_name.clone(), child);
        declared
    };
    let pg = Postgres::new();
    // Every spelling, left to the surviving parent: nothing is refused, the
    // count finds nothing, and the plan applies whole.
    for (default, lands_on) in [
        ("E'keep'", "keep"),
        ("U&'keep'", "keep"),
        ("$$keep$$", "keep"),
        ("$q$keep$q$", "keep"),
        ("U&'keep' UESCAPE '!'", "keep"),
        ("U&'ke!0065p' UESCAPE '!'", "keep"),
        // An escaped quote inside, and a cast around it, in both spellings
        // of a cast; a dollar-quoted string whose body holds a quote
        // (DECISIONS 357).
        ("E'it\\'s'::text", "it's"),
        ("CAST(E'it\\'s' AS text)", "it's"),
        ("$$it's$$::text", "it's"),
        // A string, a `)` or a quoted identifier ends its token without a
        // gap before `AS`, and a quoted type needs none after (DECISIONS 364).
        ("CAST('keep'AS text)", "keep"),
        ("CAST($$keep$$AS\"text\")", "keep"),
        ("CAST(('keep')AS pg_catalog.text)", "keep"),
        // A typed literal, in the spellings of its string and of its type
        // (DECISIONS 367).
        ("TEXT 'keep'", "keep"),
        ("text'keep'", "keep"),
        ("pg_catalog.text E'it\\'s'", "it's"),
        ("\"text\" /* c */ $$keep$$", "keep"),
        ("VARCHAR(10) U&'k!0065ep' UESCAPE '!'", "keep"),
    ] {
        conn.execute(&fixture).await.expect("the fixture");
        let declared = declare(default);
        let ids = mint_ids(&declared, &IdsFile::default(), &[]);
        let base = connected_base(&mut conn, &declared, &s).await;
        let cs = plan(&base, &ids, &declared, &ids);
        assert!(
            cs.changes
                .iter()
                .any(|c| matches!(&c.change, pbps_model::Change::UpdateRow { columns, .. }
                    if matches!(columns.get("ref"), Some((_, pbps_model::Cell::Default(d))) if d == default))),
            "{default}: the row is left to the declared spelling: {cs:#?}"
        );
        let probes = pg.preflight(&cs);
        for p in probes
            .iter()
            .filter(|p| p.description.contains("cannot evaluate"))
        {
            assert_eq!(
                counted(&mut conn, &p.sql).await,
                0,
                "{default} is a literal, nothing to refuse: {}",
                p.sql
            );
        }
        let count = probes
            .iter()
            .find(|p| {
                p.description.starts_with("rows in other tables") && p.description.contains("`old`")
            })
            .expect("the count for the row the child references");
        assert_eq!(
            counted(&mut conn, &count.sql).await,
            0,
            "{default}: {}",
            count.sql
        );
        apply(&mut conn, &pg, &cs).await;
        assert_eq!(
            number(
                &mut conn,
                &format!(
                    "SELECT count(*)::int FROM {s}.child WHERE ref = '{}'",
                    lands_on.replace('\'', "''")
                )
            )
            .await,
            1,
            "{default}: the row is left at the literal and the parent row went"
        );
        conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
            .await
            .expect("drop");
        fresh(&mut conn, &s).await;
    }

    // The negative half: the same spelling naming the doomed row is compared
    // as the literal it is, and the count finds the row.
    conn.execute(&fixture).await.expect("the fixture");
    let declared = declare("E'old'");
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    assert!(
        !probes
            .iter()
            .any(|p| p.description.contains("cannot evaluate")),
        "a literal is never refused: {probes:#?}"
    );
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`old`")
        })
        .expect("the count for the row the child references");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        1,
        "the row left to E'old' still references it: {}",
        count.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

/// A default spelled as a number in any form this engine reads — underscores
/// between digits, a hexadecimal, octal or binary prefix — is the constant it
/// is, and a row left to it is compared by it.
///
/// **Measured** on 18.6: `2_55`, `0xFF`, `0o377` and `0b11111111` are each
/// 255 to the engine, and so are `+ 255`, `- -255` and `+-(-255)`. Read as
/// an expression no probe can evaluate, the update to such a default was
/// refused; read as the number, the row's tuple after the update is compared
/// like any other (DECISIONS 360, 365).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
async fn a_default_spelled_as_a_number_in_any_base_is_the_constant_it_is() {
    let mut conn = connect().await;
    let s = data_schema("numdef");
    fresh(&mut conn, &s).await;
    let fixture = format!(
        "CREATE TABLE {s}.parent (code integer PRIMARY KEY, label text);
         CREATE TABLE {s}.child (id integer PRIMARY KEY,
             ref integer DEFAULT 1,
             CONSTRAINT child_ref FOREIGN KEY (ref) REFERENCES {s}.parent(code));
         INSERT INTO {s}.parent VALUES (1, 'One'), (255, 'Max');
         INSERT INTO {s}.child VALUES (1, 1);"
    );
    let parent_name = TableName::new(&s, "parent");
    let child_name = TableName::new(&s, "child");
    let declare = |default: &str| {
        let mut parent = Table::default();
        parent
            .columns
            .insert("code".into(), Column::new(ty("integer")).not_null());
        parent
            .columns
            .insert("label".into(), Column::new(ty("text")));
        parent.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["code".into()],
        });
        with_data(
            &mut parent,
            DataMode::Exact,
            &[("255", row(&[("label", Value::Text("Max".into()))]))],
        );
        let mut child = Table::default();
        child
            .columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        let mut r = Column::new(ty("integer"));
        r.default = Some(default.into());
        child.columns.insert("ref".into(), r);
        child.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        child.foreign_keys.insert(
            "child_ref".into(),
            ForeignKey {
                columns: vec!["ref".into()],
                references_table: parent_name.clone(),
                references_columns: vec!["code".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        with_data(&mut child, DataMode::Exact, &[("1", row(&[]))]);
        let mut declared = Schema::default();
        declared.tables.insert(parent_name.clone(), parent);
        declared.tables.insert(child_name.clone(), child);
        declared
    };
    let pg = Postgres::new();
    for default in [
        "2_55",
        "0xFF",
        "0o377",
        "0b11111111",
        "0xF_F",
        // A sign parted from its operand by a gap, a comment, a grouping or
        // another sign (DECISIONS 365).
        "+ 255",
        "- -255",
        "+ /* c */ 0xFF",
        "-(-2_55)",
        "+-(-255)",
        "-\n-\n255",
        // A typed literal of a number, and one under a sign (DECISIONS 367).
        "INTEGER '255'",
        "int4 E'255'",
        "- NUMERIC(5, 0) '-255'",
    ] {
        conn.execute(&fixture).await.expect("the fixture");
        let declared = declare(default);
        let ids = mint_ids(&declared, &IdsFile::default(), &[]);
        let base = connected_base(&mut conn, &declared, &s).await;
        let cs = plan(&base, &ids, &declared, &ids);
        let probes = pg.preflight(&cs);
        assert!(
            !probes
                .iter()
                .any(|p| p.description.contains("cannot evaluate")),
            "{default} is a number, nothing to refuse: {probes:#?}"
        );
        let count = probes
            .iter()
            .find(|p| {
                p.description.starts_with("rows in other tables") && p.description.contains("`1`")
            })
            .expect("the count for the row the child references");
        assert_eq!(
            counted(&mut conn, &count.sql).await,
            0,
            "{default}: {}",
            count.sql
        );
        apply(&mut conn, &pg, &cs).await;
        assert_eq!(
            number(
                &mut conn,
                &format!("SELECT count(*)::int FROM {s}.child WHERE ref = 255")
            )
            .await,
            1,
            "{default}: the row is left at 255 and parent 1 went"
        );
        conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
            .await
            .expect("drop");
        fresh(&mut conn, &s).await;
    }

    // The negative half: the same spelling naming the doomed row is compared
    // as the number it is, and the count finds the row.
    conn.execute(&fixture).await.expect("the fixture");
    let declared = declare("0x1");
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let base = connected_base(&mut conn, &declared, &s).await;
    let cs = plan(&base, &ids, &declared, &ids);
    let probes = pg.preflight(&cs);
    assert!(
        !probes
            .iter()
            .any(|p| p.description.contains("cannot evaluate")),
        "a number is never refused: {probes:#?}"
    );
    let count = probes
        .iter()
        .find(|p| {
            p.description.starts_with("rows in other tables") && p.description.contains("`1`")
        })
        .expect("the count for the row the child references");
    assert_eq!(
        counted(&mut conn, &count.sql).await,
        1,
        "the row left to 0x1 still references 1: {}",
        count.sql
    );
    conn.execute(&format!("DROP SCHEMA {s} CASCADE"))
        .await
        .expect("drop");
}

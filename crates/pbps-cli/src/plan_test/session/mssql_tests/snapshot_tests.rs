//! What a verified re-pin reads from an owned SQL Server database, as the apply
//! login (ADR-0019 §5): rows as typed text cells, and every allocator's next
//! value by really using it. Each case here is a way the obvious read is wrong.
use super::*;
use pbps_mssql::test_scratch::snapshot::{AllocatorRead, TableRead, read_allocators, read_table};

async fn apply_connection(session: &Session<'_>) -> anyhow::Result<Conn> {
    Ok(Conn::connect(Driver::Mssql, session.target().connection()).await?)
}

async fn run(conn: &mut Conn, sql: &str) -> anyhow::Result<()> {
    Ok(conn.execute(sql).await?)
}

fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
    cells.iter().map(|c| c.map(str::to_owned)).collect()
}

fn count_of(read: &TableRead, cells: &[Option<&str>]) -> u64 {
    read.rows.get(&row(cells)).copied().unwrap_or(0)
}

fn next_of(all: &[AllocatorRead], name: &str) -> String {
    all.iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("no allocator {name}: {:?}", names(all)))
        .next
        .clone()
}

fn names(all: &[AllocatorRead]) -> Vec<&str> {
    all.iter().map(|a| a.name.as_str()).collect()
}

/// The error text of a read that must be refused, so a pass cannot be a
/// different failure.
async fn refused_table(conn: &mut Conn, schema: &str, table: &str) -> anyhow::Result<String> {
    match read_table(conn, schema, table).await {
        Ok(_) => anyhow::bail!("{schema}.{table} was read"),
        Err(error) => Ok(error.to_string()),
    }
}

async fn refused_allocators(conn: &mut Conn) -> anyhow::Result<String> {
    match read_allocators(conn).await {
        Ok(all) => anyhow::bail!("allocators were read: {:?}", names(&all)),
        Err(error) => Ok(error.to_string()),
    }
}

pub(super) async fn exercise_snapshot(session: &Session<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let base = fixture.input(&IdsFile::default(), false, false)?;
    bootstrap(session, &base, &mut Phase::new("bootstrap", "base")).await?;
    let mut conn = apply_connection(session).await?;
    rows(&mut conn).await?;
    refusals(&mut conn).await?;
    allocators(&mut conn).await?;
    Ok(())
}

async fn rows(conn: &mut Conn) -> anyhow::Result<()> {
    // CREATE SCHEMA must be alone in its batch.
    run(conn, "CREATE SCHEMA snap").await?;
    run(
        conn,
        "CREATE TABLE snap.words (w nvarchar(20) NULL, n int NOT NULL)",
    )
    .await?;
    run(
        conn,
        "INSERT snap.words VALUES (N'a', 1), (N'A', 1), (N'a ', 1), (N'', 1), (NULL, 1), \
         (N'dup', 2), (N'dup', 2)",
    )
    .await?;
    let read = read_table(conn, "snap", "words").await?;
    assert_eq!(read.columns, ["n", "w"], "columns sort by name");
    // A case-insensitive, trailing-space-blind GROUP BY would merge the first
    // three; the multiset keeps every one of them.
    for w in [Some("a"), Some("A"), Some("a "), Some(""), None] {
        assert_eq!(count_of(&read, &[Some("1"), w]), 1, "{w:?}");
    }
    assert_eq!(count_of(&read, &[Some("2"), Some("dup")]), 2);
    assert_eq!(read.rows.len(), 6);

    // Every supported type reads as text that does not depend on the session.
    run(
        conn,
        "CREATE TABLE snap.typed (\
           i bigint, d decimal(10,3), f float, r real, m money, b bit, \
           dt datetime2(7), dto datetimeoffset(3), da date, ti time(3), sdt smalldatetime, \
           g uniqueidentifier, bin varbinary(8), x xml, c char(5), t text, nt ntext, \
           h hierarchyid, geo geography)",
    )
    .await?;
    run(
        conn,
        "INSERT snap.typed VALUES (\
           -9223372036854775808, 1234567.125, 0.1, 0.1, 922337203685477.5807, 1, \
           '2024-02-29T23:59:59.1234567', '2024-02-29T23:59:59.123+05:30', '2024-02-29', \
           '23:59:59.123', '2024-02-29T23:59:00', '6F9619FF-8B86-D011-B42D-00C04FC964FF', \
           0x00FF10, N'<a b=\"1\"/>', 'ab', 'text', N'ntext', '/1/2/', \
           geography::STGeomFromText('POINT(1 2)', 4326)), \
           (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
            NULL, NULL, NULL, NULL, NULL)",
    )
    .await?;
    let typed = read_table(conn, "snap", "typed").await?;
    let first = typed
        .rows
        .keys()
        .find(|cells| cells[0].is_some())
        .expect("the row with values");
    // Columns are sorted by name: b, bin, c, d, da, dt, dto, f, g, geo, h, i, m, nt, r, sdt, t, ti, x.
    let want = [
        "1",
        "0x00FF10",
        "ab   ",
        "1234567.125",
        "2024-02-29",
        "2024-02-29 23:59:59.1234567",
        "2024-02-29 23:59:59.123 +05:30",
        "1.0000000000000001e-001",
        "6F9619FF-8B86-D011-B42D-00C04FC964FF",
        "0xE6100000010C0000000000000040000000000000F03F",
        "0x5B40",
        "-9223372036854775808",
        "922337203685477.5807",
        "ntext",
        "1.0000000149011612e-001",
        "2024-02-29 23:59:00.000",
        "text",
        "23:59:59.123",
        "<a b=\"1\"/>",
    ];
    let got: Vec<&str> = first.iter().map(|c| c.as_deref().unwrap()).collect();
    assert_eq!(got, want);
    assert!(
        typed
            .rows
            .keys()
            .any(|cells| cells.iter().all(Option::is_none)),
        "an all-NULL row stays NULL in every column"
    );
    Ok(())
}

async fn refusals(conn: &mut Conn) -> anyhow::Result<()> {
    run(
        conn,
        "CREATE TABLE snap.versioned (id int NOT NULL, v rowversion)",
    )
    .await?;
    let text = refused_table(conn, "snap", "versioned").await?;
    assert!(
        text.contains("snap.versioned.v") && text.contains("generated by the engine"),
        "{text}"
    );
    run(
        conn,
        "CREATE TABLE snap.variant (id int NOT NULL, v sql_variant)",
    )
    .await?;
    let text = refused_table(conn, "snap", "variant").await?;
    assert!(
        text.contains("snap.variant.v") && text.contains("sql_variant"),
        "{text}"
    );
    run(
        conn,
        "CREATE TABLE snap.temporal (id int NOT NULL PRIMARY KEY, \
           s datetime2 GENERATED ALWAYS AS ROW START NOT NULL, \
           e datetime2 GENERATED ALWAYS AS ROW END NOT NULL, \
           PERIOD FOR SYSTEM_TIME (s, e)) \
         WITH (SYSTEM_VERSIONING = ON)",
    )
    .await?;
    let text = refused_table(conn, "snap", "temporal").await?;
    assert!(text.contains("snap.temporal.s"), "{text}");
    // Absent is not empty.
    let text = refused_table(conn, "snap", "missing").await?;
    assert!(text.contains("no readable columns"), "{text}");
    // A name with every quote in it still reads.
    run(conn, "CREATE SCHEMA [we'ird]]s]").await?;
    run(
        conn,
        "CREATE TABLE [we'ird]]s].[t]]'x] ([c]]'] int); INSERT [we'ird]]s].[t]]'x] VALUES (7)",
    )
    .await?;
    let read = read_table(conn, "we'ird]s", "t]'x").await?;
    assert_eq!(read.columns, ["c]'"]);
    assert_eq!(count_of(&read, &[Some("7")]), 1);
    // Cleanup of what the next section must not see.
    run(
        conn,
        "ALTER TABLE snap.temporal SET (SYSTEM_VERSIONING = OFF); \
         DROP TABLE snap.temporal; DROP TABLE snap.versioned; DROP TABLE snap.variant",
    )
    .await?;
    Ok(())
}

async fn allocators(conn: &mut Conn) -> anyhow::Result<()> {
    run(
        conn,
        "CREATE TABLE snap.fresh (id int IDENTITY(10,5) PRIMARY KEY, name nvarchar(10) NOT NULL); \
         CREATE TABLE snap.used (id int IDENTITY(10,5) PRIMARY KEY, name nvarchar(10) NOT NULL); \
         INSERT snap.used(name) VALUES (N'a'), (N'b'); DELETE snap.used WHERE id = 15; \
         CREATE TABLE snap.never (id int IDENTITY(1,1), x int NULL); \
         CREATE TABLE snap.once (id int IDENTITY(1,1), x int NULL); \
         INSERT snap.once(x) VALUES (1); \
         DBCC CHECKIDENT('snap.never', RESEED, 50) WITH NO_INFOMSGS; \
         DBCC CHECKIDENT('snap.once', RESEED, 50) WITH NO_INFOMSGS; \
         CREATE TABLE snap.audit (n int IDENTITY(1,1), note nvarchar(20)); \
         CREATE TABLE snap.watched (id int IDENTITY(1,1) PRIMARY KEY, x int NULL)",
    )
    .await?;
    run(
        conn,
        "CREATE TRIGGER snap.watch ON snap.watched AFTER INSERT AS \
         INSERT snap.audit(note) VALUES (N'ins')",
    )
    .await?;
    run(
        conn,
        "CREATE SEQUENCE snap.codes START WITH 5 INCREMENT BY 3; \
         CREATE SEQUENCE snap.shared START WITH 1; \
         CREATE TABLE snap.s1 (id int NOT NULL DEFAULT (NEXT VALUE FOR snap.codes), \
                               a int NOT NULL DEFAULT (NEXT VALUE FOR snap.shared)); \
         CREATE TABLE snap.s2 (a int NOT NULL DEFAULT (NEXT VALUE FOR snap.shared))",
    )
    .await?;
    let rows_before = count(
        conn,
        "SELECT (SELECT COUNT(*) FROM snap.watched) + (SELECT COUNT(*) FROM snap.audit) \
         + (SELECT COUNT(*) FROM snap.fresh) AS n",
    )
    .await?;
    let all = read_allocators(conn).await?;
    // The value the engine hands out, not IDENT_CURRENT + 1: a column reseeded
    // before it ever generated a value hands out the reseed, one that had does not.
    assert_eq!(next_of(&all, "snap.fresh.id (identity)"), "10");
    assert_eq!(next_of(&all, "snap.used.id (identity)"), "20");
    assert_eq!(next_of(&all, "snap.never.id (identity)"), "50");
    assert_eq!(next_of(&all, "snap.once.id (identity)"), "51");
    assert_eq!(next_of(&all, "snap.watched.id (identity)"), "1");
    // The trigger on `watched` writes to `audit` during `watched`'s probe, after
    // `audit`'s own; the value reported is from the state before any probe.
    assert_eq!(next_of(&all, "snap.audit.n (identity)"), "1");
    assert_eq!(next_of(&all, "snap.codes"), "5");
    // A sequence one column fills is compared under it; two columns share one.
    let codes = all.iter().find(|a| a.name == "snap.codes").unwrap();
    assert_eq!(codes.used_by.as_deref(), Some("snap.s1.id"));
    let shared = all.iter().find(|a| a.name == "snap.shared").unwrap();
    assert_eq!(shared.used_by, None);
    // The ledger's own allocators are not compared.
    assert!(
        all.iter().all(|a| !a.name.contains("__pbps_")),
        "{:?}",
        names(&all)
    );
    // The probe left no row and no trigger side effect behind.
    let rows_after = count(
        conn,
        "SELECT (SELECT COUNT(*) FROM snap.watched) + (SELECT COUNT(*) FROM snap.audit) \
         + (SELECT COUNT(*) FROM snap.fresh) AS n",
    )
    .await?;
    assert_eq!(rows_before, rows_after);
    // A second read starts from where the first left the allocators.
    let again = read_allocators(conn).await?;
    assert_eq!(next_of(&again, "snap.fresh.id (identity)"), "15");
    assert_eq!(next_of(&again, "snap.audit.n (identity)"), "3");
    assert_eq!(next_of(&again, "snap.codes"), "8");

    // A probe that takes no value, or not exactly one, is not guessed at: an
    // INSTEAD OF trigger that swallows the insert, and one that inserts into its
    // own table again (so the last value taken is not the first).
    for (table, trigger, word) in [
        (
            "hidden",
            "CREATE TRIGGER snap.swallow ON snap.hidden INSTEAD OF INSERT AS SET NOCOUNT ON",
            "snap.swallow",
        ),
        (
            "twice",
            "CREATE TRIGGER snap.again ON snap.twice AFTER INSERT AS INSERT snap.twice(x) VALUES (0)",
            "snap.again",
        ),
    ] {
        run(
            conn,
            &format!("CREATE TABLE snap.{table} (id int IDENTITY(1,1), x int NULL)"),
        )
        .await?;
        run(conn, trigger).await?;
        let text = refused_allocators(conn).await?;
        assert!(
            text.contains(&format!("snap.{table}")) && text.contains("exactly one value"),
            "{text}"
        );
        run(
            conn,
            &format!("DROP TRIGGER {word}; DROP TABLE snap.{table}"),
        )
        .await?;
    }

    // A trigger that inserts into another table only while that table's identity
    // is below 10 moves it during a probe of the first. Its next value is read
    // from before any probe, so the move is not mistaken for its state.
    run(
        conn,
        "CREATE TABLE snap.ca (id int IDENTITY(1,1), x int NULL); \
         CREATE TABLE snap.cb (id int IDENTITY(1,1), x int NULL); \
         INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES; \
         INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES; \
         INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES; INSERT snap.cb DEFAULT VALUES",
    )
    .await?;
    run(
        conn,
        "CREATE TRIGGER snap.reach ON snap.ca AFTER INSERT AS \
         IF IDENT_CURRENT(N'snap.cb') < 10 INSERT snap.cb DEFAULT VALUES",
    )
    .await?;
    let triggered = read_allocators(conn).await?;
    assert_eq!(next_of(&triggered, "snap.ca.id (identity)"), "1");
    // `cb` is 9 rows in: its next is 10, and `ca`'s trigger took it first.
    assert_eq!(next_of(&triggered, "snap.cb.id (identity)"), "10");
    run(
        conn,
        "DROP TRIGGER snap.reach; DROP TABLE snap.ca; DROP TABLE snap.cb",
    )
    .await?;

    // A probe of one table can take the last value another has: the second is
    // read from its state before any probe, and not refused as exhausted by a
    // probe of its own.
    run(
        conn,
        "CREATE TABLE snap.pa (id int IDENTITY(1,1), x int NULL); \
         CREATE TABLE snap.pz (id tinyint IDENTITY(255,1), x int NULL)",
    )
    .await?;
    run(
        conn,
        "CREATE TRIGGER snap.last ON snap.pa AFTER INSERT AS INSERT snap.pz DEFAULT VALUES",
    )
    .await?;
    let last = read_allocators(conn).await?;
    assert_eq!(next_of(&last, "snap.pa.id (identity)"), "1");
    assert_eq!(next_of(&last, "snap.pz.id (identity)"), "255");
    run(
        conn,
        "DROP TRIGGER snap.last; DROP TABLE snap.pa; DROP TABLE snap.pz",
    )
    .await?;

    // Sequences are read before identity columns: a probe evaluates the table's
    // defaults and takes a value of a sequence its column names.
    run(
        conn,
        "CREATE SEQUENCE snap.mixseq START WITH 1; \
         CREATE TABLE snap.mix (id int IDENTITY(1,1), c int NOT NULL DEFAULT (NEXT VALUE FOR snap.mixseq))",
    )
    .await?;
    assert_eq!(next_of(&read_allocators(conn).await?, "snap.mixseq"), "1");
    assert_eq!(next_of(&read_allocators(conn).await?, "snap.mixseq"), "3");
    run(conn, "DROP TABLE snap.mix; DROP SEQUENCE snap.mixseq").await?;

    // An exhausted identity column and an exhausted sequence name themselves.
    run(
        conn,
        "CREATE TABLE snap.small (id tinyint IDENTITY(250,3), x int NULL); \
         INSERT snap.small(x) VALUES (1), (2)",
    )
    .await?;
    let text = refused_allocators(conn).await?;
    assert!(text.contains("snap.small"), "{text}");
    run(conn, "DROP TABLE snap.small").await?;
    run(
        conn,
        "CREATE SEQUENCE snap.last START WITH 1 MAXVALUE 1 NO CYCLE; \
         SELECT NEXT VALUE FOR snap.last",
    )
    .await?;
    let text = refused_allocators(conn).await?;
    assert!(text.contains("snap.last"), "{text}");
    Ok(())
}

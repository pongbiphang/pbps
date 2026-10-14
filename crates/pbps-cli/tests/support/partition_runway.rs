use super::*;

/// `doctor` on a PostgreSQL target holding partitioned tables of every kind
/// of runway (#1789, DEC-1789.2), through the real CLI and the real engine.
///
/// Nine parents, each a question:
///
/// | parent | rows | the series | what `doctor` says |
/// |---|---|---|---|
/// | `short` | the last partition | ends with it | runway short: generate more |
/// | `covered` | November | runs to April | nothing |
/// | `empty` | none | ends in January | nothing: no row to be ahead of |
/// | `pending` | the last partition | ends with it, declared further | runway short: plan it |
/// | `ints` | the last partition | ends with it | runway short, in key values |
/// | `withdef` | October, and the DEFAULT's in a declared range | ahead | DEFAULT rows |
/// | `far` | October, and the DEFAULT's outside every declared range | ahead | nothing |
/// | `tz` | the last week | weekly in `timestamptz`, ends with it | runway short, in `+00` |
/// | `wall` | the only year | yearly in `timestamp`, ends with it | runway short, in a year |
///
/// Then the horizon in `pbps.yml`, a plan applied and a move allowed (the
/// findings leave), and a login that may not read the tables (its own
/// `…-unknown` findings, never silence).
pub fn check(server: &str, slug: &str) {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    // Declared before the database: dropped after it, once nothing is granted
    // to it any more.
    let role = Role(
        server.to_owned(),
        format!("pbps_runway_{}", std::process::id()),
    );
    let reader = role.1.as_str();
    let own = OwnDatabase::new(server, slug);
    let conn = own.connection().to_owned();
    on_server(&conn, "CREATE SCHEMA app");
    let d = Demo::new(slug);

    let months = |parent: &str, range: std::ops::RangeInclusive<u32>| -> String {
        let edge = |m: u32| {
            if m > 12 {
                format!("2027-{:02}-01", m - 12)
            } else {
                format!("2026-{m:02}-01")
            }
        };
        range
            .map(|m| {
                format!(
                    "  {parent}_{}: {{from: ['{}'], to: ['{}']}}\n",
                    edge(m)[..7].replace('-', "_"),
                    edge(m),
                    edge(m + 1)
                )
            })
            .collect()
    };
    let parent = |name: &str, key: &str, partitions: &str| {
        std::fs::write(
            d.dir.join(format!("schema/app.{name}.yml")),
            format!(
                "table: app.{name}\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  \
                 k: {{type: {key}, nullable: false}}\nprimary_key: [id, k]\n\
                 partition_by: [k]\npartitions:\n{partitions}"
            ),
        )
        .unwrap();
    };
    parent("short", "date", &months("short", 10..=12));
    parent("covered", "date", &months("covered", 10..=15));
    parent("empty", "date", &months("empty", 10..=12));
    parent("pending", "date", &months("pending", 10..=12));
    parent(
        "ints",
        "bigint",
        "  ints_0: {from: ['0'], to: ['1000']}\n  ints_1000: {from: ['1000'], to: ['2000']}\n  \
         ints_2000: {from: ['2000'], to: ['3000']}\n",
    );
    let with_default = |name: &str| format!("{}  {name}_rest: default\n", months(name, 10..=12));
    parent("withdef", "date", &with_default("withdef"));
    parent("far", "date", &with_default("far"));
    parent(
        "tz",
        "timestamptz",
        "  tz_2026_12_28: {from: ['2026-12-28 00:00:00+00'], to: ['2027-01-04 00:00:00+00']}\n  \
         tz_2027_01_04: {from: ['2027-01-04 00:00:00+00'], to: ['2027-01-11 00:00:00+00']}\n",
    );
    parent(
        "wall",
        "timestamp",
        "  wall_2026: {from: ['2026-01-01 00:00:00'], to: ['2027-01-01 00:00:00']}\n",
    );
    succeeds(d.run(&["fmt"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &conn]));
    succeeds(d.run(&["verify", "--db", &conn]));
    on_server(
        &conn,
        "INSERT INTO app.short VALUES (1, '2026-12-15'); \
         INSERT INTO app.covered VALUES (1, '2026-11-15'); \
         INSERT INTO app.pending VALUES (1, '2026-12-15'); \
         INSERT INTO app.ints VALUES (1, 2500); \
         INSERT INTO app.withdef VALUES (1, '2026-10-15'), (2, '2027-01-15'); \
         INSERT INTO app.far VALUES (1, '2026-10-15'), (2, '2035-01-01'); \
         INSERT INTO app.tz VALUES (1, '2027-01-05 10:00:00+00'); \
         INSERT INTO app.wall VALUES (1, '2026-06-01 12:00:00')",
    );
    assert_eq!(
        text_of(
            &conn,
            "SELECT string_agg(id::text || ':' || replace(tableoid::regclass::text, 'app.', ''), \
             ',' ORDER BY id) FROM app.withdef"
        ),
        "1:withdef_2026_10,2:withdef_rest",
        "the row of January 2027 is in the DEFAULT, as the scenario needs"
    );

    // Declared further than the database has: `pending` is a plan to apply.
    // `withdef` and `far` declare the months their DEFAULT's rows may belong
    // to, and `far`'s row lies outside every one of them.
    for table in ["pending", "withdef", "far"] {
        succeeds(d.run(&[
            "partitions",
            "extend",
            &format!("app.{table}"),
            "--every",
            "1month",
            "--until",
            "2027-03-01",
        ]));
    }

    let doctor = |target: &str| -> (Output, serde_json::Value) {
        let output = d.run(&["doctor", "--db", target, "--format", "json"]);
        let json = json_of(&output);
        (output, json)
    };
    let of = |json: &serde_json::Value, id: &str| -> Vec<serde_json::Value> {
        json["findings"]
            .as_array()
            .unwrap_or_else(|| panic!("{json}"))
            .iter()
            .filter(|f| f["id"] == id)
            .cloned()
            .collect()
    };
    // The parents a family of findings is about, by the name each message
    // gives first: `app.<parent>'s`.
    let parents_of = |json: &serde_json::Value, id: &str| -> Vec<String> {
        let mut parents: Vec<String> = of(json, id)
            .iter()
            .map(|f| {
                let message = f["message"].as_str().unwrap();
                let start = message.find("app.").unwrap_or_else(|| panic!("{message}"));
                let name: String = message[start + 4..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                // The DEFAULT finding names the DEFAULT first.
                name.split('_').next().unwrap().to_owned()
            })
            .collect();
        parents.sort();
        parents
    };
    let one = |json: &serde_json::Value, id: &str, parent: &str| -> serde_json::Value {
        let found: Vec<serde_json::Value> = of(json, id)
            .into_iter()
            .filter(|f| {
                f["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("app.{parent}'s"))
            })
            .collect();
        assert_eq!(found.len(), 1, "{id} for app.{parent}: {json}");
        found[0].clone()
    };
    let none_unknown = |json: &serde_json::Value| {
        for id in [
            "doctor.partition-runway-unknown",
            "doctor.partition-default-rows-unknown",
        ] {
            assert!(of(json, id).is_empty(), "{id}: {json}");
        }
    };

    // 1. The default horizon, twice the newest partition's width.
    let (o, json) = doctor(&conn);
    assert_eq!(code(&o), 0, "warnings leave doctor green: {json}");
    assert_eq!(json["data"]["environments"][0]["state"], "ready", "{json}");
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["ints", "pending", "short", "tz", "wall"],
        "{json}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-default-rows"),
        ["withdef"],
        "{json}"
    );
    none_unknown(&json);
    for f in of(&json, "doctor.partition-runway")
        .iter()
        .chain(&of(&json, "doctor.partition-default-rows"))
    {
        assert_eq!(f["severity"], "warning", "{f}");
    }
    let short = one(&json, "doctor.partition-runway", "short");
    let message = short["message"].as_str().unwrap();
    for said in [
        "partitions end at `2027-01-01`",
        "twice the newest partition's width",
        "the partition from `2026-12-01`",
        "to `2027-02-01` at least",
        "rows past it are refused, as no partition takes them",
    ] {
        assert!(message.contains(said), "{said}: {message}");
    }
    assert_eq!(
        short["remedy"], "pbps partitions extend app.short --every 1month --until \"<end>\"",
        "{short}"
    );
    let pending = one(&json, "doctor.partition-runway", "pending");
    assert!(
        pending["message"]
            .as_str()
            .unwrap()
            .contains("a plan applied here"),
        "{pending}"
    );
    assert!(
        pending["remedy"]
            .as_str()
            .unwrap()
            .starts_with("pbps plan --db "),
        "{pending}"
    );
    let ints = one(&json, "doctor.partition-runway", "ints");
    assert!(
        ints["message"].as_str().unwrap().contains("end at `3000`")
            && ints["message"]
                .as_str()
                .unwrap()
                .contains("to `4000` at least"),
        "{ints}"
    );
    assert!(
        ints["remedy"].as_str().unwrap().contains("--every 1000 "),
        "{ints}"
    );
    // The bounds are printed as the engine prints them, in the key's own type.
    let tz = one(&json, "doctor.partition-runway", "tz");
    let message = tz["message"].as_str().unwrap();
    assert!(
        message.contains("end at `2027-01-11 00:00:00+00`")
            && message.contains("the partition from `2027-01-04 00:00:00+00`")
            && message.contains("to `2027-01-18 00:00:00+00` at least"),
        "{tz}"
    );
    assert!(
        tz["remedy"].as_str().unwrap().contains("--every 1week "),
        "{tz}"
    );
    let wall = one(&json, "doctor.partition-runway", "wall");
    let message = wall["message"].as_str().unwrap();
    assert!(
        message.contains("end at `2027-01-01 00:00:00`")
            && message.contains("to `2028-01-01 00:00:00` at least"),
        "{wall}"
    );
    assert!(
        wall["remedy"].as_str().unwrap().contains("--every 1year "),
        "{wall}"
    );
    let held = of(&json, "doctor.partition-default-rows");
    let message = held[0]["message"].as_str().unwrap();
    // The partition whose range the row is in, not the first of those missing.
    assert!(
        message.contains(
            "app.withdef_rest holds rows inside the range of app.withdef_2027_01, which app.withdef's"
        ),
        "{message}"
    );
    assert_eq!(
        held[0]["remedy"],
        "pbps move-rows app.withdef_2027_01 --max-rows \"<max-rows>\" --reason \"<why>\"",
        "{}",
        held[0]
    );
    // The human view says the same, and is not the all-clear.
    let human = d.run(&["doctor", "--db", &conn]);
    assert_eq!(code(&human), 0, "{}{}", stdout(&human), stderr(&human));
    assert!(
        stdout(&human).contains("short of the horizon")
            && !stdout(&human).contains("Nothing to report"),
        "{}",
        stdout(&human)
    );

    // 2. The horizon in `pbps.yml`.
    let configured = |setting: &str| {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\npartition_runway: {setting}\n"),
        )
        .unwrap();
        doctor(&conn).1
    };
    // One month from the newest rows is met by every date series; a width of
    // time on a number is not a width, and is said so rather than guessed at.
    let json = configured("1month");
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["tz"],
        "a month on from the 4th of January is past the weeks the series has: {json}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-runway-unknown"),
        ["ints"],
        "{json}"
    );
    let unknown = one(&json, "doctor.partition-runway-unknown", "ints");
    assert!(
        unknown["message"]
            .as_str()
            .unwrap()
            .contains("a width of time, and this key is a bigint"),
        "{unknown}"
    );
    // A count of partitions, the one with the newest rows among them.
    let json = configured("3");
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["ints", "pending", "short", "tz", "wall"],
        "{json}"
    );
    let message = one(&json, "doctor.partition-runway", "short")["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        message.contains("(3 partitions)")
            && message.contains("for 3 partitions, counting the one holding the newest rows"),
        "{message}"
    );
    none_unknown(&json);
    // A horizon nobody can read is reported for every parent it would have
    // measured, and the check is not quietly run with the default.
    let json = configured("2fortnight");
    assert!(of(&json, "doctor.partition-runway").is_empty(), "{json}");
    assert_eq!(
        parents_of(&json, "doctor.partition-runway-unknown"),
        [
            "covered", "empty", "far", "ints", "pending", "short", "tz", "wall", "withdef"
        ],
        "{json}"
    );
    assert!(
        one(&json, "doctor.partition-runway-unknown", "short")["message"]
            .as_str()
            .unwrap()
            .contains("`fortnight` is not a unit"),
        "{json}"
    );
    // The DEFAULT's rows do not depend on the horizon.
    assert_eq!(
        parents_of(&json, "doctor.partition-default-rows"),
        ["withdef"],
        "{json}"
    );
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();

    // 3. The declared partitions applied, and the move of the DEFAULT's rows
    //    allowed: `pending` and `withdef` stop being findings, `short` and
    //    `ints` have had nothing done for them.
    succeeds(d.run(&["plan"]));
    succeeds(d.run(&[
        "move-rows",
        "app.withdef_2027_01",
        "--max-rows",
        "1",
        "--reason",
        "no reader of app.withdef",
    ]));
    d.commit();
    let plan = d.dir.join("runway.json");
    succeeds(d.run(&["plan", "--db", &conn, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(&d, &conn, &plan, &[]));
    let (o, json) = doctor(&conn);
    assert_eq!(code(&o), 0, "{json}");
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["ints", "short", "tz", "wall"],
        "{json}"
    );
    assert!(
        of(&json, "doctor.partition-default-rows").is_empty(),
        "the row moved out of the DEFAULT, and `far`'s, outside every declared range, was \
         never a finding: {json}"
    );
    none_unknown(&json);
    assert_eq!(
        text_of(
            &conn,
            "SELECT string_agg(id::text || ':' || replace(tableoid::regclass::text, 'app.', ''), \
             ',' ORDER BY id) FROM app.withdef"
        ),
        "1:withdef_2026_10,2:withdef_2027_01"
    );
    // And a series generated, planned and applied is no longer short.
    succeeds(d.run(&[
        "partitions",
        "extend",
        "app.short",
        "--every",
        "1month",
        "--until",
        "2027-03-01",
    ]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", &conn, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(&d, &conn, &plan, &[]));
    let (_, json) = doctor(&conn);
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["ints", "tz", "wall"],
        "{json}"
    );

    // 4. A login that may read the catalogs and the ledger and the parent of
    //    one table only. Privileges are checked on the parent, not on the
    //    partitions the read is pruned to: `short`'s partitions are not
    //    granted, and its runway is read all the same.
    on_server(
        &conn,
        &format!(
            "CREATE ROLE {reader} LOGIN NOSUPERUSER PASSWORD 'runway-1789-secret'; \
             GRANT USAGE ON SCHEMA public, app TO {reader}; \
             GRANT SELECT ON public.__pbps_state, public.__pbps_lock TO {reader}; \
             GRANT SELECT ON app.short TO {reader}"
        ),
    );
    let login = format!(
        "{} user={reader} password=runway-1789-secret",
        conn.split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    on_server(&conn, &format!("GRANT SELECT ON app.ints TO {reader}"));
    // Declared months the database lacks again, so that the DEFAULT of
    // `withdef` and of `far` has a range to be asked about.
    for table in ["withdef", "far"] {
        succeeds(d.run(&[
            "partitions",
            "extend",
            &format!("app.{table}"),
            "--every",
            "1month",
            "--until",
            "2027-05-01",
        ]));
    }
    let (o, json) = doctor(&login);
    assert!(
        !stdout(&o).contains("runway-1789-secret") && !stderr(&o).contains("runway-1789-secret"),
        "the password is never printed"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-runway"),
        ["ints"],
        "read through the parent alone: {json}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-runway-unknown"),
        [
            "covered", "empty", "far", "pending", "tz", "wall", "withdef"
        ],
        "{json}"
    );
    let denied = one(&json, "doctor.partition-runway-unknown", "withdef");
    assert!(
        denied["message"]
            .as_str()
            .unwrap()
            .contains("permission denied for table withdef"),
        "{denied}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-default-rows-unknown"),
        ["far", "withdef"],
        "{json}"
    );
    assert!(
        of(&json, "doctor.partition-default-rows").is_empty(),
        "a DEFAULT that could not be read is not a DEFAULT with nothing in it: {json}"
    );
    for f in of(&json, "doctor.partition-runway-unknown")
        .iter()
        .chain(&of(&json, "doctor.partition-default-rows-unknown"))
    {
        assert_eq!(f["severity"], "warning", "{f}");
    }
    // `SELECT` on the parent of `withdef` alone is enough for its DEFAULT's
    // question too, though the DEFAULT is a table of its own the login may not
    // read: a row of March, a month declared and not created, is found.
    on_server(
        &conn,
        &format!(
            "INSERT INTO app.withdef VALUES (3, '2027-03-20'); \
             GRANT SELECT ON app.withdef TO {reader}"
        ),
    );
    assert_eq!(
        text_of(
            &conn,
            "SELECT replace(tableoid::regclass::text, 'app.', '') FROM app.withdef WHERE id = 3"
        ),
        "withdef_rest"
    );
    try_on_server(&login, "SELECT count(*) FROM app.withdef_rest")
        .expect_err("the login may not read the DEFAULT itself");
    let (_, json) = doctor(&login);
    assert_eq!(
        parents_of(&json, "doctor.partition-default-rows"),
        ["withdef"],
        "{json}"
    );
    assert!(
        of(&json, "doctor.partition-default-rows")[0]["message"]
            .as_str()
            .unwrap()
            .contains("holds rows inside the range of app.withdef_2027_03, which"),
        "{json}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-runway-unknown"),
        ["covered", "empty", "far", "pending", "tz", "wall"],
        "{json}"
    );
    assert_eq!(
        parents_of(&json, "doctor.partition-default-rows-unknown"),
        ["far"],
        "{json}"
    );
    // The same login with the ledger's `SELECT` taken away cannot say what
    // the environment calls the parents: one finding for all of them.
    on_server(
        &conn,
        &format!("REVOKE SELECT ON public.__pbps_state FROM {reader}"),
    );
    let (_, json) = doctor(&login);
    let unknown = of(&json, "doctor.partition-runway-unknown");
    assert_eq!(unknown.len(), 1, "{json}");
    assert!(
        unknown[0]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read recorded identities"),
        "{unknown:?}"
    );
    assert!(
        of(&json, "doctor.partition-runway").is_empty()
            && of(&json, "doctor.partition-default-rows-unknown").is_empty(),
        "{json}"
    );
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", stdout(output), stderr(output)))
}

/// A parent a rebuild holds `ACCESS EXCLUSIVE` is not waited for (#1789,
/// DEC-1789.2): `doctor` is the command run to find out what is going on, so
/// both questions give up after the lock timeout and say so, each as its own
/// `…-unknown` finding, and the same parent answers once the lock is gone.
pub fn locked(server: &str, slug: &str) {
    let own = OwnDatabase::new(server, slug);
    let conn = own.connection().to_owned();
    on_server(&conn, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    std::fs::write(
        d.dir.join("schema/app.t.yml"),
        "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
         k: {type: date, nullable: false}\nprimary_key: [id, k]\npartition_by: [k]\n\
         partitions:\n  t_2026_12: {from: ['2026-12-01'], to: ['2027-01-01']}\n  t_rest: default\n",
    )
    .unwrap();
    succeeds(d.run(&["fmt"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &conn]));
    on_server(&conn, "INSERT INTO app.t VALUES (1, '2026-12-15')");
    succeeds(d.run(&[
        "partitions",
        "extend",
        "app.t",
        "--every",
        "1month",
        "--until",
        "2027-02-01",
    ]));
    on_server(&conn, "INSERT INTO app.t VALUES (2, '2027-01-15')");

    let (held, release) = (
        std::sync::mpsc::channel::<()>(),
        std::sync::mpsc::channel::<()>(),
    );
    let holder = {
        let conn = conn.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &conn)
                        .await
                        .unwrap();
                    c.execute("BEGIN; LOCK TABLE app.t IN ACCESS EXCLUSIVE MODE")
                        .await
                        .unwrap();
                    held.0.send(()).unwrap();
                    release.1.recv().ok();
                    c.execute("ROLLBACK").await.unwrap();
                });
        })
    };
    held.1.recv().expect("the lock is held");
    let started = std::time::Instant::now();
    let output = d.run(&["doctor", "--db", &conn, "--format", "json"]);
    let waited = started.elapsed();
    release.0.send(()).ok();
    holder.join().unwrap();
    let json = json_of(&output);
    assert_eq!(code(&output), 0, "{json}");
    assert!(
        waited < std::time::Duration::from_secs(40),
        "doctor queued behind the lock for {waited:?}"
    );
    let ids: Vec<&str> = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .filter(|id| id.starts_with("doctor.partition"))
        .collect();
    assert_eq!(
        ids,
        [
            "doctor.partition-runway-unknown",
            "doctor.partition-default-rows-unknown"
        ],
        "{json}"
    );
    for f in json["findings"].as_array().unwrap() {
        if f["id"].as_str().unwrap().starts_with("doctor.partition") {
            assert!(
                f["message"].as_str().unwrap().contains("lock timeout"),
                "{f}"
            );
        }
    }

    // Released: the same parent answers.
    let json = json_of(&d.run(&["doctor", "--db", &conn, "--format", "json"]));
    let ids: Vec<&str> = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .filter(|id| id.starts_with("doctor.partition"))
        .collect();
    assert_eq!(
        ids,
        ["doctor.partition-runway", "doctor.partition-default-rows"],
        "{json}"
    );
}

/// `doctor` on parents whose bounds carry fractions of a second (#1923): the
/// newest partition is the one ending last inside its second, the reach it
/// asks for keeps its fraction, and a series ending a microsecond short of
/// that reach is short.
///
/// Four parents, each with a row in each of its first two partitions, and a
/// horizon of `1day` in `pbps.yml`:
///
/// | parent | key | the series ends at | what `doctor` says |
/// |---|---|---|---|
/// | `ts` | `timestamp` | `…-02 00:00:00.9` | short: the reach is `…-03 00:00:00.1` |
/// | `tz` | `timestamptz` | `…-02 00:00:00.9+00` | short, in `+00` |
/// | `meets` | `timestamp` | `…-03 00:00:00.1` | nothing: it is exactly the reach |
/// | `misses` | `timestamp` | `…-03 00:00:00.099999` | short by one microsecond |
///
/// Were the partition ending at `.1` taken for the newest (the two ends read
/// as equal), the rows of `ts` would be measured from the first partition,
/// whose day on is `…-02 00:00:00.5`, and the series would read as covered.
pub fn fractions(server: &str, slug: &str) {
    let own = OwnDatabase::new(server, slug);
    let conn = own.connection().to_owned();
    on_server(&conn, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    let parent = |name: &str, key: &str, zone: &str, last: Option<&str>| {
        let last = last
            .map(|end| {
                format!(
                    "  {name}_c: {{from: ['2026-01-02 00:00:00.9{zone}'], to: ['{end}{zone}']}}\n"
                )
            })
            .unwrap_or_default();
        std::fs::write(
            d.dir.join(format!("schema/app.{name}.yml")),
            format!(
                "table: app.{name}\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  \
                 k: {{type: {key}, nullable: false}}\nprimary_key: [id, k]\n\
                 partition_by: [k]\npartitions:\n  \
                 {name}_a: {{from: ['2026-01-01 00:00:00.5{zone}'], \
                 to: ['2026-01-02 00:00:00.1{zone}']}}\n  \
                 {name}_b: {{from: ['2026-01-02 00:00:00.1{zone}'], \
                 to: ['2026-01-02 00:00:00.9{zone}']}}\n{last}"
            ),
        )
        .unwrap();
    };
    parent("ts", "timestamp", "", None);
    parent("tz", "timestamptz", "+00", None);
    parent("meets", "timestamp", "", Some("2026-01-03 00:00:00.1"));
    parent(
        "misses",
        "timestamp",
        "",
        Some("2026-01-03 00:00:00.099999"),
    );
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\npartition_runway: 1day\n",
    )
    .unwrap();
    succeeds(d.run(&["fmt"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &conn]));
    succeeds(d.run(&["verify", "--db", &conn]));
    let rows: String = ["ts", "meets", "misses"]
        .iter()
        .map(|t| {
            format!(
                "INSERT INTO app.{t} VALUES (1, '2026-01-01 12:00:00'), \
                 (2, '2026-01-02 00:00:00.5'); "
            )
        })
        .collect();
    on_server(
        &conn,
        &format!(
            "{rows}INSERT INTO app.tz VALUES (1, '2026-01-01 12:00:00+00'), \
             (2, '2026-01-02 00:00:00.5+00')"
        ),
    );
    // The engine prints the bounds as the scenario writes them, which is what
    // the reader is asked to take.
    assert_eq!(
        text_of(
            &conn,
            "SELECT string_agg(pg_get_expr(c.relpartbound, c.oid), ' ' ORDER BY c.relname) \
             FROM pg_class c WHERE c.relname IN ('ts_a', 'ts_b')"
        ),
        "FOR VALUES FROM ('2026-01-01 00:00:00.5') TO ('2026-01-02 00:00:00.1') \
         FOR VALUES FROM ('2026-01-02 00:00:00.1') TO ('2026-01-02 00:00:00.9')"
    );

    let o = d.run(&["doctor", "--db", &conn, "--format", "json"]);
    assert_eq!(code(&o), 0, "warnings leave doctor green: {}", stdout(&o));
    let json = json_of(&o);
    let findings = json["findings"]
        .as_array()
        .unwrap_or_else(|| panic!("{json}"));
    assert!(
        findings
            .iter()
            .all(|f| f["id"] != "doctor.partition-runway-unknown"),
        "a fraction of a second is a bound the reader takes: {json}"
    );
    let of = |parent: &str| -> Vec<&serde_json::Value> {
        findings
            .iter()
            .filter(|f| {
                f["id"] == "doctor.partition-runway"
                    && f["message"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("app.{parent}'s"))
            })
            .collect()
    };
    for (parent, zone) in [("ts", ""), ("tz", "+00")] {
        let found = of(parent);
        assert_eq!(found.len(), 1, "{parent}: {json}");
        let message = found[0]["message"].as_str().unwrap();
        for said in [
            format!("end at `2026-01-02 00:00:00.9{zone}`"),
            format!("the partition from `2026-01-02 00:00:00.1{zone}`"),
            format!("to `2026-01-03 00:00:00.1{zone}` at least"),
        ] {
            assert!(message.contains(&said), "{said}: {message}");
        }
        // Fractions are off the generator's whole-second grid: the remedy
        // keeps its placeholder rather than naming a step it could not write.
        assert!(
            found[0]["remedy"]
                .as_str()
                .unwrap()
                .contains("--every \"<step>\""),
            "{}",
            found[0]
        );
    }
    assert!(
        of("meets").is_empty(),
        "a series ending exactly at the reach is covered: {json}"
    );
    let found = of("misses");
    assert_eq!(found.len(), 1, "{json}");
    let message = found[0]["message"].as_str().unwrap();
    assert!(
        message.contains("end at `2026-01-03 00:00:00.099999`")
            && message.contains("to `2026-01-03 00:00:00.1` at least"),
        "{message}"
    );
}

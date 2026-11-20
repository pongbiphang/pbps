//! The texts the tests run. Every one is valid SQLite, which the corpus
//! checks by running it (`accepted`): a text the engine refuses is dropped, so
//! the corpus holds only what a stored `CREATE` can be.

use crate::tests::Fresh;

/// A statement and what has to exist before it runs.
#[derive(Clone)]
pub(super) struct Case {
    pub setup: &'static str,
    pub sql: String,
}

/// Parents for the foreign keys of the corpus.
pub(super) const PARENTS: &str = "CREATE TABLE p(id integer primary key, x int, y int, UNIQUE(x, y)); CREATE TABLE q(a int, b int, PRIMARY KEY (a, b))";

fn case(sql: &str) -> Case {
    Case {
        setup: PARENTS,
        sql: sql.to_owned(),
    }
}

/// Whether the engine takes `case`.
pub(super) fn takes(case: &Case) -> bool {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(case.setup).expect("the setup");
    c.execute_batch(&case.sql).is_ok()
}

/// The cases the engine takes.
pub(super) fn accepted(cases: Vec<Case>) -> Vec<Case> {
    cases.into_iter().filter(takes).collect()
}

/// Every table text of the corpus. One statement each, named `t`.
pub(super) fn tables() -> Vec<Case> {
    let mut out: Vec<Case> = Vec::new();
    let mut add = |sql: &str| out.push(case(sql));
    // Types and layout.
    add("CREATE TABLE t(a int, b text)");
    add("CREATE TABLE t(a, b)");
    add("CREATE TABLE t(a)");
    add(
        "CREATE TABLE t(a VARCHAR( 10 ), b DECIMAL(10, 2), c UNSIGNED BIG INT, d DOUBLE PRECISION, e Integer)",
    );
    add("CREATE TABLE t(a \"my type\", b [t], c `x y`, d 'str', e \"a\"\"b\" int)");
    add("CREATE TABLE t(a int(5), b char(-1), c numeric(+3, 2))");
    add("CREATE TABLE t(a int\n,\n\tb\u{c}text\r\n)");
    add("CREATE /*c*/ TABLE -- x\n t /*c*/ ( /*c*/ a /*c*/ int /*c*/ , /*c*/ b /*c*/ ) /*c*/");
    add("create table t(a integer primary key, b TEXT NOT NULL)");
    add("CREATE TABLE t(a int -- trailing\n)");
    // The eight words the grammar takes as names and never in a type, inside
    // a type, and names that are keywords SQLite takes bare.
    for word in [
        "cross", "full", "indexed", "inner", "left", "natural", "outer", "right",
    ] {
        add(&["CREATE TABLE t(a unsigned ", word, " int)"].concat());
        add(&["CREATE TABLE t(a int, ", word, " int)"].concat());
    }
    for word in [
        "key",
        "action",
        "generated",
        "always",
        "ignore",
        "temp",
        "strict",
        "without",
        "rowid",
        "match",
        "no",
        "cascade",
        "deferred",
        "initially",
        "immediate",
        "stored",
        "virtual",
        "asc",
        "desc",
        "conflict",
        "fail",
        "abort",
        "replace",
        "rollback",
        "restrict",
        "if",
        "after",
    ] {
        add(&["CREATE TABLE t(", word, " int, b int)"].concat());
        add(&[
            "CREATE TABLE t(a int, ",
            word,
            " text NOT NULL DEFAULT 'x')",
        ]
        .concat());
    }
    // Names in the four quoted forms, for the table, a column and a constraint.
    add("CREATE TABLE \"t\"(\"a\" int, CONSTRAINT \"k\" CHECK (\"a\" > 0))");
    add("CREATE TABLE `t`(`a` int, CONSTRAINT `k` CHECK (`a` > 0))");
    add("CREATE TABLE [t]([a] int, CONSTRAINT [k] CHECK ([a] > 0))");
    add("CREATE TABLE 't'('a' int, CONSTRAINT 'k' CHECK (a > 0))");
    add(
        "CREATE TABLE \"t\"(\"a\"\"b\" int, `c``d` int, [e f] int, CONSTRAINT \"k\"\"1\" CHECK (\"a\"\"b\" > 0))",
    );
    add("CREATE TABLE t(\"a b\" int, \"é\" int, \"ß\" int, \"\" int)");
    // Column-level constraints, named and not.
    add("CREATE TABLE t(a int NOT NULL, b int NULL, c int PRIMARY KEY)");
    add(
        "CREATE TABLE t(a int CONSTRAINT nn NOT NULL CONSTRAINT dd DEFAULT 5 CONSTRAINT ck CHECK (a > 0) CONSTRAINT uq UNIQUE CONSTRAINT rf REFERENCES p(id) ON DELETE CASCADE)",
    );
    add("CREATE TABLE t(a int CONSTRAINT pk PRIMARY KEY, b int CONSTRAINT n NULL)");
    add(
        "CREATE TABLE t(a int UNIQUE, b int UNIQUE NOT NULL, c int CHECK (c > 0), d int REFERENCES p(id))",
    );
    add("CREATE TABLE t(a int NOT NULL NOT NULL, b int DEFAULT 1 DEFAULT 2)");
    // Table-level constraints, named and not.
    add("CREATE TABLE t(a int, b int, PRIMARY KEY (a, b))");
    add("CREATE TABLE t(a int, b int, PRIMARY KEY (b, a))");
    add(
        "CREATE TABLE t(a int, b int, CONSTRAINT pk PRIMARY KEY (b), CONSTRAINT u UNIQUE (a, b), UNIQUE (b, a), CHECK (a < b), CONSTRAINT c CHECK (a <> b))",
    );
    add("CREATE TABLE t(a int, b int, UNIQUE (a), UNIQUE (a), UNIQUE (a, b), UNIQUE (b, a))");
    add("CREATE TABLE t(a int, b int, PRIMARY KEY (a), UNIQUE (a))");
    add("CREATE TABLE t(a integer, PRIMARY KEY (a))");
    add("CREATE TABLE t(a integer PRIMARY KEY, b int, UNIQUE (b))");
    add("CREATE TABLE t(a int primary key, b int primary key)");
    // Checks: nesting, strings and comments that hold a parenthesis.
    add("CREATE TABLE t(a int, CHECK (a > 0))");
    add("CREATE TABLE t(a int, CHECK ((a > 0) AND (a < 9)))");
    add("CREATE TABLE t(a text, CHECK (a <> ')' AND a <> '(('))");
    add("CREATE TABLE t(a text, CHECK (a <> \")\" OR a IN ('a', 'b')))");
    add("CREATE TABLE t(a int, CHECK (a /* ) */ > 0 -- )\n))");
    add("CREATE TABLE t(a int, CHECK (a /* /* */ > 0))");
    add("CREATE TABLE t(a int CHECK (a > 0 AND (a < 9 OR a = 11)), b int CHECK ( b IS NOT NULL ))");
    add("CREATE TABLE t(a int, CHECK (abs(a) > length('x,y')))");
    add(
        "CREATE TABLE t(a int, CONSTRAINT \"c,d\" CHECK (a > 0), CONSTRAINT \"e)f\" CHECK (a < 9))",
    );
    // Defaults.
    for default in [
        "1",
        "(1)",
        "-5",
        "+5",
        "- 5",
        "1.5e3",
        "1e+5",
        "1E-5",
        "0x1F",
        ".5",
        "1.",
        "'x'",
        "'it''s'",
        "''",
        "x'ab'",
        "X'ab'",
        "x''",
        "NULL",
        "null",
        "TRUE",
        "false",
        "CURRENT_TIME",
        "current_date",
        "CURRENT_TIMESTAMP",
        "(lower('A'))",
        "( 'a' || 'b' )",
        "('x)' )",
        "(1+1)",
        "(-5)",
        "((1))",
        "(abs(-1))",
        "(1 /* ) */)",
    ] {
        add(&["CREATE TABLE t(a text DEFAULT ", default, ", b int)"].concat());
        add(&[
            "CREATE TABLE t(b int, a text NOT NULL DEFAULT ",
            default,
            " CHECK (b > 0))",
        ]
        .concat());
    }
    // Foreign keys.
    add("CREATE TABLE t(a int REFERENCES p(id), b int REFERENCES p)");
    add(
        "CREATE TABLE t(a int, b int, FOREIGN KEY (a) REFERENCES p(id), CONSTRAINT f FOREIGN KEY (a, b) REFERENCES q(a, b))",
    );
    add(
        "CREATE TABLE t(a int, b int, c int, d int, CONSTRAINT f1 FOREIGN KEY (a) REFERENCES p(id) ON DELETE CASCADE ON UPDATE SET NULL, CONSTRAINT f2 FOREIGN KEY (b, c) REFERENCES p(x, y) ON DELETE RESTRICT, FOREIGN KEY (d) REFERENCES p)",
    );
    add(
        "CREATE TABLE t(a int REFERENCES p(id) ON UPDATE NO ACTION ON DELETE SET DEFAULT, b int REFERENCES p(id) ON DELETE SET NULL)",
    );
    add(
        "CREATE TABLE t(a int, FOREIGN KEY (a) REFERENCES \"p\"(\"id\"), FOREIGN KEY (a) REFERENCES `p`, FOREIGN KEY (a) REFERENCES [p](id))",
    );
    add("CREATE TABLE t(a int REFERENCES P(id), b int REFERENCES missing(id))");
    add(
        "CREATE TABLE t(a int REFERENCES p(id) NOT NULL, b int REFERENCES p(id) DEFAULT 1, c int REFERENCES p(id) CONSTRAINT k CHECK (c > 0))",
    );
    add("CREATE TABLE t(a int, FOREIGN KEY (a) REFERENCES t(a))");
    // Markers.
    add(
        "CREATE TABLE t(a text COLLATE nocase, b text COLLATE \"rtrim\", c text COLLATE BINARY NOT NULL)",
    );
    add(
        "CREATE TABLE t(a int NOT NULL ON CONFLICT IGNORE, b int UNIQUE ON CONFLICT REPLACE, c int PRIMARY KEY ON CONFLICT FAIL)",
    );
    add(
        "CREATE TABLE t(a int, b int, PRIMARY KEY (a, b) ON CONFLICT ROLLBACK, UNIQUE (b) ON CONFLICT ABORT)",
    );
    add("CREATE TABLE t(a integer PRIMARY KEY AUTOINCREMENT, b int)");
    add("CREATE TABLE t(a INTEGER PRIMARY KEY DESC, b int)");
    add("CREATE TABLE t(a integer PRIMARY KEY ASC ON CONFLICT IGNORE AUTOINCREMENT)");
    add(
        "CREATE TABLE t(a int, b int, PRIMARY KEY (a DESC, b COLLATE nocase ASC), UNIQUE (b COLLATE rtrim DESC))",
    );
    add(
        "CREATE TABLE t(a int, b int GENERATED ALWAYS AS (a + 1) VIRTUAL, c int AS (a + 2) STORED, d int as (a), e int generated always as (a * 2))",
    );
    add(
        "CREATE TABLE t(a int REFERENCES p(id) MATCH FULL DEFERRABLE INITIALLY DEFERRED, b int REFERENCES p(id) NOT DEFERRABLE INITIALLY IMMEDIATE, c int REFERENCES p(id) DEFERRABLE)",
    );
    add(
        "CREATE TABLE t(a int, FOREIGN KEY (a) REFERENCES p(id) ON DELETE CASCADE MATCH SIMPLE NOT DEFERRABLE INITIALLY DEFERRED)",
    );
    add(
        "CREATE TABLE t(a int, b int NOT NULL GENERATED ALWAYS AS (a) STORED, c int NOT NULL CHECK (c > 0) AS (a + 1))",
    );
    add("CREATE TABLE t(a int primary key) STRICT");
    add("CREATE TABLE t(a int primary key, b int) WITHOUT ROWID");
    add("CREATE TABLE t(a int primary key, b int) STRICT, WITHOUT ROWID");
    add("CREATE TABLE t(a int primary key, b int) WITHOUT ROWID, STRICT");
    add("CREATE TABLE t(a int) strict /* c */");
    // Shapes the engine leaves after `ALTER TABLE` (see also `closure`).
    add(
        "CREATE TABLE \"t\" (\n    \"id\" integer NOT NULL,\n    \"quantity\" integer, \"z\" integer NOT NULL DEFAULT 0,\n    PRIMARY KEY (\"id\"),\n    CONSTRAINT \"c_qty\" CHECK (\"quantity\" >= 0)\n, CONSTRAINT \"c_z\" CHECK (z >= 0))",
    );
    add(
        "CREATE TABLE \"t\" (\n    \"id\" integer NOT NULL,\n    \"name\" text DEFAULT 'x' NOT NULL,\n    \"owner\" integer NOT NULL,\n    CONSTRAINT \"f_owner\" FOREIGN KEY (\"owner\") REFERENCES \"p\" (\"id\") ON DELETE CASCADE ON UPDATE NO ACTION\n)",
    );
    add(
        "CREATE TABLE t(a int /* c1 */, \"x\" integer NOT NULL DEFAULT 0, CONSTRAINT k CHECK (a > 0))",
    );
    // Named constraints of every kind, for the `DROP CONSTRAINT` probes.
    add("CREATE TABLE t(a int CONSTRAINT nn NOT NULL, b int CONSTRAINT nb NOT NULL DEFAULT 1)");
    add(
        "CREATE TABLE t(a int, b int, CONSTRAINT ck1 CHECK (a > 0), CONSTRAINT ck2 CHECK (b > 0), CONSTRAINT fk1 FOREIGN KEY (a) REFERENCES p(id), CONSTRAINT fk2 FOREIGN KEY (b) REFERENCES p)",
    );
    add(
        "CREATE TABLE t(\"a b\" int CONSTRAINT \"n 1\" NOT NULL, CONSTRAINT \"c\"\"q\" CHECK (1), CONSTRAINT \"f f\" FOREIGN KEY (\"a b\") REFERENCES p(id))",
    );
    add(
        "CREATE TABLE t(a int CONSTRAINT [x y] CHECK (a > 0), b int CONSTRAINT `z` REFERENCES p(id), c int CONSTRAINT 'w' NOT NULL)",
    );
    add("CREATE TABLE t(a int CONSTRAINT K CHECK (a > 0), b int CONSTRAINT k2 CHECK (b > 0))");
    add(
        "CREATE TABLE t(a int CONSTRAINT c1 CHECK (a > 0) CONSTRAINT c2 CHECK (a < 9) CONSTRAINT n1 NOT NULL CONSTRAINT d1 DEFAULT 4)",
    );
    add(
        "CREATE TABLE t(a int, b int, c int, CONSTRAINT c1 CHECK (a > 0), CONSTRAINT c2 CHECK (b > 0), CONSTRAINT c3 CHECK (c > 0), CONSTRAINT u1 UNIQUE (a), CONSTRAINT p1 PRIMARY KEY (b, c))",
    );
    add(
        "CREATE TABLE t(a int CONSTRAINT p PRIMARY KEY, b int CONSTRAINT u UNIQUE, c int CONSTRAINT n NULL, d int CONSTRAINT dflt DEFAULT (1+1))",
    );
    add("CREATE TABLE t(a int, CONSTRAINT \"é\" CHECK (a > 0), CONSTRAINT \"\" CHECK (a < 9))");
    add(
        "CREATE TABLE t(a int, CONSTRAINT x CHECK (a > 0), CONSTRAINT y CHECK (a > 1), CONSTRAINT z FOREIGN KEY (a) REFERENCES p(id) ON DELETE SET NULL ON UPDATE CASCADE) STRICT",
    );
    // Many items.
    let wide: Vec<String> = (0..40).map(|i| format!("c{i} int DEFAULT {i}")).collect();
    add(&["CREATE TABLE t(", &wide.join(", "), ")"].concat());
    out
}

/// Every index text of the corpus, on the table `t(a, b, c)`.
pub(super) fn indexes() -> Vec<Case> {
    const TABLE: &str = "CREATE TABLE t(a int, b int, c text, \"d e\" int)";
    let mut out = Vec::new();
    for sql in [
        "CREATE INDEX i ON t(a)",
        "CREATE UNIQUE INDEX i ON t(a, b)",
        "CREATE INDEX i ON t(a DESC, b ASC)",
        "CREATE INDEX i ON t(c COLLATE nocase)",
        "CREATE INDEX i ON t(lower(c))",
        "CREATE INDEX i ON t(a + b, c)",
        "CREATE INDEX i ON t((a))",
        "CREATE INDEX i ON t(a) WHERE b > 0",
        "CREATE UNIQUE INDEX i ON t(a) WHERE b IS NOT NULL AND (c <> 'x' OR a < 3)",
        "CREATE INDEX i ON t(a) WHERE c <> ')'",
        "CREATE INDEX i ON t(a) WHERE a /* ) */ > 0 -- )\n",
        "CREATE INDEX i ON t(a) WHERE a /* /* */ > 0",
        "CREATE INDEX i ON t(a)\n  WHERE\n    b > 0",
        "CREATE INDEX \"i\"\"j\" ON \"t\"(\"d e\")",
        "CREATE INDEX `i` ON [t](`d e`)",
        "CREATE INDEX 'i' ON t('a')",
        "create unique index if not exists main.i on t(a)",
        "CREATE INDEX /*c*/ i /*c*/ ON /*c*/ t /*c*/ ( /*c*/ a /*c*/ , b ) /*c*/ WHERE /*c*/ a > 0",
        "CREATE INDEX i ON t(a, b, c, \"d e\")",
        "CREATE INDEX i ON t(abs(a), (b), c)",
        "CREATE INDEX i ON t(max(a, b), c)",
        "CREATE INDEX i ON t(coalesce(a, b, 0) DESC, 'x,y' || c)",
        "CREATE INDEX i ON t(a) WHERE (b)",
        "CREATE INDEX i ON t(a) WHERE b > 0 -- note",
        "CREATE INDEX i ON t(a) WHERE b > 0 /* why */",
        "CREATE INDEX i ON t(a) WHERE b > 0\n-- note\n",
        "CREATE INDEX i ON t(a) WHERE b > 0;",
        "CREATE INDEX i ON t(a) WHERE /* c */ b > 0",
        "CREATE INDEX i ON t(a) WHERE -- c\n b > 0",
        "CREATE INDEX i ON t(a) /* c */ WHERE b > 0   \n",
        "CREATE INDEX i ON t(a) -- c\n WHERE /* d */ b > 0 /* e */ -- f\n",
        "CREATE INDEX i ON t(a) WHERE b > 0 -- note\n;",
    ] {
        out.push(Case {
            setup: TABLE,
            sql: sql.to_owned(),
        });
    }
    out
}

/// Every view text of the corpus, over the table `t(a, b)`.
pub(super) fn views() -> Vec<Case> {
    const TABLE: &str = "CREATE TABLE t(a int, b int)";
    let mut out = Vec::new();
    for sql in [
        "CREATE VIEW v AS SELECT a FROM t",
        "CREATE VIEW v(x, y) AS SELECT a, b FROM t",
        "CREATE VIEW v (\"x y\", [z]) AS SELECT a, b FROM t",
        "CREATE VIEW v AS WITH x AS (SELECT 1 AS a) SELECT a FROM x",
        "CREATE VIEW v AS VALUES (1), (2)",
        "CREATE VIEW v AS SELECT a /* ; */ FROM t -- AS\n WHERE b > 0",
        "CREATE VIEW v /* c */ AS -- note\n SELECT a FROM t",
        "CREATE VIEW \"v\"\"w\" AS SELECT 'AS' AS \"AS\" FROM t",
        "CREATE VIEW `v` AS SELECT a FROM [t]",
        "CREATE VIEW 'v' AS SELECT a FROM t",
        "CREATE VIEW IF NOT EXISTS main.v AS SELECT a FROM t",
        "CREATE VIEW v AS SELECT a FROM t UNION SELECT b FROM t",
        "CREATE VIEW v AS SELECT (a) FROM t WHERE (b) > (SELECT 1)",
        "create view v as select a from t",
        "CREATE VIEW v AS\n\n   SELECT\ta\n FROM t\n",
        "CREATE VIEW v AS SELECT a FROM t -- note",
        "CREATE VIEW v AS SELECT a FROM t /* why */",
        "CREATE VIEW v AS SELECT a FROM t\n-- note\n",
        "CREATE VIEW v AS SELECT a FROM t;",
        "CREATE VIEW v AS SELECT a FROM t /* a */ -- b\n /* c */",
        "CREATE VIEW v AS /* c */ SELECT a FROM t",
        "CREATE VIEW v AS -- c\n SELECT a FROM t",
        "CREATE VIEW v /* c */ AS\n\n  SELECT a FROM t   \n\n",
        "CREATE VIEW v AS SELECT a FROM t -- note\n;",
    ] {
        out.push(Case {
            setup: TABLE,
            sql: sql.to_owned(),
        });
    }
    out
}

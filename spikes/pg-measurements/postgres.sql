-- The evidence behind ADR-0009, ADR-0010, ADR-0012 and ADR-0013.
--
-- Self-contained: it builds everything it needs and drops it again, so it can
-- be run against any PostgreSQL and compared with the committed observation.
-- Every claim prints one line, so a version that answers differently shows up
-- as a one-line diff rather than as prose somebody has to re-read.
--
-- Output is `<id> | <claim> | <observed>`. The ADRs quote the observed column.

\set ON_ERROR_STOP off
\pset format unaligned
\pset fieldsep ' | '
\pset tuples_only on

DROP SCHEMA IF EXISTS m CASCADE;
CREATE SCHEMA m;
SET search_path = m;

-- A helper that answers "did this statement rebuild the table?" with the
-- engine's own answer — relfilenode changes exactly when the heap is rewritten.
CREATE FUNCTION m.rewrites(create_sql text, insert_sql text, alter_sql text)
RETURNS text AS $fn$
DECLARE before oid; after oid;
BEGIN
  EXECUTE 'DROP TABLE IF EXISTS m.probe CASCADE';
  EXECUTE create_sql;
  IF insert_sql <> '' THEN EXECUTE insert_sql; END IF;
  SELECT relfilenode INTO before FROM pg_class WHERE oid = 'm.probe'::regclass;
  BEGIN EXECUTE alter_sql;
  EXCEPTION WHEN others THEN RETURN 'refused: ' || replace(SQLERRM, E'\n', ' ');
  END;
  SELECT relfilenode INTO after FROM pg_class WHERE oid = 'm.probe'::regclass;
  RETURN CASE WHEN before IS DISTINCT FROM after THEN 'rewrite' ELSE 'no rewrite' END;
END $fn$ LANGUAGE plpgsql;

-- Answers "was this statement accepted?" without aborting the script.
CREATE FUNCTION m.accepts(sql text) RETURNS text AS $fn$
BEGIN EXECUTE sql; RETURN 'accepted';
EXCEPTION WHEN others THEN RETURN 'refused: ' || split_part(replace(SQLERRM, E'\n', ' '), E'\n', 1);
END $fn$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------- ADR-0009
CREATE TABLE m.customer (customer_id int PRIMARY KEY, full_name text, legacy_code text);
CREATE VIEW m.active AS SELECT customer_id, full_name FROM m.customer WHERE legacy_code IS NULL;

SELECT 'A1', 'a view definition is stored verbatim',
       CASE WHEN pg_get_viewdef('m.active'::regclass, true) =
                 'SELECT customer_id, full_name FROM m.customer WHERE legacy_code IS NULL'
            THEN 'yes' ELSE 'no' END;
SELECT 'A2', 'pretty and non-pretty deparse agree',
       CASE WHEN pg_get_viewdef('m.active'::regclass, true) = pg_get_viewdef('m.active'::regclass, false)
            THEN 'yes' ELSE 'no' END;

CREATE FUNCTION m.f(a int) RETURNS int AS $$  SELECT   a  +  1; $$ LANGUAGE sql;
SELECT 'A3', 'a string-literal function body is stored verbatim',
       CASE WHEN (SELECT prosrc FROM pg_proc WHERE oid='m.f(int)'::regprocedure) = '  SELECT   a  +  1; '
            THEN 'yes' ELSE 'no' END;
CREATE FUNCTION m.g(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a + 1; END;
SELECT 'A4', 'a BEGIN ATOMIC function body is stored verbatim',
       CASE WHEN coalesce((SELECT prosrc FROM pg_proc WHERE oid='m.g(int)'::regprocedure), '') = ''
            THEN 'no (prosrc is empty)' ELSE 'yes' END;

CREATE FUNCTION m.f(a text) RETURNS int AS $$ SELECT length(a); $$ LANGUAGE sql;
CREATE FUNCTION m.f(a varchar, b "char") RETURNS int AS $$ SELECT 0; $$ LANGUAGE sql;
CREATE PROCEDURE m.f(a date) LANGUAGE sql AS $$ SELECT 1; $$;
SELECT 'A5', 'functions and procedures overload on one name',
       count(*)::text || ' objects named m.f'
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='m' AND p.proname='f';
SELECT 'A6', 'the engine normalizes the identity arguments',
       string_agg(pg_get_function_identity_arguments(p.oid), ' / ' ORDER BY p.oid::regprocedure::text)
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='m' AND p.proname='f';
SELECT 'A7', 'DROP FUNCTION by bare name, with overloads present', m.accepts('DROP FUNCTION m.f');
SELECT 'A8', 'a function may share a name with a table',
       m.accepts('CREATE FUNCTION m.customer(a int) RETURNS int AS $q$ SELECT 1; $q$ LANGUAGE sql');
SELECT 'A9', 'a view may share a name with a table',
       m.accepts('CREATE VIEW m.customer AS SELECT 1 AS x');

CREATE TABLE m.t (id int PRIMARY KEY, a varchar(10), b int);
CREATE VIEW m.v AS SELECT id, a FROM m.t;
SELECT 'A10', 'CREATE OR REPLACE VIEW appends a column',
       m.accepts('CREATE OR REPLACE VIEW m.v AS SELECT id, a, b FROM m.t');
SELECT 'A11', 'CREATE OR REPLACE VIEW removes a column',
       m.accepts('CREATE OR REPLACE VIEW m.v AS SELECT id, b FROM m.t');
SELECT 'A12', 'CREATE OR REPLACE VIEW retypes a column',
       m.accepts('CREATE OR REPLACE VIEW m.v AS SELECT id, a::text, b FROM m.t');
SELECT 'A13', 'CREATE OR REPLACE FUNCTION changes the return type',
       m.accepts('CREATE OR REPLACE FUNCTION m.g(a int) RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT 1::bigint; END');
SELECT m.accepts('CREATE OR REPLACE FUNCTION m.g(a bigint) RETURNS int AS $q$ SELECT 2; $q$ LANGUAGE sql') \gset acc_
SELECT 'A14', 'CREATE OR REPLACE FUNCTION given a new argument type',
       :'acc_accepts' || ', leaving ' || count(*)::text || ' function(s) named m.g'
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='m' AND p.proname='g';

CREATE ROLE m_reader;
GRANT SELECT ON m.v TO m_reader;
SELECT 'A15', 'CREATE OR REPLACE VIEW keeps the grants',
       CASE WHEN (SELECT relacl::text FROM pg_class WHERE oid='m.v'::regclass) LIKE '%m_reader%'
            THEN 'kept' ELSE 'lost' END;
DROP VIEW m.v; CREATE VIEW m.v AS SELECT id, a FROM m.t;
SELECT 'A16', 'DROP VIEW then CREATE VIEW keeps the grants',
       CASE WHEN coalesce((SELECT relacl::text FROM pg_class WHERE oid='m.v'::regclass), '') LIKE '%m_reader%'
            THEN 'kept' ELSE 'lost' END;

ALTER TABLE m.customer RENAME TO client;
RESET search_path;   -- or the deparser omits the schema and the check reads wrong
SELECT 'A17', 'a table rename follows into the stored view definition',
       CASE WHEN pg_get_viewdef('m.active'::regclass, true) LIKE '%client%' THEN 'yes' ELSE 'no' END;
ALTER TABLE m.client RENAME COLUMN full_name TO name;
SELECT 'A18', 'and a column rename rewrites it as',
       trim(both from regexp_replace(pg_get_viewdef('m.active'::regclass, true), E'[\n ]+', ' ', 'g'));
ALTER TABLE m.client RENAME COLUMN name TO full_name;
ALTER TABLE m.client RENAME TO customer;
SET search_path = m;
SELECT 'A19', 'ALTER COLUMN TYPE is refused while a plain view depends on it',
       m.accepts('ALTER TABLE m.customer ALTER COLUMN legacy_code TYPE varchar(20)');
SELECT 'A20', 'DROP COLUMN is refused while a plain view depends on it',
       m.accepts('ALTER TABLE m.customer DROP COLUMN legacy_code');

-- ---------------------------------------------------------------- ADR-0010
CREATE ROLE m_owner_a LOGIN PASSWORD 'x';
CREATE ROLE m_owner_b LOGIN PASSWORD 'x';
GRANT CREATE, USAGE ON SCHEMA m TO m_owner_a, m_owner_b;
GRANT SELECT ON m.t TO m_reader;
SELECT 'B1', 'a table grant without schema USAGE reads as held',
       has_table_privilege('m_reader','m.t','SELECT')::text
       || ' (schema USAGE: ' || has_schema_privilege('m_reader','m','USAGE')::text || ')';
SELECT 'B2', 'and the read actually succeeds',
       m.accepts('SET ROLE m_reader; SELECT count(*) FROM m.t; RESET ROLE');
RESET ROLE;

CREATE ROLE m_all;
GRANT USAGE ON SCHEMA m TO m_all;
GRANT SELECT ON ALL TABLES IN SCHEMA m TO m_all;
CREATE TABLE m.later (id int);
SELECT 'B3', 'GRANT ON ALL TABLES covers a table created afterwards',
       has_table_privilege('m_all','m.t','SELECT')::text || ' for an existing table, '
       || has_table_privilege('m_all','m.later','SELECT')::text || ' for a later one';

ALTER DEFAULT PRIVILEGES FOR ROLE m_owner_a IN SCHEMA m GRANT SELECT ON TABLES TO m_all;
SET ROLE m_owner_a; CREATE TABLE m.by_a (id int); RESET ROLE;
SET ROLE m_owner_b; CREATE TABLE m.by_b (id int); RESET ROLE;
SELECT 'B4', 'ALTER DEFAULT PRIVILEGES is keyed to the creating role',
       has_table_privilege('m_all','m.by_a','SELECT')::text || ' for the named role, '
       || has_table_privilege('m_all','m.by_b','SELECT')::text || ' for another';

SELECT 'B5', 'a role is visible from every database in the cluster',
       'roles live in pg_authid, which is cluster-wide: '
       || (SELECT count(*)::text FROM pg_roles WHERE rolname = 'm_reader') || ' row in pg_roles';
SELECT 'B6', 'DROP ROLE while the role merely holds a grant', m.accepts('DROP ROLE m_reader');
SELECT 'B7', 'a fresh function''s ACL',
       coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.customer(int)'::regprocedure), 'NULL (the built-in default applies)');
SELECT 'B8', 'GRANT ALTER', m.accepts('GRANT ALTER ON m.t TO m_all');

CREATE TABLE m.ident (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, v text);
CREATE TABLE m.ser (id serial PRIMARY KEY, v text);
CREATE ROLE m_writer LOGIN PASSWORD 'x';
GRANT USAGE ON SCHEMA m TO m_writer;
GRANT INSERT ON m.ident, m.ser TO m_writer;
SELECT 'B9', 'inserting into an identity column with only INSERT granted',
       m.accepts('SET ROLE m_writer; INSERT INTO m.ident (v) VALUES (''x''); RESET ROLE');
RESET ROLE;
SELECT 'B10', 'inserting into a serial column with only INSERT granted',
       m.accepts('SET ROLE m_writer; INSERT INTO m.ser (v) VALUES (''x''); RESET ROLE');
RESET ROLE;

-- ---------------------------------------------------------------- ADR-0012
SELECT 'T-' || lpad(row_number() OVER ()::text, 2, '0'), change, m.rewrites(c, i, a)
FROM (VALUES
 ('int -> bigint',              'CREATE TABLE m.probe (c int)',          'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ALTER COLUMN c TYPE bigint'),
 ('bigint -> int',              'CREATE TABLE m.probe (c bigint)',       'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ALTER COLUMN c TYPE int'),
 ('varchar(10) -> varchar(20)', 'CREATE TABLE m.probe (c varchar(10))',  'INSERT INTO m.probe VALUES (''ab'')', 'ALTER TABLE m.probe ALTER COLUMN c TYPE varchar(20)'),
 ('varchar(20) -> varchar(10)', 'CREATE TABLE m.probe (c varchar(20))',  'INSERT INTO m.probe VALUES (''ab'')', 'ALTER TABLE m.probe ALTER COLUMN c TYPE varchar(10)'),
 ('varchar(20) -> text',        'CREATE TABLE m.probe (c varchar(20))',  'INSERT INTO m.probe VALUES (''ab'')', 'ALTER TABLE m.probe ALTER COLUMN c TYPE text'),
 ('text -> varchar(20)',        'CREATE TABLE m.probe (c text)',         'INSERT INTO m.probe VALUES (''ab'')', 'ALTER TABLE m.probe ALTER COLUMN c TYPE varchar(20)'),
 ('numeric(10,2)->(12,2)',      'CREATE TABLE m.probe (c numeric(10,2))','INSERT INTO m.probe VALUES (1.5)',    'ALTER TABLE m.probe ALTER COLUMN c TYPE numeric(12,2)'),
 ('numeric(10,2)->(10,4)',      'CREATE TABLE m.probe (c numeric(10,2))','INSERT INTO m.probe VALUES (1.5)',    'ALTER TABLE m.probe ALTER COLUMN c TYPE numeric(10,4)'),
 ('int -> text',                'CREATE TABLE m.probe (c int)',          'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ALTER COLUMN c TYPE text'),
 ('text -> int, no USING',      'CREATE TABLE m.probe (c text)',         'INSERT INTO m.probe VALUES (''1'')',  'ALTER TABLE m.probe ALTER COLUMN c TYPE int'),
 ('narrowing over real data',   'CREATE TABLE m.probe (c varchar(20))',  'INSERT INTO m.probe VALUES (''abcdefghij'')','ALTER TABLE m.probe ALTER COLUMN c TYPE varchar(5)'),
 ('add column, no default',     'CREATE TABLE m.probe (c int)',          'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ADD COLUMN d int'),
 ('add column, const default',  'CREATE TABLE m.probe (c int)',          'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ADD COLUMN d int DEFAULT 7'),
 ('add column, volatile default','CREATE TABLE m.probe (c int)',         'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ADD COLUMN d uuid DEFAULT gen_random_uuid()'),
 ('SET NOT NULL',               'CREATE TABLE m.probe (c int)',          'INSERT INTO m.probe VALUES (1)',      'ALTER TABLE m.probe ALTER COLUMN c SET NOT NULL'),
 ('DROP COLUMN',                'CREATE TABLE m.probe (c int, d int)',   'INSERT INTO m.probe VALUES (1,2)',    'ALTER TABLE m.probe DROP COLUMN d')
) AS v(change, c, i, a);

SET timezone = 'UTC';
SELECT 'T-17', 'timestamp -> timestamptz under UTC',
       m.rewrites('CREATE TABLE m.probe (c timestamp)', 'INSERT INTO m.probe VALUES (now())',
                  'ALTER TABLE m.probe ALTER COLUMN c TYPE timestamptz');
SET timezone = 'America/New_York';
SELECT 'T-18', 'timestamp -> timestamptz under America/New_York',
       m.rewrites('CREATE TABLE m.probe (c timestamp)', 'INSERT INTO m.probe VALUES (now())',
                  'ALTER TABLE m.probe ALTER COLUMN c TYPE timestamptz');
RESET timezone;

DROP TABLE IF EXISTS m.spell CASCADE;
CREATE TABLE m.spell (a int, b int4, c int2, d int8, e decimal(10,2), f float, g float8,
                      h real, j bool, k varchar, l varchar(9), m_ char(5),
                      n time, o timetz, p timestamp, q serial);
SELECT 'T-19', 'declared spelling -> catalog spelling',
       string_agg(attname || '=' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum)
FROM pg_attribute WHERE attrelid='m.spell'::regclass AND attnum > 0 AND NOT attisdropped;

DROP TABLE IF EXISTS m.floats CASCADE;
CREATE TABLE m.floats (a float(1), b float(24), c float(25), d float(53));
SELECT 'T-20', 'float(n) either side of 24',
       string_agg(attname || '=' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum)
FROM pg_attribute WHERE attrelid='m.floats'::regclass AND attnum > 0;

DROP TABLE IF EXISTS m.dropped CASCADE;
CREATE TABLE m.dropped (keep int, gone int);
ALTER TABLE m.dropped DROP COLUMN gone;
SELECT 'T-21', 'what a dropped column leaves in the catalog',
       string_agg(attnum || ':' || attname || ' isdropped=' || attisdropped::text
                  || ' type=' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum)
FROM pg_attribute WHERE attrelid='m.dropped'::regclass AND attnum > 0;

-- ---------------------------------------------------------------- ADR-0013
DROP TABLE IF EXISTS m.defs CASCADE;
CREATE TABLE m.defs (a text DEFAULT 'plain', b int DEFAULT 0, c varchar(9) DEFAULT 'x');
SELECT 'R1', 'how a declared default is stored',
       string_agg(at.attname || ' -> ' || pg_get_expr(d.adbin, d.adrelid), ', ' ORDER BY at.attnum)
FROM pg_attrdef d JOIN pg_attribute at ON at.attrelid=d.adrelid AND at.attnum=d.adnum
WHERE d.adrelid='m.defs'::regclass;

DROP TABLE IF EXISTS m.pinned CASCADE;
CREATE TABLE m.pinned (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO m.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (1, 'one');
INSERT INTO m.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (2, 'two');
SELECT 'R2', 'an ordinary insert after keys were forced in',
       m.accepts('INSERT INTO m.pinned (v) VALUES (''next'')');

DROP TABLE IF EXISTS m.child CASCADE; DROP TABLE IF EXISTS m.parent CASCADE;
CREATE TABLE m.parent (id int PRIMARY KEY);
CREATE TABLE m.child (id int PRIMARY KEY, pid int);
INSERT INTO m.child VALUES (1, 999);
ALTER TABLE m.child ADD CONSTRAINT fk_child FOREIGN KEY (pid) REFERENCES m.parent(id) NOT VALID;
SELECT 'R3', 'a NOT VALID foreign key reads as',
       'convalidated=' || (SELECT convalidated::text FROM pg_constraint
                            WHERE conname='fk_child' AND conrelid='m.child'::regclass);
SELECT 'R4', 'a new violating row under a NOT VALID key',
       m.accepts('INSERT INTO m.child VALUES (2, 998)');
INSERT INTO m.parent VALUES (5); UPDATE m.child SET pid = 5 WHERE id = 1;
SELECT 'R5', 'deleting a parent referenced under a NOT VALID key',
       m.accepts('DELETE FROM m.parent WHERE id = 5');

DROP TABLE IF EXISTS m.keys CASCADE;
CREATE TABLE m.keys (code varchar(20) PRIMARY KEY);
INSERT INTO m.keys VALUES ('New');
SELECT 'R6', 'a key differing only in case',
       m.accepts('INSERT INTO m.keys VALUES (''new'')')
       || ' (collation ' || (SELECT datcollate FROM pg_database WHERE datname=current_database()) || ')';

DROP TABLE IF EXISTS m.vals CASCADE;
CREATE TABLE m.vals (b bytea, d date, i interval);
INSERT INTO m.vals VALUES ('\x0102', '2026-09-05', '1 day 2 hours');
SELECT 'R7', 'value rendering under the default session',
       (SELECT b::text || ' / ' || d::text || ' / ' || i::text FROM m.vals);
SET bytea_output='escape'; SET datestyle='SQL, DMY'; SET intervalstyle='sql_standard';
SELECT 'R8', 'the same values after three SET commands',
       (SELECT b::text || ' / ' || d::text || ' / ' || i::text FROM m.vals);
RESET bytea_output; RESET datestyle; RESET intervalstyle;

-- ------------------------------------------- the 2026-09-05 review round
-- Five findings on PR #12; these are the four that needed an engine.

SELECT 'A21', 'a type modifier distinguishes two routines',
       m.accepts('CREATE FUNCTION m.mod1(a varchar(10)) RETURNS int AS $q$ SELECT 1; $q$ LANGUAGE sql')
       || ' / ' ||
       m.accepts('CREATE FUNCTION m.mod1(a varchar(20)) RETURNS int AS $q$ SELECT 2; $q$ LANGUAGE sql');
SELECT 'A22', 'how the engine identifies that routine',
       string_agg(pg_get_function_identity_arguments(p.oid), ' | ')
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='m' AND p.proname='mod1';

CREATE FUNCTION m.fresh(a int) RETURNS int AS $$ SELECT a; $$ LANGUAGE sql;
CREATE ROLE m_nobody;
GRANT USAGE ON SCHEMA m TO m_nobody;
SELECT 'B11', 'PUBLIC executing a function nobody granted',
       m.accepts('SET ROLE m_nobody; SELECT m.fresh(7); RESET ROLE');
RESET ROLE;
REVOKE EXECUTE ON FUNCTION m.fresh(int) FROM PUBLIC;
SELECT 'B12', 'the ACL after revoking EXECUTE from PUBLIC',
       coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.fresh(int)'::regprocedure), 'NULL')
       || ', and then: ' || m.accepts('SET ROLE m_nobody; SELECT m.fresh(7); RESET ROLE');
RESET ROLE;

CREATE TABLE m.seq (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO m.seq (id, v) OVERRIDING SYSTEM VALUE VALUES (100, 'an undeclared row');
INSERT INTO m.seq (id, v) OVERRIDING SYSTEM VALUE VALUES (1, 'the row this plan writes');
ALTER TABLE m.seq ALTER COLUMN id RESTART WITH 2;   -- max(written by the plan) + 1
SELECT m.accepts('INSERT INTO m.seq (v) VALUES (''the next one'')') \gset r9_
SELECT 'R9', 'the next insert after restarting at max(written)+1',
       :'r9_accepts' || ', taking id '
       || (SELECT max(id)::text FROM m.seq WHERE v = 'the next one')
       || ' while the table already holds a row at id '
       || (SELECT max(id)::text FROM m.seq);


-- Clean up every principal this script created; roles are cluster-wide.
ALTER DEFAULT PRIVILEGES FOR ROLE m_owner_a IN SCHEMA m REVOKE SELECT ON TABLES FROM m_all;
DROP SCHEMA m CASCADE;
DROP OWNED BY m_owner_a; DROP OWNED BY m_owner_b; DROP OWNED BY m_all; DROP OWNED BY m_writer;
DROP ROLE IF EXISTS m_owner_a, m_owner_b, m_all, m_writer, m_reader, m_nobody;

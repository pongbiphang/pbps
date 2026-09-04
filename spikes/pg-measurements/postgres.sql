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
-- The replace has to happen *after* the grant, or this measures only that the
-- GRANT worked. A15 read `kept` for that reason in the first version of this
-- file, and would have gone on reading `kept` if a later PostgreSQL dropped
-- privileges during a replacement.
SELECT m.accepts('CREATE OR REPLACE VIEW m.v AS SELECT id, a, b, id AS also_id FROM m.t') \gset a15_
SELECT 'A15', 'CREATE OR REPLACE VIEW keeps the grants',
       :'a15_accepts' || ', and the grant is '
       || CASE WHEN (SELECT relacl::text FROM pg_class WHERE oid='m.v'::regclass) LIKE '%m_reader%'
               THEN 'kept' ELSE 'LOST' END;
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

CREATE DATABASE m_other;
\connect m_other
SELECT 'B5', 'a role created in another database, seen from this one',
       'connected to ' || current_database()
       || ', pg_roles rows for m_reader: '
       || (SELECT count(*)::text FROM pg_roles WHERE rolname = 'm_reader');
\connect postgres
SET search_path = m;
DROP DATABASE m_other;
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


-- ------------------------------------ the second 2026-09-05 review round
-- Three more findings on PR #12, all consequences of the first round's fixes.

CREATE FUNCTION m.reb(a int) RETURNS int AS $$ SELECT a; $$ LANGUAGE sql;
REVOKE EXECUTE ON FUNCTION m.reb(int) FROM PUBLIC;
SELECT 'A23', 'the ACL after an operator revokes EXECUTE from PUBLIC',
       coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.reb(int)'::regprocedure), 'NULL');
DROP FUNCTION m.reb(int);
CREATE FUNCTION m.reb(a int) RETURNS bigint AS $$ SELECT a::bigint; $$ LANGUAGE sql;
SELECT 'A24', 'the ACL after a drop-and-create rebuild of that function',
       coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.reb(int)'::regprocedure),
                'NULL — the default is back, and PUBLIC can execute it again');

-- Can "try the replace, and recover" be implemented? A failed statement dooms
-- a PostgreSQL transaction; ROLLBACK TO SAVEPOINT un-dooms it.
CREATE FUNCTION m.sp(a int) RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;
CREATE TABLE m.evidence (note text);
BEGIN;
INSERT INTO m.evidence VALUES ('before the attempt');
SAVEPOINT try_replace;
CREATE OR REPLACE FUNCTION m.sp(a int) RETURNS bigint AS $$ SELECT 1::bigint; $$ LANGUAGE sql;
ROLLBACK TO SAVEPOINT try_replace;
INSERT INTO m.evidence VALUES ('after rolling back to the savepoint');
COMMIT;
SELECT 'A25', 'work surviving a failed CREATE OR REPLACE inside a savepoint',
       count(*)::text || ' of 2 rows committed' FROM m.evidence;

SELECT 'R10', 'a descending identity, with no MAXVALUE the model could carry',
       m.accepts('CREATE TABLE m.desc_id (id int GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY -1) PRIMARY KEY)');
CREATE TABLE m.desc_ok (id int GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY -1 MAXVALUE 100 MINVALUE -1000) PRIMARY KEY, v text);
INSERT INTO m.desc_ok (id, v) OVERRIDING SYSTEM VALUE VALUES (100, 'the pinned row');
SELECT 'R11', 'RESTART at max(keys)+1 on a descending identity',
       m.accepts('ALTER TABLE m.desc_ok ALTER COLUMN id RESTART WITH 101');
SELECT 'R12', 'RESTART at min(keys)-1, the direction the sequence counts',
       m.accepts('ALTER TABLE m.desc_ok ALTER COLUMN id RESTART WITH 99')
       || ' / ' || m.accepts('INSERT INTO m.desc_ok (v) VALUES (''generated'')');

SELECT 'R13', 'a backslash-escaped quote inside an E-string',
       'standard_conforming_strings=' || current_setting('standard_conforming_strings')
       || ', E''it\''s  here'' is ' || length(E'it\'s  here')::text
       || ' characters: ' || E'it\'s  here';

-- ------------------------------------- the fourth 2026-09-05 review round

CREATE ROLE m_owner LOGIN PASSWORD 'x';
GRANT CREATE, USAGE ON SCHEMA m TO m_owner;
SET ROLE m_owner;
CREATE FUNCTION m.owned(a int) RETURNS int SECURITY DEFINER AS $$ SELECT a; $$ LANGUAGE sql;
RESET ROLE;
SELECT 'A26', 'a SECURITY DEFINER function before a rebuild',
       'owner=' || (SELECT proowner::regrole::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure)
       || ' secdef=' || (SELECT prosecdef::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure)
       || ' acl=' || coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure), 'NULL');
DROP FUNCTION m.owned(int);
CREATE FUNCTION m.owned(a int) RETURNS bigint SECURITY DEFINER AS $$ SELECT a::bigint; $$ LANGUAGE sql;
SELECT 'A27', 'the same function after the deployment account rebuilds it',
       'owner=' || (SELECT proowner::regrole::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure)
       || ' secdef=' || (SELECT prosecdef::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure)
       || ' acl=' || coalesce((SELECT proacl::text FROM pg_proc WHERE oid='m.owned(int)'::regprocedure),
                              'NULL — so the ACL check cannot see the change');

CREATE TABLE m.lockseq (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO m.lockseq (v) VALUES ('a');
BEGIN;
SELECT max(id) FROM m.lockseq;
SELECT 'R14', 'the lock held after only reading max(id)',
       coalesce((SELECT string_agg(DISTINCT mode, ', ') FROM pg_locks
                 WHERE relation='m.lockseq'::regclass AND pid=pg_backend_pid()), '(none)')
       ;   -- what that lock permits is argued in the ADR, not observed here
ALTER TABLE m.lockseq ALTER COLUMN id RESTART WITH 2;
SELECT 'R15', 'the lock held once the RESTART runs',
       (SELECT string_agg(DISTINCT mode, ', ') FROM pg_locks
        WHERE relation='m.lockseq'::regclass AND pid=pg_backend_pid())
       ;   -- likewise: the ordering is the observation, the window is the argument
COMMIT;

-- -------------------------------------- the fifth 2026-09-05 review round

CREATE TABLE m.opt_t (id int PRIMARY KEY, a text);
CREATE VIEW m.opt_v WITH (security_invoker = true, security_barrier = true)
  AS SELECT id, a FROM m.opt_t;
SELECT 'A28', 'options a view carries outside its definition',
       'reloptions: ' || coalesce((SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE oid='m.opt_v'::regclass), 'NULL')
       || ' / pg_get_viewdef shows: '
       || trim(both from regexp_replace(pg_get_viewdef('m.opt_v'::regclass, true), E'[\n ]+', ' ', 'g'));
SELECT m.accepts('CREATE OR REPLACE VIEW m.opt_v AS SELECT id, a, id AS also FROM m.opt_t') \gset a29_
SELECT 'A29', 'those options after CREATE OR REPLACE',
       :'a29_accepts' || ', reloptions: '
       || coalesce((SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE oid='m.opt_v'::regclass), 'NULL — lost');
DROP VIEW m.opt_v;
CREATE VIEW m.opt_v AS SELECT id, a FROM m.opt_t;
SELECT 'A30', 'those options after a drop-and-create rebuild',
       'reloptions: '
       || coalesce((SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE oid='m.opt_v'::regclass), 'NULL — lost');

-- What losing security_invoker actually does to a reader with no rights on the
-- underlying table.
CREATE ROLE m_sreader;
GRANT USAGE ON SCHEMA m TO m_sreader;
DROP VIEW m.opt_v;
CREATE VIEW m.opt_v WITH (security_invoker = true) AS SELECT id, a FROM m.opt_t;
GRANT SELECT ON m.opt_v TO m_sreader;
SELECT 'A31', 'a reader querying the view while security_invoker=true',
       m.accepts('SET ROLE m_sreader; SELECT * FROM m.opt_v; RESET ROLE');
RESET ROLE;
DROP VIEW m.opt_v;
CREATE VIEW m.opt_v AS SELECT id, a FROM m.opt_t;   -- the rebuild pbps would emit
GRANT SELECT ON m.opt_v TO m_sreader;
SELECT 'A32', 'the same reader after a rebuild dropped the option',
       m.accepts('SET ROLE m_sreader; SELECT * FROM m.opt_v; RESET ROLE');
RESET ROLE;

-- -------------------------------------- the sixth 2026-09-05 review round

CREATE TABLE m.step (id int GENERATED ALWAYS AS IDENTITY (START WITH 1 INCREMENT BY 2) PRIMARY KEY, v text);
INSERT INTO m.step (v) VALUES ('a'), ('b'), ('c'), ('d'), ('e'), ('f');
SELECT 'R16', 'the keys a step-2 generator owns',
       (SELECT string_agg(id::text, ', ' ORDER BY id) FROM m.step);
INSERT INTO m.step (id, v) OVERRIDING SYSTEM VALUE VALUES (13, 'a pinned row');
ALTER TABLE m.step ALTER COLUMN id RESTART WITH 14;   -- max(keys) + 1
INSERT INTO m.step (v) VALUES ('after'), ('and again');
SELECT 'R17', 'the same generator after restarting at max(keys)+1',
       (SELECT string_agg(id::text, ', ' ORDER BY id) FROM m.step)
       || ' — it has crossed onto the even series';

CREATE TABLE m.sp_t (id int PRIMARY KEY, a text);
SET search_path = '';
SELECT 'R18', 'an unqualified reference inside a definition, search_path empty',
       m.accepts('CREATE VIEW m.sp_v AS SELECT id, a FROM sp_t');
SET search_path = m;
SELECT 'R19', 'the same definition with the object''s own schema on the path',
       m.accepts('CREATE VIEW m.sp_v AS SELECT id, a FROM sp_t');

CREATE VIEW m.uv AS SELECT id, a FROM m.sp_t;
ALTER VIEW m.uv ALTER COLUMN a SET DEFAULT 'from the view default';
SELECT 'A33', 'a view column default, and where it lives',
       'pg_attrdef: ' || (SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d WHERE d.adrelid='m.uv'::regclass)
       || ' / pg_get_viewdef shows: '
       || trim(both from regexp_replace(pg_get_viewdef('m.uv'::regclass, true), E'[\n ]+', ' ', 'g'));
DROP VIEW m.uv; CREATE VIEW m.uv AS SELECT id, a FROM m.sp_t;
SELECT 'A34', 'that default after a drop-and-create rebuild',
       coalesce((SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d WHERE d.adrelid='m.uv'::regclass),
                'gone');

-- ----------------------------------- the seventh 2026-09-05 review round

CREATE TABLE m.lockable (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
SELECT 'R20', 'locking a sequence with LOCK TABLE',
       m.accepts('LOCK TABLE ' || pg_get_serial_sequence('m.lockable','id') || ' IN ACCESS EXCLUSIVE MODE');
SELECT 'R21', 'locking the sequence''s row with FOR UPDATE',
       m.accepts('SELECT last_value FROM ' || pg_get_serial_sequence('m.lockable','id') || ' FOR UPDATE');

-- Since no lock conflicts with nextval, the restart must be a write that cannot
-- move the sequence backwards, whatever another session did in the meantime.
CREATE TABLE m.never_lower (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO m.never_lower (id, v) OVERRIDING SYSTEM VALUE VALUES (100, 'pinned');
SELECT 'R22', 'setval(GREATEST(target, nextval)) with the sequence behind the target',
       'returned ' || setval(pg_get_serial_sequence('m.never_lower','id'),
                             GREATEST(101, nextval(pg_get_serial_sequence('m.never_lower','id'))))::text;
SELECT nextval(pg_get_serial_sequence('m.never_lower','id')) \gset n1_
SELECT nextval(pg_get_serial_sequence('m.never_lower','id')) \gset n2_
SELECT 'R23', 'the same call after another allocation pushed it past the target',
       'sequence had reached ' || :'n2_nextval'
       || ', setval(GREATEST(101, nextval)) returned '
       || setval(pg_get_serial_sequence('m.never_lower','id'),
                 GREATEST(101, nextval(pg_get_serial_sequence('m.never_lower','id'))))::text
       || ' — it did not go back';

CREATE FUNCTION m.scs() RETURNS text AS $fn$
DECLARE n int;
BEGIN
  EXECUTE 'SELECT length(' || chr(39) || 'it' || chr(92) || chr(39) || 's  here' || chr(39) || ')' INTO n;
  RETURN 'one literal of length ' || n::text;
EXCEPTION WHEN others THEN RETURN 'refused: ' || split_part(SQLERRM, E'\n', 1);
END $fn$ LANGUAGE plpgsql;
SET standard_conforming_strings = on;
SELECT 'R24', 'a plain literal with a backslash-quote, standard_conforming_strings=on', m.scs();
SET standard_conforming_strings = off;
SELECT 'R25', 'the same literal with standard_conforming_strings=off', m.scs();
SET standard_conforming_strings = on;

CREATE TABLE m.sp2 (id int PRIMARY KEY, a text);
CREATE VIEW m.sp2v AS SELECT id, a FROM m.sp2;
SET search_path = '';
SELECT 'R26', 'pg_get_viewdef with an empty search_path',
       trim(both from regexp_replace(pg_get_viewdef('m.sp2v'::regclass, true), E'[\n ]+',' ','g'));
SET search_path = m;
SELECT 'R27', 'pg_get_viewdef with the object''s schema on the path',
       trim(both from regexp_replace(pg_get_viewdef('m.sp2v'::regclass, true), E'[\n ]+',' ','g'));

CREATE SCHEMA m_ext;
CREATE FUNCTION m_ext.helper(a int) RETURNS int AS $$ SELECT a * 2; $$ LANGUAGE sql;
SET search_path = m;
SELECT 'R28', 'an unqualified function from another schema, path = the object''s schema only',
       m.accepts('CREATE VIEW m.uses AS SELECT id, helper(id) AS h FROM m.sp2');
SET search_path = m, m_ext;
SELECT 'R29', 'the same definition with that other schema on the path',
       m.accepts('CREATE VIEW m.uses AS SELECT id, helper(id) AS h FROM m.sp2');
SET search_path = m;
DROP SCHEMA m_ext CASCADE;

-- ------------------------------------ the eighth 2026-09-05 review round

CREATE TABLE m.race (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO m.race (id, v) OVERRIDING SYSTEM VALUE VALUES (100, 'pinned');
SELECT setval(pg_get_serial_sequence('m.race','id'), 104) \gset seed_
SELECT nextval(pg_get_serial_sequence('m.race','id')) \gset inner_
SELECT nextval(pg_get_serial_sequence('m.race','id')) \gset other_
SELECT setval(pg_get_serial_sequence('m.race','id'), GREATEST(101, :inner_nextval)) \gset wrote_
SELECT nextval(pg_get_serial_sequence('m.race','id')) \gset after_
SELECT 'R30', 'setval(GREATEST(target, nextval)) interleaved with one other allocation',
       'inner nextval ' || :'inner_nextval'
       || ', another session took ' || :'other_nextval'
       || ', setval wrote ' || :'wrote_setval'
       || ', next caller receives ' || :'after_nextval'
       || CASE WHEN :after_nextval = :other_nextval THEN ' — already issued' ELSE ' — no collision' END;

CREATE SEQUENCE m.fresh_seq;
ALTER SEQUENCE m.fresh_seq INCREMENT BY 100;
SELECT nextval('m.fresh_seq') \gset f1_
SELECT nextval('m.fresh_seq') \gset f2_
SELECT 'R31', 'a widened INCREMENT on a sequence never called',
       'first nextval ' || :'f1_nextval' || ', second ' || :'f2_nextval'
       || ' — the increment does not apply to the first call';

SELECT 'R32', 'ALTER SEQUENCE INCREMENT BY does not move last_value',
       (SELECT 'last_value ' || last_value::text FROM m.fresh_seq) AS before_alter;
ALTER SEQUENCE m.fresh_seq INCREMENT BY 1;
SELECT 'R33', 'and after restoring the increment',
       (SELECT 'last_value ' || last_value::text || ' — nothing was lowered' FROM m.fresh_seq);

-- ------------------------------------- the ninth 2026-09-05 review round

CREATE ROLE m_deploy LOGIN PASSWORD 'x';
CREATE ROLE m_bystander;
GRANT CREATE, USAGE ON SCHEMA m TO m_deploy;
CREATE TABLE m.dp_t (id int PRIMARY KEY, a text);
ALTER TABLE m.dp_t OWNER TO m_deploy;
ALTER DEFAULT PRIVILEGES FOR ROLE m_deploy IN SCHEMA m GRANT SELECT ON TABLES TO m_bystander;
SET ROLE m_deploy;
CREATE VIEW m.dp_v AS SELECT id, a FROM m.dp_t;
RESET ROLE;
SELECT 'A35', 'the ACL of a view the deployment role just created',
       coalesce((SELECT relacl::text FROM pg_class WHERE oid='m.dp_v'::regclass), 'NULL')
       || ' — granted by no declaration';
SELECT 'A36', 'so a rebuild of an object whose old ACL was NULL',
       'old acl NULL passes an "reproduce the old ACL" check, new acl is '
       || coalesce((SELECT relacl::text FROM pg_class WHERE oid='m.dp_v'::regclass), 'NULL');

SET search_path = '';
SELECT 'R34', 'a catalog query with an empty search_path',
       count(*)::text || ' row(s) from pg_class — pg_catalog stays reachable'
FROM pg_class WHERE relname = 'dp_t';
SELECT 'R35', 'deparse under an empty path is fully qualified',
       trim(both from regexp_replace(pg_get_viewdef('m.dp_v'::regclass, true), E'[\n ]+',' ','g'));
SET search_path = m;
SELECT 'R36', 'the same view deparsed under the project path',
       trim(both from regexp_replace(pg_get_viewdef('m.dp_v'::regclass, true), E'[\n ]+',' ','g'));

ALTER DEFAULT PRIVILEGES FOR ROLE m_deploy IN SCHEMA m REVOKE SELECT ON TABLES FROM m_bystander;

-- ------------------------------------- the tenth 2026-09-05 review round

CREATE SCHEMA m_a; CREATE SCHEMA m_b;
CREATE TABLE m_a.t (id int PRIMARY KEY, marker text);
CREATE TABLE m_b.t (id int PRIMARY KEY, marker text);
INSERT INTO m_a.t VALUES (1, 'schema m_a');
INSERT INTO m_b.t VALUES (1, 'schema m_b');
SET search_path = m_a, m_b;
CREATE VIEW m_b.v AS SELECT id, marker FROM t;
SELECT 'R37', 'a view in m_b created under a path ordered (m_a, m_b)',
       'binds to ' || (SELECT marker FROM m_b.v)
       || ', and its stored definition is only: '
       || trim(both from regexp_replace(pg_get_viewdef('m_b.v'::regclass, true), E'[\n ]+',' ','g'));
SET search_path = m_b, m_a;
CREATE VIEW m_b.v2 AS SELECT id, marker FROM t;
SELECT 'R38', 'the same view with its own schema first',
       'binds to ' || (SELECT marker FROM m_b.v2);
SET search_path = m;
DROP SCHEMA m_a CASCADE; DROP SCHEMA m_b CASCADE;

CREATE TABLE m.cust (id int PRIMARY KEY, full_name text);
CREATE VIEW m.act AS SELECT id, full_name FROM m.cust;
ALTER TABLE m.cust RENAME TO clnt;
SET search_path = '';
SELECT 'A37', 'what a table rename leaves in the view''s stored definition',
       trim(both from regexp_replace(pg_get_viewdef('m.act'::regclass, true), E'[\n ]+',' ','g'));
SET search_path = m;
SELECT 'A38', 'recreating that view from the unchanged declaration text',
       m.accepts('CREATE VIEW m.rebuilt AS SELECT id, full_name FROM m.cust');

-- ---------------------------------- the twelfth 2026-09-05 review round

CREATE TABLE m.orders (id int PRIMARY KEY);
CREATE TABLE m.customers (id int PRIMARY KEY);
CREATE FUNCTION m.noop() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql;
CREATE TRIGGER audit AFTER INSERT ON m.orders FOR EACH ROW EXECUTE FUNCTION m.noop();
CREATE TRIGGER audit AFTER INSERT ON m.customers FOR EACH ROW EXECUTE FUNCTION m.noop();
SELECT 'A39', 'two triggers of the same name in one schema',
       string_agg(c.relname || '.' || t.tgname, ', ' ORDER BY c.relname)
FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'm' AND NOT t.tgisinternal;
SELECT 'A40', 'DROP TRIGGER without naming the table', m.accepts('DROP TRIGGER audit');
SELECT 'A41', 'DROP TRIGGER naming the table', m.accepts('DROP TRIGGER audit ON m.orders');

-- -------------------------------- the thirteenth 2026-09-05 review round

CREATE FUNCTION m.fdep(a int) RETURNS int IMMUTABLE AS $$ SELECT a * 2 $$ LANGUAGE sql;
CREATE TABLE m.d_chk (id int PRIMARY KEY, v int CHECK (m.fdep(v) > 0));
SELECT 'A42', 'DROP FUNCTION with a managed check constraint on it',
       m.accepts('DROP FUNCTION m.fdep(int)');
DROP TABLE m.d_chk;
CREATE TABLE m.d_def (id int PRIMARY KEY, v int DEFAULT m.fdep(1));
SELECT 'A43', 'DROP FUNCTION with a column default on it',
       m.accepts('DROP FUNCTION m.fdep(int)');
DROP TABLE m.d_def;
CREATE TABLE m.d_gen (id int PRIMARY KEY, v int, g int GENERATED ALWAYS AS (m.fdep(v)) STORED);
SELECT 'A44', 'DROP FUNCTION with a generated column on it',
       m.accepts('DROP FUNCTION m.fdep(int)');
DROP TABLE m.d_gen;
CREATE TABLE m.d_idx (id int PRIMARY KEY, v int);
CREATE INDEX ix_fdep ON m.d_idx (m.fdep(v));
SELECT 'A45', 'DROP FUNCTION with an expression index on it',
       m.accepts('DROP FUNCTION m.fdep(int)');

-- ------------------------------- the fourteenth 2026-09-05 review round

CREATE SEQUENCE m.roll;
BEGIN;
SELECT nextval('m.roll') FROM generate_series(1,5) \gset r_
ROLLBACK;
SELECT 'R39', 'a sequence advance after the transaction that made it rolled back',
       'last_value ' || (SELECT last_value::text FROM m.roll) || ' — the advance survived';

CREATE SEQUENCE m.cyc START 8 MINVALUE 1 MAXVALUE 10 CYCLE;
SELECT 'R40', 'nextval on a CYCLE sequence, six calls',
       (SELECT string_agg(nextval('m.cyc')::text, ', ') FROM generate_series(1,6))
       || ' — not monotonic, so "advance until past the maximum" never ends';
SELECT 'R41', 'what the catalog says about that sequence',
       'cycle=' || seqcycle::text || ' min=' || seqmin::text || ' max=' || seqmax::text
       || ' — detectable, though Identity records only seed and increment'
FROM pg_sequence WHERE seqrelid = 'm.cyc'::regclass;

-- -------------------------------- the fifteenth 2026-09-05 review round
-- The two ownership prerequisites. Both refusals need a *non-superuser*
-- session — a superuser bypasses them — so the session transcripts are in
-- ADR-0009 and what one connection can establish is the catalog state.

CREATE ROLE m_ow_owner;
CREATE ROLE m_ow_deploy LOGIN PASSWORD 'x';
GRANT CREATE, USAGE ON SCHEMA m TO m_ow_owner;
SELECT 'A46', 'the target owner''s CREATE on the containing schema',
       has_schema_privilege('m_ow_owner','m','CREATE')::text || ' — the prerequisite ALTER ... OWNER TO checks';
REVOKE CREATE ON SCHEMA m FROM m_ow_owner;
SELECT 'A47', 'the same after REVOKE CREATE',
       has_schema_privilege('m_ow_owner','m','CREATE')::text
       || ' — an object may still be validly owned by it';
GRANT m_ow_owner TO m_ow_deploy WITH SET FALSE;
SELECT 'A48', 'membership granted WITH SET FALSE',
       'set_option=' || set_option::text || ' — membership without the right to assume it'
FROM pg_auth_members WHERE roleid='m_ow_owner'::regrole AND member='m_ow_deploy'::regrole;

-- Clean up every principal this script created; roles are cluster-wide.
ALTER DEFAULT PRIVILEGES FOR ROLE m_owner_a IN SCHEMA m REVOKE SELECT ON TABLES FROM m_all;
DROP SCHEMA m CASCADE;
DROP OWNED BY m_owner_a; DROP OWNED BY m_owner_b; DROP OWNED BY m_all; DROP OWNED BY m_writer; DROP OWNED BY m_owner; DROP OWNED BY m_sreader; DROP OWNED BY m_deploy; DROP OWNED BY m_bystander; DROP OWNED BY m_ow_owner; DROP OWNED BY m_ow_deploy;
DROP ROLE IF EXISTS m_owner_a, m_owner_b, m_all, m_writer, m_reader, m_nobody, m_owner, m_sreader, m_deploy, m_bystander, m_ow_owner, m_ow_deploy;

-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE TYPE app.int42;
CREATE FUNCTION app.int42_in(cstring) RETURNS app.int42 AS 'int4in' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION app.int42_out(app.int42) RETURNS cstring AS 'int4out' LANGUAGE internal IMMUTABLE STRICT;
CREATE TYPE app.int42 (INPUT=app.int42_in, OUTPUT=app.int42_out, INTERNALLENGTH=4, PASSEDBYVALUE, ALIGNMENT=int4, STORAGE=plain);

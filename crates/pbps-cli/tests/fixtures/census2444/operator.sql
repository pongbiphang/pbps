-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE OPERATOR app.=== (LEFTARG=integer,RIGHTARG=integer,FUNCTION=int4eq);

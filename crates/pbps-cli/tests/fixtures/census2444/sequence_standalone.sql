-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
-- A plain standalone sequence is held (#2721), so the census names the one the model
-- cannot hold: an unlogged one.
CREATE UNLOGGED SEQUENCE app.lonely;

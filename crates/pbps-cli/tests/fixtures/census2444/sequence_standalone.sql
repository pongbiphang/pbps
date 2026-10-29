-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE SEQUENCE app.lonely;

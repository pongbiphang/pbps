-- Before running: docker exec pbps-2441-pg16 (or pg18) install -d -o postgres -g postgres /tmp/pbps2444_tablespace
-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE TABLESPACE inv_space LOCATION '/tmp/pbps2444_tablespace';

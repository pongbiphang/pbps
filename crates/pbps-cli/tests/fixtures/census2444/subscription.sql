-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE SUBSCRIPTION inv_sub CONNECTION 'host=127.0.0.1 port=1 dbname=unused' PUBLICATION absent WITH (connect=false, enabled=false, create_slot=false, slot_name=NONE);

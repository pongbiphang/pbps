-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE TRANSFORM FOR integer LANGUAGE plpgsql (FROM SQL WITH FUNCTION dsimple_init(internal), TO SQL WITH FUNCTION int4recv(internal));

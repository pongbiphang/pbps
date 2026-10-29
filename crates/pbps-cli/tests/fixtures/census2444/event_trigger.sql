-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE FUNCTION app.ddl_event() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN RETURN; END$$; CREATE EVENT TRIGGER inv_ddl ON ddl_command_end EXECUTE FUNCTION app.ddl_event();

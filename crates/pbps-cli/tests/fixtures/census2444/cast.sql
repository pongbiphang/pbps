-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE FUNCTION app.i_to_xml(integer) RETURNS xml LANGUAGE sql IMMUTABLE AS $$SELECT xmlparse(content $1::text)$$; CREATE CAST (integer AS xml) WITH FUNCTION app.i_to_xml(integer) AS ASSIGNMENT;

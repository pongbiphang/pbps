-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);

-- Candidate added after the ordinary-table baseline.
CREATE OPERATOR CLASS app.my_int_ops FOR TYPE integer USING btree AS OPERATOR 1 <(integer,integer), OPERATOR 2 <=(integer,integer), OPERATOR 3 =(integer,integer), OPERATOR 4 >=(integer,integer), OPERATOR 5 >(integer,integer), FUNCTION 1 btint4cmp(integer,integer);

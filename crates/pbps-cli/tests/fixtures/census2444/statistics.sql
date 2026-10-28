-- Run in a fresh database on the task-owned instance.
CREATE SCHEMA app;
CREATE TABLE app.ordinary (id integer);
ALTER TABLE app.ordinary ADD COLUMN other integer; 
-- Candidate added after the ordinary-table baseline.
CREATE STATISTICS app.stat (dependencies,ndistinct,mcv) ON id,other FROM app.ordinary;

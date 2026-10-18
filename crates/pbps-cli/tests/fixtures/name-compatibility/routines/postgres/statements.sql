SET search_path = "app", "pg_temp";
CREATE VIEW "app"."viewname
line[31m\‮名前" AS
SELECT 1 AS id
;
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE FUNCTION "app"."routinename
line[31m\‮名前"
(v app."name
line[31m\‮名前") RETURNS integer LANGUAGE sql RETURN 1
;
SET search_path = '';
SET search_path = "app", "pg_temp";
REVOKE EXECUTE ON ROUTINE "app"."routinename
line[31m\‮名前"(app."name
line[31m\‮名前") FROM PUBLIC;
SET search_path = '';
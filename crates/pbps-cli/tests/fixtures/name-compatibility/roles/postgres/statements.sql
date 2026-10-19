SET search_path = "app", "pg_temp";
GRANT SELECT ON TABLE "app"."role
line[31m\‮名前" TO "role
line[31m\‮名前";
SET search_path = '';
SET search_path = "app", "pg_temp";
GRANT EXECUTE ON ROUTINE "app"."role
line[31m\‮名前"(integer) TO "role
line[31m\‮名前";
SET search_path = '';
SET search_path = "role
line[31m\‮名前", "pg_temp";
GRANT EXECUTE ON SCHEMA "role
line[31m\‮名前" TO "role
line[31m\‮名前";
SET search_path = '';
SET search_path = "app", "pg_temp";
REVOKE UPDATE ON TABLE "app"."role
line[31m\‮名前" FROM "role
line[31m\‮名前";
SET search_path = '';
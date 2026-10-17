SET search_path = "app", "pg_temp";
ALTER TABLE "app"."name
line[31m\‮名前" ADD COLUMN "name
line[31m\‮名前1" integer NULL;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."name
line[31m\‮名前" DROP COLUMN "name
line[31m\‮名前2";
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."name
line[31m\‮名前" ALTER COLUMN "name
line[31m\‮名前1" SET NOT NULL;
SET search_path = '';
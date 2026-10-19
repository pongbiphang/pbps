SET search_path = "app", "pg_temp";
CREATE TABLE "app"."child" (
    "col
line[31m\‮名前" integer NOT NULL,
    "col
line[31m\‮名前payload" integer NOT NULL DEFAULT 2
,
    CONSTRAINT "pk_child" PRIMARY KEY ("col
line[31m\‮名前")
) USING heap;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "uq_child" UNIQUE ("col
line[31m\‮名前payload");
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "fk_parent" FOREIGN KEY ("col
line[31m\‮名前payload") REFERENCES "app"."parent" ("col
line[31m\‮名前");
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE INDEX "ix_child" ON "app"."child" ("col
line[31m\‮名前" ASC) INCLUDE ("col
line[31m\‮名前payload");
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE TABLE "app"."parent" (
    "col
line[31m\‮名前" integer NOT NULL
) USING heap;
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE TABLE "app"."part" PARTITION OF "app"."partitioned" FOR VALUES FROM (E'1') TO (E'9') USING heap;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."part" ALTER COLUMN "col
line[31m\‮名前" SET DEFAULT 3
, ALTER COLUMN "col
line[31m\‮名前" SET NOT NULL;
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE TABLE "app"."partitioned" (
    "col
line[31m\‮名前" integer NOT NULL
) PARTITION BY RANGE ("col
line[31m\‮名前");
SET search_path = '';
SET search_path = "app", "pg_temp";
DO $pbps$
#variable_conflict use_column
<<pbps>>
DECLARE
    pbps_rows bigint;
BEGIN
UPDATE "app"."child" SET "col
line[31m\‮名前payload" = E'2'
 WHERE "col
line[31m\‮名前" OPERATOR(pg_catalog.=) E'1'
   AND (CAST("col
line[31m\‮名前payload" AS text)) COLLATE "C" = (CAST(E'1' AS text)) COLLATE "C";
GET DIAGNOSTICS pbps.pbps_rows = ROW_COUNT;
IF pbps.pbps_rows <> 1 THEN
    RAISE EXCEPTION USING MESSAGE = E'app.child row `1` is not as the plan recorded it: changed or deleted since the plan was made. Plan again.';
END IF;
IF NOT EXISTS (SELECT 1 FROM "app"."child" WHERE (CAST("col
line[31m\‮名前" AS text)) COLLATE "C" = (CAST(E'1' AS text)) COLLATE "C"
       AND (CAST("col
line[31m\‮名前payload" AS text)) COLLATE "C" = (CAST(E'2' AS text)) COLLATE "C") THEN
    RAISE EXCEPTION USING MESSAGE = E'app.child row `1` is not what this plan wrote once the statement had run -- a trigger on the table, another writer inside it, or a value the engine stores differently from the way it is declared. Nothing was applied; `pbps plan --db` says which.';
END IF;
END pbps
$pbps$;
SET search_path = '';
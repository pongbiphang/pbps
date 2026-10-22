SET search_path = "app", "pg_temp";
CREATE TABLE "app"."typed" (
    "id" integer NOT NULL,
    "amount" numeric(18, 4) NULL,
    "domain" "app"."type
line""'[31m\‮名前domain" NULL,
    "domains" "app"."type
line""'[31m\‮名前domain"[] NULL,
    "enumeration" "app"."type
line""'[31m\‮名前enum" NULL,
    "enumerations" "app"."type
line""'[31m\‮名前enum"[] NULL,
    "at" timestamp(3) with time zone NULL
) USING heap;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."typed" ADD COLUMN "added" "app"."type
line""'[31m\‮名前domain" NULL;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."typed" ALTER COLUMN "id" TYPE bigint;
SET search_path = '';
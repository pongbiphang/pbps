SET search_path = "app", "pg_temp";
CREATE TABLE "app"."child" (
    "id" integer NOT NULL,
    "code" integer NOT NULL,
    CONSTRAINT "local
line[31m\‮名前pk" PRIMARY KEY ("id")
) USING heap;
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前uq" UNIQUE ("code");
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前ck" CHECK (code > 0
);
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前fk" FOREIGN KEY ("code") REFERENCES "app"."parent" ("id");
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE INDEX "local
line[31m\‮名前expr" ON "app"."child" ((code + 1
) ASC) WHERE (code > 0
);
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE INDEX "local
line[31m\‮名前ix" ON "app"."child" ("code" ASC) INCLUDE ("id") WHERE (code > 0
);
SET search_path = '';
ALTER TABLE "app"."child" REPLICA IDENTITY USING INDEX "local
line[31m\‮名前uq";
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前pk" PRIMARY KEY ("id");
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前uq" UNIQUE ("code");
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" DROP CONSTRAINT "local
line[31m\‮名前uq";
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前fk" FOREIGN KEY ("code") REFERENCES "app"."parent" ("id");
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" DROP CONSTRAINT "local
line[31m\‮名前fk";
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" ADD CONSTRAINT "local
line[31m\‮名前ck" CHECK (code > 0
);
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER TABLE "app"."child" DROP CONSTRAINT "local
line[31m\‮名前ck";
SET search_path = '';
SET search_path = "app", "pg_temp";
CREATE INDEX "local
line[31m\‮名前ix" ON "app"."child" ("code" ASC) INCLUDE ("id") WHERE (code > 0
);
SET search_path = '';
SET search_path = "app", "pg_temp";
DROP INDEX "app"."local
line[31m\‮名前ix";
SET search_path = '';
DO $pbps$
DECLARE ix name := (SELECT i.relname FROM pg_catalog.pg_constraint k
                     JOIN pg_catalog.pg_class i ON i.oid = k.conindid
                     WHERE k.conrelid = '"app"."child"'::pg_catalog.regclass AND k.contype = 'p');
BEGIN
    EXECUTE pg_catalog.format('ALTER INDEX "app".%I SET (fillfactor = ''80'')', ix);
END
$pbps$;
SET search_path = "app", "pg_temp";
ALTER INDEX "app"."local
line[31m\‮名前uq" SET (fillfactor = '80');
SET search_path = '';
SET search_path = "app", "pg_temp";
ALTER INDEX "app"."local
line[31m\‮名前ix" SET (fillfactor = '80');
SET search_path = '';
ALTER TABLE "app"."child" REPLICA IDENTITY USING INDEX "local
line[31m\‮名前uq";
ALTER TABLE "app"."child" REPLICA IDENTITY USING INDEX "local
line[31m\‮名前ix";
DO $pbps$
DECLARE
    c name;
    t name;
    n integer;
    held text;
    tmp name[] := '{}';
    targets name[] := ARRAY['local
line[31m\‮名前child0', 'local
line[31m\‮名前child1', 'local
line[31m\‮名前child2', 'local
line[31m\‮名前child3', 'local
line[31m\‮名前child4']::name[];
    o pg_catalog.oid := '"app"."child"'::pg_catalog.regclass::pg_catalog.oid;
    ns pg_catalog.oid := '"app"'::pg_catalog.regnamespace::pg_catalog.oid;
BEGIN
    LOCK TABLE ONLY "app"."parent" IN ACCESS EXCLUSIVE MODE;
    c := (SELECT k.conname FROM pg_catalog.pg_constraint k WHERE k.conrelid = o AND k.contype = 'p');
    IF c IS NULL THEN
        RAISE EXCEPTION 'no clone of % on % to rename', 'local
line[31m\‮名前parent0', '"app"."child"';
    END IF;
    IF c <> 'local
line[31m\‮名前child0' THEN
    n := 0;
    LOOP
        t := pg_catalog.format('pbps_t%s_%s', 1, n);
        EXIT WHEN NOT (t = ANY (targets))
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_class
                            WHERE relnamespace = ns AND relname = t)
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint
                            WHERE conrelid = o AND conname = t);
        n := n + 1;
    END LOOP;
        tmp[1] := t;
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', c, t);
    END IF;
    c := (SELECT k.conname FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_constraint pk ON pk.oid = k.conparentid WHERE k.conrelid = o AND pk.conrelid = '"app"."parent"'::pg_catalog.regclass::pg_catalog.oid AND pk.conname = 'local
line[31m\‮名前parent1');
    IF c IS NULL THEN
        RAISE EXCEPTION 'no clone of % on % to rename', 'local
line[31m\‮名前parent1', '"app"."child"';
    END IF;
    IF c <> 'local
line[31m\‮名前child1' THEN
    n := 0;
    LOOP
        t := pg_catalog.format('pbps_t%s_%s', 2, n);
        EXIT WHEN NOT (t = ANY (targets))
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_class
                            WHERE relnamespace = ns AND relname = t)
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint
                            WHERE conrelid = o AND conname = t);
        n := n + 1;
    END LOOP;
        tmp[2] := t;
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', c, t);
    END IF;
    c := (SELECT k.conname FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_constraint pk ON pk.oid = k.conparentid WHERE k.conrelid = o AND pk.conrelid = '"app"."parent"'::pg_catalog.regclass::pg_catalog.oid AND pk.conname = 'local
line[31m\‮名前parent2');
    IF c IS NULL THEN
        RAISE EXCEPTION 'no clone of % on % to rename', 'local
line[31m\‮名前parent2', '"app"."child"';
    END IF;
    IF c <> 'local
line[31m\‮名前child2' THEN
    n := 0;
    LOOP
        t := pg_catalog.format('pbps_t%s_%s', 3, n);
        EXIT WHEN NOT (t = ANY (targets))
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_class
                            WHERE relnamespace = ns AND relname = t)
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint
                            WHERE conrelid = o AND conname = t);
        n := n + 1;
    END LOOP;
        tmp[3] := t;
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', c, t);
    END IF;
    c := (SELECT ci.relname FROM pg_catalog.pg_inherits h JOIN pg_catalog.pg_class ci ON ci.oid = h.inhrelid JOIN pg_catalog.pg_index i ON i.indexrelid = ci.oid JOIN pg_catalog.pg_class pi ON pi.oid = h.inhparent JOIN pg_catalog.pg_index x ON x.indexrelid = pi.oid WHERE i.indrelid = o AND x.indrelid = '"app"."parent"'::pg_catalog.regclass::pg_catalog.oid AND pi.relname = 'local
line[31m\‮名前parent4');
    IF c IS NULL THEN
        RAISE EXCEPTION 'no clone of % on % to rename', 'local
line[31m\‮名前parent4', '"app"."child"';
    END IF;
    IF c <> 'local
line[31m\‮名前child4' THEN
    n := 0;
    LOOP
        t := pg_catalog.format('pbps_t%s_%s', 5, n);
        EXIT WHEN NOT (t = ANY (targets))
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_class
                            WHERE relnamespace = ns AND relname = t)
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint
                            WHERE conrelid = o AND conname = t);
        n := n + 1;
    END LOOP;
        tmp[5] := t;
        EXECUTE pg_catalog.format('ALTER INDEX "app".%I RENAME TO %I', c, t);
    END IF;
    ALTER TABLE "app"."parent" DETACH PARTITION "app"."child";
    n := 0;
    LOOP
        t := pg_catalog.format('pbps_t%s_%s', 4, n);
        EXIT WHEN NOT (t = ANY (targets))
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_class
                            WHERE relnamespace = ns AND relname = t)
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint
                            WHERE conrelid = o AND conname = t);
        n := n + 1;
    END LOOP;
    tmp[4] := t;
    EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', 'local
line[31m\‮名前parent3', t);
    IF tmp[1] IS NOT NULL THEN
        held := COALESCE((SELECT CASE WHEN tb.relname IS NULL THEN pg_catalog.format('relation %I.%I', 'app', cl.relname) ELSE pg_catalog.format('index %I.%I on %I.%I', 'app', cl.relname, 'app', tb.relname) END FROM pg_catalog.pg_class cl LEFT JOIN pg_catalog.pg_index ix ON ix.indexrelid = cl.oid LEFT JOIN pg_catalog.pg_class tb ON tb.oid = ix.indrelid WHERE cl.relnamespace = ns AND cl.relname = 'local
line[31m\‮名前child0'), (SELECT pg_catalog.format('constraint %I on %s', k.conname, '"app"."child"') FROM pg_catalog.pg_constraint k WHERE k.conrelid = o AND k.conname = 'local
line[31m\‮名前child0'));
        IF held IS NOT NULL THEN
            RAISE EXCEPTION USING
                MESSAGE = pg_catalog.format('cannot give %s the name %I: %s already holds it', '"app"."child"', 'local
line[31m\‮名前child0', held),
                HINT = 'Free that name in a plan of its own first (another partition''s clone: detach that partition first), or declare another name (DEC-1565.1).';
        END IF;
    END IF;
    IF tmp[2] IS NOT NULL THEN
        held := COALESCE((SELECT CASE WHEN tb.relname IS NULL THEN pg_catalog.format('relation %I.%I', 'app', cl.relname) ELSE pg_catalog.format('index %I.%I on %I.%I', 'app', cl.relname, 'app', tb.relname) END FROM pg_catalog.pg_class cl LEFT JOIN pg_catalog.pg_index ix ON ix.indexrelid = cl.oid LEFT JOIN pg_catalog.pg_class tb ON tb.oid = ix.indrelid WHERE cl.relnamespace = ns AND cl.relname = 'local
line[31m\‮名前child1'), (SELECT pg_catalog.format('constraint %I on %s', k.conname, '"app"."child"') FROM pg_catalog.pg_constraint k WHERE k.conrelid = o AND k.conname = 'local
line[31m\‮名前child1'));
        IF held IS NOT NULL THEN
            RAISE EXCEPTION USING
                MESSAGE = pg_catalog.format('cannot give %s the name %I: %s already holds it', '"app"."child"', 'local
line[31m\‮名前child1', held),
                HINT = 'Free that name in a plan of its own first (another partition''s clone: detach that partition first), or declare another name (DEC-1565.1).';
        END IF;
    END IF;
    IF tmp[3] IS NOT NULL THEN
        held := (SELECT pg_catalog.format('constraint %I on %s', k.conname, '"app"."child"') FROM pg_catalog.pg_constraint k WHERE k.conrelid = o AND k.conname = 'local
line[31m\‮名前child2');
        IF held IS NOT NULL THEN
            RAISE EXCEPTION USING
                MESSAGE = pg_catalog.format('cannot give %s the name %I: %s already holds it', '"app"."child"', 'local
line[31m\‮名前child2', held),
                HINT = 'Free that name in a plan of its own first (another partition''s clone: detach that partition first), or declare another name (DEC-1565.1).';
        END IF;
    END IF;
    IF tmp[4] IS NOT NULL THEN
        held := (SELECT pg_catalog.format('constraint %I on %s', k.conname, '"app"."child"') FROM pg_catalog.pg_constraint k WHERE k.conrelid = o AND k.conname = 'local
line[31m\‮名前child3');
        IF held IS NOT NULL THEN
            RAISE EXCEPTION USING
                MESSAGE = pg_catalog.format('cannot give %s the name %I: %s already holds it', '"app"."child"', 'local
line[31m\‮名前child3', held),
                HINT = 'Free that name in a plan of its own first (another partition''s clone: detach that partition first), or declare another name (DEC-1565.1).';
        END IF;
    END IF;
    IF tmp[5] IS NOT NULL THEN
        held := (SELECT CASE WHEN tb.relname IS NULL THEN pg_catalog.format('relation %I.%I', 'app', cl.relname) ELSE pg_catalog.format('index %I.%I on %I.%I', 'app', cl.relname, 'app', tb.relname) END FROM pg_catalog.pg_class cl LEFT JOIN pg_catalog.pg_index ix ON ix.indexrelid = cl.oid LEFT JOIN pg_catalog.pg_class tb ON tb.oid = ix.indrelid WHERE cl.relnamespace = ns AND cl.relname = 'local
line[31m\‮名前child4');
        IF held IS NOT NULL THEN
            RAISE EXCEPTION USING
                MESSAGE = pg_catalog.format('cannot give %s the name %I: %s already holds it', '"app"."child"', 'local
line[31m\‮名前child4', held),
                HINT = 'Free that name in a plan of its own first (another partition''s clone: detach that partition first), or declare another name (DEC-1565.1).';
        END IF;
    END IF;
    IF tmp[1] IS NOT NULL THEN
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', tmp[1], 'local
line[31m\‮名前child0');
    END IF;
    IF tmp[2] IS NOT NULL THEN
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', tmp[2], 'local
line[31m\‮名前child1');
    END IF;
    IF tmp[3] IS NOT NULL THEN
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', tmp[3], 'local
line[31m\‮名前child2');
    END IF;
    IF tmp[4] IS NOT NULL THEN
        EXECUTE pg_catalog.format('ALTER TABLE "app"."child" RENAME CONSTRAINT %I TO %I', tmp[4], 'local
line[31m\‮名前child3');
    END IF;
    IF tmp[5] IS NOT NULL THEN
        EXECUTE pg_catalog.format('ALTER INDEX "app".%I RENAME TO %I', tmp[5], 'local
line[31m\‮名前child4');
    END IF;
END
$pbps$;
DO $pbps$
BEGIN
IF NOT EXISTS (SELECT FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid WHERE n.nspname='app' AND t.typname='type
line"''[31m\‮名前domain' AND t.typisdefined AND (t.typtype IN ('e','d') OR (t.typtype='c' AND c.relkind='c')) AND pg_catalog.has_schema_privilege(n.oid,'USAGE') AND pg_catalog.has_type_privilege(t.oid,'USAGE')) THEN RAISE EXCEPTION '%', 'required type `app."type
line""''[31m\‮名前domain"` is missing, unusable or not an enum, domain or standalone composite'; END IF;
IF NOT EXISTS (SELECT FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid WHERE n.nspname='app' AND t.typname='type
line"''[31m\‮名前enum' AND t.typisdefined AND (t.typtype IN ('e','d') OR (t.typtype='c' AND c.relkind='c')) AND pg_catalog.has_schema_privilege(n.oid,'USAGE') AND pg_catalog.has_type_privilege(t.oid,'USAGE')) THEN RAISE EXCEPTION '%', 'required type `app."type
line""''[31m\‮名前enum"` is missing, unusable or not an enum, domain or standalone composite'; END IF;
END
$pbps$;
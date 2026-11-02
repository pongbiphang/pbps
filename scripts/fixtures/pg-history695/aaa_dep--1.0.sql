CREATE FUNCTION aaa_dep_two() RETURNS integer LANGUAGE sql IMMUTABLE RETURN zzz_base_one() + 1;

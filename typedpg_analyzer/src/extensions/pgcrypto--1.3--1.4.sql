/* contrib/pgcrypto/pgcrypto--1.3--1.4.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

CREATE FUNCTION fips_mode()
RETURNS bool
AS 'MODULE_PATHNAME', 'pg_check_fipsmode'
LANGUAGE C VOLATILE STRICT PARALLEL SAFE;

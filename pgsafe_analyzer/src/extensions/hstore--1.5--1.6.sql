/* contrib/hstore/hstore--1.5--1.6.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

CREATE FUNCTION hstore_hash_extended(hstore, int8)
RETURNS int8
AS 'MODULE_PATHNAME','hstore_hash_extended'
LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

ALTER OPERATOR FAMILY hash_hstore_ops USING hash ADD
    FUNCTION    2   hstore_hash_extended(hstore, int8);

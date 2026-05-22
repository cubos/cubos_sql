/* contrib/pg_prewarm/pg_prewarm--1.1--1.2.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

CREATE FUNCTION autoprewarm_start_worker()
RETURNS VOID STRICT
AS 'MODULE_PATHNAME', 'autoprewarm_start_worker'
LANGUAGE C;

CREATE FUNCTION autoprewarm_dump_now()
RETURNS pg_catalog.int8 STRICT
AS 'MODULE_PATHNAME', 'autoprewarm_dump_now'
LANGUAGE C;

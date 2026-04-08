/* contrib/pg_buffercache/pg_buffercache--1.2--1.3.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

GRANT EXECUTE ON FUNCTION pg_buffercache_pages() TO pg_monitor;
GRANT SELECT ON pg_buffercache TO pg_monitor;

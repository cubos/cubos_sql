/* contrib/tcn/tcn--1.0.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION

CREATE FUNCTION triggered_change_notification()
RETURNS pg_catalog.trigger
AS 'MODULE_PATHNAME'
LANGUAGE C;

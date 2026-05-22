/* contrib/spi/autoinc--1.0.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION

CREATE FUNCTION autoinc()
RETURNS trigger
AS 'MODULE_PATHNAME'
LANGUAGE C;

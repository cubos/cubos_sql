/* contrib/spi/moddatetime--1.0.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION

CREATE FUNCTION moddatetime()
RETURNS trigger
AS 'MODULE_PATHNAME'
LANGUAGE C;

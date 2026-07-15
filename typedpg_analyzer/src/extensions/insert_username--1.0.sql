/* contrib/spi/insert_username--1.0.sql */

-- complain if script is sourced in psql, rather than via CREATE EXTENSION

CREATE FUNCTION insert_username()
RETURNS trigger
AS 'MODULE_PATHNAME'
LANGUAGE C;

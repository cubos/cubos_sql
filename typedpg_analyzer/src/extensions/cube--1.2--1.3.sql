/* contrib/cube/cube--1.2--1.3.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

ALTER OPERATOR <= (cube, cube) SET (
	RESTRICT = scalarlesel, JOIN = scalarlejoinsel
);

ALTER OPERATOR >= (cube, cube) SET (
	RESTRICT = scalargesel, JOIN = scalargejoinsel
);

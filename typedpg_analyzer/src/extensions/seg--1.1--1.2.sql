/* contrib/seg/seg--1.1--1.2.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

ALTER OPERATOR <= (seg, seg) SET (
	RESTRICT = scalarlesel,
	JOIN = scalarlejoinsel
);

ALTER OPERATOR >= (seg, seg) SET (
	RESTRICT = scalargesel,
	JOIN = scalargejoinsel
);

/* contrib/hstore/hstore--1.4--1.5.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

ALTER OPERATOR #<=# (hstore, hstore) SET (
       RESTRICT = scalarlesel,
       JOIN = scalarlejoinsel
);

ALTER OPERATOR #>=# (hstore, hstore) SET (
       RESTRICT = scalargesel,
       JOIN = scalargejoinsel
);

/* contrib/seg/seg--1.3--1.4.sql */

-- complain if script is sourced in psql, rather than via ALTER EXTENSION

-- Remove @ and ~
DROP OPERATOR @ (seg, seg);
DROP OPERATOR ~ (seg, seg);

//! Literal-content battery for the input-function validators in
//! `literal_input.rs` — bit/varbit, money, inet/cidr, macaddr, the
//! geometric family, tid/pg_lsn/xid, and the datetime tokenizer-alphabet
//! rule. Valid and invalid contents side by side, mirrored against the
//! live PG (outcome + error wording + SQLSTATE) by `analyze_checked`.
#![cfg(feature = "pg_sanity")]

#[path = "common/parity.rs"]
mod parity;

const SETUP: &str = "CREATE TABLE t (id INT PRIMARY KEY);";

const BATTERY: &[&str] = &[
    // ── bit / varbit ────────────────────────────────────────────────────
    "SELECT '101'::varbit",
    "SELECT 'x1F'::varbit",
    "SELECT 'b101'::varbit",
    "SELECT ''::varbit",
    "SELECT 'NaN'::varbit",
    "SELECT ' 42 '::varbit",
    "SELECT '0b101'::varbit",
    "SELECT '102'::varbit",
    "SELECT 'xFG'::varbit",
    "SELECT '101'::bit(3)",
    "SELECT '2'::bit(1)",
    // ── money ───────────────────────────────────────────────────────────
    "SELECT '123'::money",
    "SELECT '$123.45'::money",
    "SELECT '-$1,000.00'::money",
    "SELECT '($123)'::money",
    "SELECT '$-123'::money",
    "SELECT '  12  '::money",
    "SELECT 'hello'::money",
    "SELECT ''::money",
    "SELECT '(1,2]'::money",
    "SELECT '1.2.3'::money",
    "SELECT '9999999999999999999999'::money",
    // ── inet / cidr ─────────────────────────────────────────────────────
    "SELECT '192.168.0.1'::inet",
    "SELECT '192.168.0.1/24'::inet",
    "SELECT '::1'::inet",
    "SELECT 'fe80::1/64'::inet",
    "SELECT '::ffff:192.168.0.1'::inet",
    "SELECT '42'::inet",
    "SELECT '192.168'::inet",
    "SELECT '256.1.1.1'::inet",
    "SELECT '192.168.0.1/33'::inet",
    "SELECT 'hello'::inet",
    "SELECT '1:2:3:4:5:6:7:8'::inet",
    "SELECT '1:2:3:4:5:6:7:8:9'::inet",
    "SELECT '1::2::3'::inet",
    "SELECT '10/8'::cidr",
    "SELECT '10.1/16'::cidr",
    "SELECT '192.168.0.0/24'::cidr",
    "SELECT 'x/8'::cidr",
    // ── macaddr / macaddr8 ──────────────────────────────────────────────
    "SELECT 'aa:bb:cc:dd:ee:ff'::macaddr",
    "SELECT 'aa-bb-cc-dd-ee-ff'::macaddr",
    "SELECT 'aabb.ccdd.eeff'::macaddr",
    "SELECT 'aabbccddeeff'::macaddr",
    "SELECT 'aa:bb:cc:dd:ee'::macaddr",
    "SELECT ' 42 '::macaddr",
    "SELECT 'hello'::macaddr",
    "SELECT 'aa:bb:cc:dd:ee:ff:00:11'::macaddr8",
    "SELECT 'aa:bb:cc:dd:ee:ff'::macaddr8",
    "SELECT 'zz:bb:cc:dd:ee:ff'::macaddr8",
    // ── geometric ───────────────────────────────────────────────────────
    "SELECT '(1,2)'::point",
    "SELECT '1,2'::point",
    "SELECT '(NaN,NaN)'::point",
    "SELECT '(1.5e3,-2)'::point",
    "SELECT '3.14'::point",
    "SELECT 'hello'::point",
    "SELECT '((0,0),(1,1))'::box",
    "SELECT ' 42 '::box",
    "SELECT '[(0,0),(1,1)]'::lseg",
    "SELECT '(1,2)'::lseg",
    "SELECT '<(0,0),5>'::circle",
    "SELECT '(1,2)'::circle",
    "SELECT '{1,2,3}'::line",
    "SELECT '((0,0),(1,1))'::line",
    "SELECT '{1,2}'::line",
    "SELECT '((0,0),(1,1),(2,0))'::polygon",
    "SELECT '(1,2)'::path",
    "SELECT '(1,2,3)'::path",
    // ── tid / pg_lsn / xid ──────────────────────────────────────────────
    "SELECT '(0,1)'::tid",
    "SELECT '(0)'::tid",
    "SELECT '42'::tid",
    "SELECT '0/0'::pg_lsn",
    "SELECT 'AB/CDEF1234'::pg_lsn",
    "SELECT '0'::pg_lsn",
    "SELECT 'X/Y'::pg_lsn",
    "SELECT '42'::xid",
    "SELECT 'ff'::xid",
    "SELECT '42'::xid8",
    "SELECT '0x10'::xid8",
    "SELECT '0x10'::cid",
    "SELECT '0x10'::xid",
    "SELECT ''::money",
    // ── datetime tokenizer alphabet ─────────────────────────────────────
    "SELECT '2024-01-01'::date",
    "SELECT '(1,2]'::date",
    "SELECT '{1,2}'::timestamp",
    "SELECT '[1,]'::timestamptz",
    "SELECT '12:30:00'::time",
    "SELECT '{}'::timetz",
    "SELECT '1 day'::interval",
    "SELECT '[1,2)'::interval",
    "SELECT '2024-01-01 12:00 America/New_York'::timestamptz",
    "SELECT 'now()'::timestamptz",
];

#[test]
fn literal_battery() {
    parity::assert_battery_parity(SETUP, BATTERY, "literal shapes");
}

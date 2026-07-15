//! Battery of bare-`$param` type-inference shapes, each checked against the
//! live PG mirror (output columns, param types, and error wording all
//! compared by `analyze_checked`). The contexts here are exactly the ones
//! the fuzzer historically under-covered: params as function arguments,
//! arithmetic operands, CASE/COALESCE branches, BETWEEN/IN/ANY positions,
//! set-operation branches, subscripts, JSON operands, and window arguments.
//! Collects every divergence instead of stopping at the first, then asserts
//! none were found.
#![cfg(feature = "pg_sanity")]

#[path = "common/parity.rs"]
mod parity;

const SETUP: &str = "
CREATE TYPE status AS ENUM ('draft', 'published');
CREATE DOMAIN email AS TEXT;
CREATE TABLE users (
    id BIGINT PRIMARY KEY, name TEXT NOT NULL, addr email, age INT,
    score NUMERIC(10,2), active BOOL NOT NULL, ratio FLOAT8,
    tags TEXT[], nums INT[], st status NOT NULL,
    prefs JSONB, created_at TIMESTAMPTZ NOT NULL, bday DATE
);
";

const BATTERY: &[&str] = &[
    // arithmetic via operator resolution
    "SELECT age + $p1 FROM users",
    "SELECT $p1 + age FROM users",
    "SELECT id * $p1 FROM users",
    "SELECT score - $p1 FROM users",
    "SELECT ratio / $p1 FROM users",
    "SELECT created_at - $p1 FROM users",
    "SELECT bday + $p1 FROM users",
    // string / pattern operators
    "SELECT name || $p1 FROM users",
    "SELECT $p1 || name FROM users",
    "SELECT id FROM users WHERE name LIKE $p1",
    "SELECT id FROM users WHERE name ~ $p1",
    // function arguments
    "SELECT length($p1)",
    "SELECT lower($p1)",
    "SELECT round($p1, 2)",
    "SELECT substr(name, $p1) FROM users",
    "SELECT lpad(name, $p1) FROM users",
    "SELECT date_trunc($p1, created_at) FROM users",
    "SELECT split_part(name, $p1, $p2) FROM users",
    "SELECT power($p1, 2)",
    // conditional constructs
    "SELECT coalesce($p1, age) FROM users",
    "SELECT coalesce(age, $p1) FROM users",
    "SELECT CASE WHEN active THEN $p1 ELSE age END FROM users",
    "SELECT CASE WHEN active THEN age ELSE $p1 END FROM users",
    "SELECT GREATEST(age, $p1) FROM users",
    "SELECT NULLIF($p1, age) FROM users",
    // special comparison forms
    "SELECT id FROM users WHERE age BETWEEN $p1 AND $p2",
    "SELECT id FROM users WHERE st IN ($p1, 'draft')",
    "SELECT id FROM users WHERE id = ANY($p1)",
    "SELECT id FROM users WHERE $p1 = ANY(tags)",
    "SELECT id FROM users WHERE addr = ANY($p1)",
    "SELECT id FROM users WHERE addr IS DISTINCT FROM $p1",
    // bare-context defaults
    "SELECT $p1 IS NULL",
    "SELECT $p1 IS TRUE",
    "SELECT active IS NOT TRUE FROM users",
    "SELECT NOT $p1",
    "SELECT id FROM users WHERE $p1",
    "SELECT tags || $p1 FROM users",
    "SELECT $p1 || nums FROM users",
    "SELECT DISTINCT ON (id = $p1) name FROM users",
    // set-op reconciliation
    "SELECT $p1 UNION ALL SELECT age FROM users",
    "SELECT age FROM users UNION ALL SELECT $p1",
    // arrays / subscripts / json
    "SELECT nums[$p1] FROM users",
    "SELECT tags[$p1:$p2] FROM users",
    "SELECT ARRAY[age, $p1] FROM users",
    "SELECT prefs -> $p1 FROM users",
    "SELECT prefs #> $p1 FROM users",
    "SELECT prefs @> $p1 FROM users",
    // window / aggregate arguments
    "SELECT lag(name, $p1) OVER (ORDER BY id) FROM users",
    "SELECT sum(age) OVER (ORDER BY id ROWS BETWEEN $p1 PRECEDING AND CURRENT ROW) FROM users",
    "SELECT ntile($p1) OVER (ORDER BY id) FROM users",
    "SELECT string_agg(name, $p1) FROM users",
    "SELECT count(*) FILTER (WHERE $p1) FROM users",
    // mixed/nested
    "SELECT age + $p1 * 2 FROM users",
    "SELECT ($p1 || name) || $p2 FROM users",
    "SELECT coalesce(upper($p1), name) FROM users",
    "SELECT id FROM users WHERE age = $p1 AND name = $p2 OR active = $p3",
    "SELECT abs($p1)",
    "SELECT id FROM users GROUP BY st HAVING count(*) > $p1",
    "SELECT id FROM users ORDER BY $p1",
];

#[test]
fn param_inference_battery() {
    parity::assert_battery_parity(SETUP, BATTERY, "param-inference shapes");
}

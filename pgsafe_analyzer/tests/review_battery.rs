//! Code-review battery: hand-written queries each aimed at a spot the
//! analyzer's source suggests could diverge from PG — JOIN USING merging,
//! ordinals, whole-row references, alias column lists, duplicate aliases,
//! inconsistent param deduction, E-strings, named windows, VALUES arity,
//! SELECT DISTINCT + ORDER BY, and friends. Every entry is mirrored against
//! the live PG (outcome, columns, params, error wording) by
//! `analyze_checked`; divergences are collected and the test fails listing
//! them all.
#![cfg(feature = "pg_sanity")]

use pgsafe_analyzer::PgCatalog;

const SETUP: &str = "
CREATE TABLE a (id BIGINT PRIMARY KEY, x INT NOT NULL, label TEXT);
CREATE TABLE b (id BIGINT PRIMARY KEY, y INT NOT NULL, label TEXT);
CREATE TABLE users (
    id BIGINT PRIMARY KEY, name TEXT NOT NULL, age INT
);
";

const BATTERY: &[&str] = &[
    // ── JOIN USING / NATURAL merging ────────────────────────────────────
    "SELECT * FROM a JOIN b USING (id)",
    "SELECT id FROM a JOIN b USING (id)",
    "SELECT label FROM a JOIN b USING (id)",
    "SELECT * FROM a NATURAL JOIN b",
    "SELECT id FROM a FULL JOIN b USING (id)",
    // ── ordinals ────────────────────────────────────────────────────────
    "SELECT name FROM users GROUP BY 1",
    "SELECT name FROM users GROUP BY 2",
    "SELECT name FROM users ORDER BY 1",
    "SELECT name FROM users ORDER BY 5",
    // ── whole-row references ────────────────────────────────────────────
    "SELECT u FROM users u",
    "SELECT row_to_json(u) FROM users u",
    "SELECT row_to_json(u.*) FROM users u",
    // ── alias column lists ──────────────────────────────────────────────
    "SELECT p FROM users AS t(p, q, r)",
    "SELECT p FROM users AS t(p, q, r, s)",
    "SELECT v FROM (SELECT 1, 'x') AS t(v, w)",
    // ── FROM-clause shape rules ─────────────────────────────────────────
    "SELECT 1 FROM users u, a u",
    "SELECT * FROM (SELECT 1)",
    // ── params: deduction conflicts and E-strings ───────────────────────
    "SELECT $p1::int4, $p1::text",
    "SELECT E'it\\'s' || $p1",
    "SELECT 'it''s' || $p1",
    // ── named windows ───────────────────────────────────────────────────
    "SELECT sum(age) OVER w FROM users WINDOW w AS (ORDER BY id)",
    "SELECT sum(age) OVER w FROM users",
    // ── VALUES arity ────────────────────────────────────────────────────
    "SELECT * FROM (VALUES (1, 2), (3)) AS v(c1, c2)",
    // ── SELECT DISTINCT + ORDER BY rule ─────────────────────────────────
    "SELECT DISTINCT name FROM users ORDER BY age",
    "SELECT DISTINCT name FROM users ORDER BY name",
    // ── HAVING without GROUP BY / grouped HAVING ────────────────────────
    "SELECT count(*) FROM users HAVING name = 'x'",
    "SELECT count(*) FROM users HAVING count(*) > 0",
    // ── WITH ORDINALITY ─────────────────────────────────────────────────
    "SELECT * FROM unnest(ARRAY[10, 20]) WITH ORDINALITY",
    "SELECT * FROM unnest(ARRAY[10, 20]) WITH ORDINALITY AS t(v, n)",
    // ── misc scope corners ──────────────────────────────────────────────
    "SELECT users.id FROM users u",
    "SELECT t.* FROM users AS t(p, q, r)",
];

#[test]
fn review_battery() {
    let mut db = PgCatalog::new().expect("mirror");
    db.apply_sql(SETUP).expect("setup");
    let mut divergences = 0;
    for sql in BATTERY {
        let (_res, div) = db.analyze_checked(sql);
        if let Some(d) = div {
            divergences += 1;
            eprintln!(
                "\n[{:?}] {sql}\n  {}",
                d.kind,
                d.message
                    .lines()
                    .filter(|l| l.contains("analyzer") || l.contains("PG"))
                    .take(4)
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }
    }
    assert_eq!(
        divergences,
        0,
        "{divergences}/{} review shapes diverged from PG",
        BATTERY.len()
    );
}

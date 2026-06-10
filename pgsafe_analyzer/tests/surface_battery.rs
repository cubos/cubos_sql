//! Surface-coverage battery: SQL features the fuzzer's generators and the
//! older batteries don't exercise — ON CONFLICT in its arbiter/excluded
//! variants, LATERAL (explicit and implicit on SRFs), multirange operators,
//! `FETCH FIRST … WITH TIES`, `UPDATE … FROM` / `DELETE … USING`, row
//! comparisons, and OVERRIDING. Every entry is mirrored against the live PG
//! (outcome, columns, params, error wording + SQLSTATE) by
//! `analyze_checked`; divergences are collected and the test fails listing
//! them all.
#![cfg(feature = "pg_sanity")]

#[path = "common/parity.rs"]
mod parity;

const SETUP: &str = "
CREATE TABLE users (
    id BIGINT PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    age INT,
    tags TEXT[] NOT NULL DEFAULT '{}',
    visits INT NOT NULL DEFAULT 0
);
CREATE TABLE events (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    span TSTZRANGE,
    spans TSTZMULTIRANGE,
    kind TEXT NOT NULL,
    UNIQUE (user_id, kind)
);
CREATE TABLE counters (
    key TEXT PRIMARY KEY,
    n INT NOT NULL
);
";

const BATTERY: &[&str] = &[
    // ── ON CONFLICT: arbiter shapes ─────────────────────────────────────
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT DO NOTHING",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO NOTHING",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO UPDATE SET n = counters.n + 1",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO UPDATE SET n = excluded.n",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO UPDATE SET n = excluded.n + counters.n WHERE counters.n < 10",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO UPDATE SET n = excluded.n RETURNING key, n",
    "INSERT INTO events (user_id, at, kind) VALUES ($u, now(), $kind) ON CONFLICT (user_id, kind) DO UPDATE SET at = excluded.at",
    // excluded misuse / unknown columns
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (key) DO UPDATE SET n = excluded.ghost",
    "INSERT INTO counters (key, n) VALUES ($k, $n) ON CONFLICT (ghost) DO NOTHING",
    "SELECT excluded.n FROM counters",
    // ── LATERAL ─────────────────────────────────────────────────────────
    "SELECT u.id, t.tag FROM users u, LATERAL unnest(u.tags) AS t(tag)",
    "SELECT u.id, t.tag FROM users u CROSS JOIN LATERAL unnest(u.tags) AS t(tag)",
    // implicit LATERAL: SRF args can see earlier FROM items without the keyword
    "SELECT u.id, t.tag FROM users u, unnest(u.tags) AS t(tag)",
    "SELECT u.id, s.total FROM users u, LATERAL (SELECT count(*) AS total FROM events e WHERE e.user_id = u.id) s",
    "SELECT u.id FROM users u JOIN LATERAL (SELECT e.at FROM events e WHERE e.user_id = u.id ORDER BY e.at DESC LIMIT 1) last ON true",
    // LATERAL cannot see *later* FROM items
    "SELECT 1 FROM LATERAL (SELECT u.id) s, users u",
    // ── ranges and multiranges ──────────────────────────────────────────
    "SELECT tstzrange(now(), now() + interval '1 hour') @> now()",
    "SELECT span @> at FROM events",
    "SELECT spans @> at FROM events",
    "SELECT span && tstzrange(now(), NULL) FROM events",
    "SELECT spans + tstzmultirange(tstzrange(now(), NULL)) FROM events",
    "SELECT spans * tstzmultirange(tstzrange(now(), NULL)) FROM events",
    "SELECT range_agg(span) FROM events",
    "SELECT unnest(spans) FROM events",
    "SELECT lower(span), upper(span), isempty(span) FROM events",
    "SELECT spans - span FROM events",
    "SELECT '{[2024-01-01,2024-02-01)}'::tstzmultirange",
    "SELECT span = $p1 FROM events",
    // ── FETCH FIRST / WITH TIES ─────────────────────────────────────────
    "SELECT id FROM users ORDER BY age FETCH FIRST 3 ROWS ONLY",
    "SELECT id FROM users ORDER BY age FETCH FIRST 3 ROWS WITH TIES",
    "SELECT id FROM users ORDER BY age OFFSET 2 ROWS FETCH NEXT 3 ROWS ONLY",
    "SELECT id FROM users ORDER BY age FETCH FIRST $p1 ROWS ONLY",
    "SELECT id FROM users FETCH FIRST 'x' ROWS ONLY",
    // ── UPDATE … FROM / DELETE … USING ──────────────────────────────────
    "UPDATE counters SET n = counters.n + e.user_id::int FROM events e WHERE e.kind = counters.key",
    "UPDATE counters c SET n = 0 FROM users u WHERE u.email = c.key RETURNING c.key, u.id",
    "DELETE FROM counters USING users u WHERE u.email = counters.key",
    "DELETE FROM counters USING users u WHERE u.email = counters.key RETURNING counters.key, u.name",
    // the target table's own alias hides the bare name
    "UPDATE counters c SET n = counters.n + 1",
    // ── row comparisons / composite shapes ──────────────────────────────
    "SELECT (id, name) = (id, name) FROM users",
    "SELECT ROW(id, name) < ROW(id, 'zzz') FROM users",
    "SELECT (id, name) IS NOT DISTINCT FROM (id, name) FROM users",
    "SELECT ROW(1, 'a') = ROW(1, 'a', true)",
    // ── OVERRIDING / DEFAULT VALUES / identity ──────────────────────────
    "INSERT INTO counters DEFAULT VALUES",
    "INSERT INTO events (id, user_id, at, kind) OVERRIDING SYSTEM VALUE VALUES (1, 1, now(), 'x')",
    "INSERT INTO events (id, user_id, at, kind) VALUES (1, 1, now(), 'x')",
    // ── misc uncovered corners ──────────────────────────────────────────
    "SELECT DISTINCT ON (kind, user_id) id FROM events ORDER BY kind, user_id, at DESC",
    "SELECT count(*) FILTER (WHERE age > 18) - count(*) FILTER (WHERE age <= 18) FROM users",
    "SELECT id FROM users WHERE EXISTS (SELECT FROM events e WHERE e.user_id = users.id)",
    "SELECT array_agg(name ORDER BY age DESC NULLS LAST) FROM users",
    "SELECT id FROM users u FOR UPDATE OF u SKIP LOCKED",
    "SELECT id FROM users FOR UPDATE OF ghost",
];

#[test]
fn surface_battery() {
    parity::assert_battery_parity(SETUP, BATTERY, "surface shapes");
}

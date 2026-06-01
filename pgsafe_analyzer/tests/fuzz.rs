//! Differential fuzzer for the static analyzer.
//!
//! The oracle is [`PgCatalog::analyze_checked`]: it analyzes a query *and*
//! cross-checks the result against a real PostgreSQL server (the same
//! `pg_sanity` mirror the assertion-based tests use), returning a
//! [`Divergence`] instead of panicking. So the fuzzer's only job is to *feed
//! it interesting queries* — any query that makes the analyzer and PG
//! disagree is, by construction, a confirmed bug. Two families of bugs fall
//! out automatically:
//!
//!   * **valid queries with a divergent type** — `Ok`/`Ok` where a column or
//!     parameter type (or count, or name) differs, and
//!   * **invalid queries with a divergent error** — `Err`/`Err` where our
//!     message doesn't start with PG's, plus the `Ok`/`Err` and `Err`/`Ok`
//!     asymmetries.
//!
//! Query generation uses four strategies:
//!   1. a schema-driven **template generator** (`gen_statement`) that emits
//!      SELECTs (with joins, subquery predicates, scalar subqueries, GROUP BY,
//!      DISTINCT [ON], …), set operations, CTEs, and DML
//!      (INSERT/UPDATE/DELETE with RETURNING) — mistypes deliberately allowed,
//!      and `$pN` parameters threaded through typed contexts;
//!   2. an **AST mutator** that parses a seed with `pg_query`, tweaks leaf
//!      nodes in the protobuf tree (constants, column refs, operators,
//!      function names, cast targets), and deparses back to SQL — which keeps
//!      the output syntactically plausible while perturbing types; and
//!   3. a **catalog-mined, type-directed generator** (`gen_typed_select`) that
//!      indexes every real function/operator by result type and builds
//!      expressions of a requested type bottom-up, so the query is *valid by
//!      construction* — this is what lets the oracle compare column/param
//!      *types* across the whole builtin surface (see `build_typed_cat`); and
//!   4. a **metamorphic** check (`metamorphic_check`) that wraps a valid query
//!      in a pass-through subquery/CTE and asserts the analyzer reports the same
//!      column type/nullability shape — the only way to test nullability
//!      propagation, since PG's wire protocol doesn't expose it.
//!
//! It also fuzzes the **schema**: each run extends the fixed base with a seeded
//! random schema (`gen_random_schema`) — domains, composites, multi-dim arrays,
//! typmod'd / generated columns, a view, a second schema — applied via the
//! non-panicking `apply_sql_checked`, so a DDL disagreement is itself a finding.
//!
//! Findings are deduplicated by a signature (kind + the message with the SQL
//! body stripped), minimized via structural reductions, written to
//! `$FUZZ_OUT` (default `target/fuzz-findings`), and summarized at the end.
//!
//! Only compiled under the `pg_sanity` feature (it needs the live mirror).
//! Run it via:
//!
//! ```bash
//! scripts/run-pg-sanity.sh -p pgsafe_analyzer --run-ignored all fuzz
//! ```
//!
//! Knobs (env vars): `FUZZ_ITERS` (default 2000), `FUZZ_SEED` (default
//! 0xC0FFEE), `FUZZ_OUT` (output dir), `FUZZ_STRICT` (panic at the end if any
//! finding was discovered), `FUZZ_DUMP=N` (print N generated statements and a
//! shape histogram, then exit — no DB needed; for inspecting what the template
//! generator produces) and `FUZZ_TYPED_DUMP=N` (print N type-directed samples
//! and the valid-by-construction rate, then exit — needs the DB).
#![cfg(feature = "pg_sanity")]

use std::collections::BTreeMap;

use pg_query::NodeEnum;
use pg_query::protobuf::{self, a_const};
use pgsafe_analyzer::{
    AnalyzedQuery, Divergence, DivergenceKind, PgCatalog, PgTypeOid, ProKind, QualifiedName,
    TypType, Type,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ──────────────────────────────────────────────────────────────────────────
// Schema the fuzzer generates queries against.
// ──────────────────────────────────────────────────────────────────────────

const SETUP_SQL: &str = "
CREATE TYPE status AS ENUM ('draft', 'published', 'archived');
CREATE DOMAIN email AS TEXT;
CREATE TABLE users (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL,
    addr       email,
    age        INT,
    score      NUMERIC(10, 2),
    active     BOOL NOT NULL DEFAULT true,
    ratio      FLOAT8,
    tags       TEXT[],
    nums       INT[],
    st         status NOT NULL DEFAULT 'draft',
    prefs      JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    bday       DATE,
    uid        UUID
);
CREATE TABLE posts (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users(id),
    title        TEXT NOT NULL,
    body         TEXT,
    views        INT NOT NULL DEFAULT 0,
    rating       FLOAT8,
    published_at TIMESTAMPTZ
);
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    BigInt,
    Numeric,
    Float,
    Text,
    Bool,
    Timestamptz,
    Date,
    Uuid,
    Jsonb,
    Enum,
    IntArr,
    TextArr,
}

struct Col {
    name: &'static str,
    ty: Ty,
}

struct Table {
    name: &'static str,
    cols: &'static [Col],
}

const TABLES: &[Table] = &[
    Table {
        name: "users",
        cols: &[
            Col {
                name: "id",
                ty: Ty::BigInt,
            },
            Col {
                name: "name",
                ty: Ty::Text,
            },
            Col {
                name: "addr",
                ty: Ty::Text,
            },
            Col {
                name: "age",
                ty: Ty::Int,
            },
            Col {
                name: "score",
                ty: Ty::Numeric,
            },
            Col {
                name: "active",
                ty: Ty::Bool,
            },
            Col {
                name: "ratio",
                ty: Ty::Float,
            },
            Col {
                name: "tags",
                ty: Ty::TextArr,
            },
            Col {
                name: "nums",
                ty: Ty::IntArr,
            },
            Col {
                name: "st",
                ty: Ty::Enum,
            },
            Col {
                name: "prefs",
                ty: Ty::Jsonb,
            },
            Col {
                name: "created_at",
                ty: Ty::Timestamptz,
            },
            Col {
                name: "bday",
                ty: Ty::Date,
            },
            Col {
                name: "uid",
                ty: Ty::Uuid,
            },
        ],
    },
    Table {
        name: "posts",
        cols: &[
            Col {
                name: "id",
                ty: Ty::BigInt,
            },
            Col {
                name: "user_id",
                ty: Ty::BigInt,
            },
            Col {
                name: "title",
                ty: Ty::Text,
            },
            Col {
                name: "body",
                ty: Ty::Text,
            },
            Col {
                name: "views",
                ty: Ty::Int,
            },
            Col {
                name: "rating",
                ty: Ty::Float,
            },
            Col {
                name: "published_at",
                ty: Ty::Timestamptz,
            },
        ],
    },
];

/// Every column name across the schema — used by the AST mutator to swap one
/// column reference for another (often of a different type).
const ALL_COLUMN_NAMES: &[&str] = &[
    "id",
    "name",
    "addr",
    "age",
    "score",
    "active",
    "ratio",
    "tags",
    "nums",
    "st",
    "prefs",
    "created_at",
    "bday",
    "uid",
    "user_id",
    "title",
    "body",
    "views",
    "rating",
    "published_at",
];

const OPERATORS: &[&str] = &[
    "=", "<>", "<", ">", "<=", ">=", "+", "-", "*", "/", "%", "^", "||", "->", "->>", "#>", "#>>",
    "@>", "<@", "?", "&&", "|", "&", "#", "<<", ">>", "~", "~~", "!~~", "~*",
];

/// Functions with assorted signatures — calling them on the wrong argument
/// types is exactly what surfaces error-message divergences.
const FUNCTIONS: &[&str] = &[
    // string
    "length",
    "upper",
    "lower",
    "char_length",
    "octet_length",
    "trim",
    "btrim",
    "ltrim",
    "rtrim",
    "substr",
    "substring",
    "replace",
    "split_part",
    "left",
    "right",
    "lpad",
    "rpad",
    "reverse",
    "concat",
    "concat_ws",
    "format",
    "md5",
    "starts_with",
    "to_char",
    // numeric
    "abs",
    "round",
    "ceil",
    "floor",
    "trunc",
    "sqrt",
    "cbrt",
    "exp",
    "ln",
    "log",
    "power",
    "sign",
    "mod",
    "div",
    "gcd",
    "lcm",
    "width_bucket",
    // date/time
    "date_trunc",
    "date_part",
    "extract",
    "age",
    "now",
    "to_timestamp",
    "to_date",
    "make_date",
    // conditional / misc
    "coalesce",
    "nullif",
    "greatest",
    "least",
    // array
    "cardinality",
    "array_length",
    "array_upper",
    "array_lower",
    "array_ndims",
    "array_append",
    "array_cat",
    "array_remove",
    "array_position",
    "array_to_string",
    "unnest",
    // json/jsonb
    "jsonb_typeof",
    "json_typeof",
    "jsonb_array_length",
    "jsonb_object_keys",
    "jsonb_build_array",
    "jsonb_build_object",
    "to_jsonb",
    "jsonb_strip_nulls",
    "jsonb_pretty",
    // aggregates (so grouped queries and misuse get exercised)
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "string_agg",
    "array_agg",
    "bool_and",
    "bool_or",
];

const BASE_TYPE_NAMES: &[&str] = &[
    "int4",
    "int8",
    "int2",
    "numeric",
    "float8",
    "text",
    "varchar",
    "bool",
    "date",
    "timestamptz",
    "uuid",
    "jsonb",
    "bytea",
];

// ──────────────────────────────────────────────────────────────────────────
// Strategy 1 — schema-driven template generation.
// ──────────────────────────────────────────────────────────────────────────

fn literal_for(ty: Ty, rng: &mut StdRng) -> String {
    match ty {
        Ty::Int => ["0", "42", "-7", "2147483647"][rng.gen_range(0..4)].to_string(),
        Ty::BigInt => ["0", "9999999999", "-1"][rng.gen_range(0..3)].to_string(),
        Ty::Numeric => ["3.14", "0.0", "100.5"][rng.gen_range(0..3)].to_string(),
        Ty::Float => ["2.5", "1e3", "0.0"][rng.gen_range(0..3)].to_string(),
        Ty::Text => ["'hello'", "'x'", "''"][rng.gen_range(0..3)].to_string(),
        Ty::Bool => ["true", "false"][rng.gen_range(0..2)].to_string(),
        Ty::Timestamptz => "now()".to_string(),
        Ty::Date => "current_date".to_string(),
        Ty::Uuid => "'00000000-0000-0000-0000-000000000000'::uuid".to_string(),
        Ty::Jsonb => "'{}'::jsonb".to_string(),
        Ty::Enum => ["'draft'", "'published'", "'archived'"][rng.gen_range(0..3)].to_string(),
        Ty::IntArr => "ARRAY[1, 2, 3]".to_string(),
        Ty::TextArr => "ARRAY['a', 'b']".to_string(),
    }
}

/// A column reference from `table`, optionally a deliberately-wrong type.
fn random_col<'a>(table: &'a Table, rng: &mut StdRng) -> &'a Col {
    &table.cols[rng.gen_range(0..table.cols.len())]
}

/// Generate an expression. `depth` bounds recursion. The generator is only
/// loosely type-aware: it freely mixes types so the oracle sees both
/// well-typed queries (type-inference bugs) and ill-typed ones (error-message
/// bugs). `np` is the running parameter counter (see [`next_param`]).
fn gen_expr(table: &Table, depth: u32, rng: &mut StdRng, np: &mut u32) -> String {
    if depth == 0 || rng.gen_bool(0.35) {
        // Leaf: column ref or literal.
        return if rng.gen_bool(0.6) {
            random_col(table, rng).name.to_string()
        } else {
            let ty = [
                Ty::Int,
                Ty::Text,
                Ty::Bool,
                Ty::Numeric,
                Ty::Float,
                Ty::Timestamptz,
            ][rng.gen_range(0..6)];
            // ~15% of leaves are a query parameter rather than a column /
            // literal, so the oracle's input-parameter-type comparison gets
            // exercised. A bare `$pN` has no type context (PG reports it as
            // `unknown`/`text`); pin it with an explicit cast so PG infers a
            // concrete type we can check against.
            if rng.gen_bool(0.15) {
                let t = BASE_TYPE_NAMES[rng.gen_range(0..BASE_TYPE_NAMES.len())];
                return format!("(${}::{t})", next_param(np));
            }
            literal_for(ty, rng)
        };
    }
    match rng.gen_range(0..8) {
        0 => {
            // Binary op.
            let op = OPERATORS[rng.gen_range(0..OPERATORS.len())];
            format!(
                "({} {} {})",
                gen_expr(table, depth - 1, rng, np),
                op,
                gen_expr(table, depth - 1, rng, np)
            )
        }
        7 => {
            // Type-aware comparison: a column against a literal — or a bare
            // parameter — of its own type. The parameter form stresses param
            // type *inference* from operator context (PG should report the
            // param as the column's type), the highest-value param case.
            let col = random_col(table, rng);
            let op = ["=", "<>", "<", ">", "<=", ">="][rng.gen_range(0..6)];
            let rhs = if rng.gen_bool(0.4) {
                format!("${}", next_param(np))
            } else {
                literal_for(col.ty, rng)
            };
            format!("({} {} {})", col.name, op, rhs)
        }
        1 => {
            // Function call with 0..3 args.
            let f = FUNCTIONS[rng.gen_range(0..FUNCTIONS.len())];
            let nargs = rng.gen_range(0..3);
            let args: Vec<String> = (0..nargs)
                .map(|_| gen_expr(table, depth - 1, rng, np))
                .collect();
            format!("{}({})", f, args.join(", "))
        }
        2 => {
            // Cast.
            let t = BASE_TYPE_NAMES[rng.gen_range(0..BASE_TYPE_NAMES.len())];
            format!("({})::{}", gen_expr(table, depth - 1, rng, np), t)
        }
        3 => format!(
            "COALESCE({}, {})",
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np)
        ),
        4 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np)
        ),
        5 => {
            // Aggregate (valid only with/without GROUP BY; the oracle judges).
            let agg = ["count", "sum", "avg", "min", "max"][rng.gen_range(0..5)];
            format!("{}({})", agg, gen_expr(table, depth - 1, rng, np))
        }
        _ => format!("(NOT {})", gen_expr(table, depth - 1, rng, np)),
    }
}

/// Allocate the next positional parameter name (`p0`, `p1`, …). Names are
/// handed out left-to-right as the query string is built, so first-occurrence
/// order matches the `$1, $2, …` numbering PG assigns — keeping the analyzer's
/// and PG's parameter lists index-aligned for the oracle's comparison.
fn next_param(np: &mut u32) -> String {
    let name = format!("p{}", *np);
    *np += 1;
    name
}

fn pick_table(rng: &mut StdRng) -> &'static Table {
    &TABLES[rng.gen_range(0..TABLES.len())]
}

/// A standalone scalar literal of a random type (no column references) — used
/// in contexts without a FROM scope (INSERT … VALUES).
fn scalar_literal(rng: &mut StdRng) -> String {
    let ty = [
        Ty::Int,
        Ty::BigInt,
        Ty::Numeric,
        Ty::Float,
        Ty::Text,
        Ty::Bool,
        Ty::Timestamptz,
        Ty::Date,
        Ty::Uuid,
        Ty::Jsonb,
        Ty::Enum,
        Ty::IntArr,
        Ty::TextArr,
    ][rng.gen_range(0..13)];
    literal_for(ty, rng)
}

/// Pick `k` distinct columns from `cols` (partial Fisher-Yates).
fn pick_cols<'a>(cols: &[&'a Col], k: usize, rng: &mut StdRng) -> Vec<&'a Col> {
    let mut idxs: Vec<usize> = (0..cols.len()).collect();
    let k = k.min(idxs.len());
    for i in 0..k {
        let j = rng.gen_range(i..idxs.len());
        idxs.swap(i, j);
    }
    idxs[..k].iter().map(|&i| cols[i]).collect()
}

/// Top-level statement dispatcher. Each invocation owns its parameter counter.
fn gen_statement(rng: &mut StdRng) -> String {
    let np = &mut 0u32;
    match rng.gen_range(0..100) {
        0..=54 => gen_select(rng, np),
        55..=66 => gen_set_op(rng, np),
        67..=74 => gen_cte(rng, np),
        _ => gen_dml(rng, np),
    }
}

/// Full-featured SELECT: DISTINCT [ON], varied joins, subquery predicates,
/// scalar subqueries, GROUP BY / ORDER BY / LIMIT.
fn gen_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = String::from("SELECT ");

    match rng.gen_range(0..10) {
        0 => sql.push_str("DISTINCT "),
        1 => sql.push_str(&format!("DISTINCT ON ({}) ", gen_expr(table, 2, rng, np))),
        _ => {}
    }

    // Projection: 1..4 expressions, occasionally a scalar subquery.
    let n = rng.gen_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let e = if rng.gen_bool(0.15) {
                gen_scalar_subquery(rng, np)
            } else {
                gen_expr(table, 3, rng, np)
            };
            if rng.gen_bool(0.4) {
                format!("{e} AS c{i}")
            } else {
                e
            }
        })
        .collect();
    sql.push_str(&projs.join(", "));
    sql.push_str(&format!(" FROM {}", table.name));

    // Optional join — varied type. CROSS JOIN takes no ON clause.
    if rng.gen_bool(0.25) {
        let other = pick_table(rng);
        let jt =
            ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN"][rng.gen_range(0..5)];
        if jt == "CROSS JOIN" {
            sql.push_str(&format!(" CROSS JOIN {} AS j", other.name));
        } else {
            sql.push_str(&format!(
                " {jt} {} AS j ON {}",
                other.name,
                gen_expr(table, 2, rng, np)
            ));
        }
    }

    if rng.gen_bool(0.6) {
        let pred = if rng.gen_bool(0.25) {
            gen_subquery_predicate(table, rng, np)
        } else {
            gen_expr(table, 3, rng, np)
        };
        sql.push_str(&format!(" WHERE {pred}"));
    }

    if rng.gen_bool(0.2) {
        let c = random_col(table, rng);
        sql.push_str(&format!(" GROUP BY {}", c.name));
    }

    if rng.gen_bool(0.2) {
        sql.push_str(&format!(" ORDER BY {}", gen_expr(table, 2, rng, np)));
    }

    if rng.gen_bool(0.2) {
        let lim = match rng.gen_range(0..3) {
            0 => rng.gen_range(0..100).to_string(),
            1 => format!("${}", next_param(np)),
            _ => gen_expr(table, 1, rng, np),
        };
        sql.push_str(&format!(" LIMIT {lim}"));
    }

    sql
}

/// `SELECT <projs> FROM <table> [WHERE <expr>]` — no clauses that would be
/// syntactically awkward inside a set-operation branch / CTE body / subquery.
fn gen_simple_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let n = rng.gen_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), table.name);
    if rng.gen_bool(0.5) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    sql
}

/// `(SELECT <agg>(<col>) FROM <table>)` — a scalar subquery for a projection
/// slot. Mostly single-column so PG accepts it as a scalar.
fn gen_scalar_subquery(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let agg = ["count", "sum", "avg", "min", "max"][rng.gen_range(0..5)];
    let arg = gen_expr(table, 1, rng, np);
    format!("(SELECT {agg}({arg}) FROM {})", table.name)
}

/// `col IN (SELECT …)` / `[NOT] EXISTS (SELECT …)` for a WHERE clause.
fn gen_subquery_predicate(table: &'static Table, rng: &mut StdRng, np: &mut u32) -> String {
    let other = pick_table(rng);
    match rng.gen_range(0..3) {
        0 => {
            let col = random_col(table, rng);
            let inner = random_col(other, rng);
            format!(
                "{} IN (SELECT {} FROM {})",
                col.name, inner.name, other.name
            )
        }
        1 => format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            other.name,
            gen_expr(other, 2, rng, np)
        ),
        _ => format!(
            "NOT EXISTS (SELECT 1 FROM {} WHERE {})",
            other.name,
            gen_expr(other, 2, rng, np)
        ),
    }
}

/// Two simple selects combined with a set operation — exercises column-count
/// and common-type reconciliation across the branches.
fn gen_set_op(rng: &mut StdRng, np: &mut u32) -> String {
    let op = ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"][rng.gen_range(0..4)];
    format!(
        "{} {op} {}",
        gen_simple_select(rng, np),
        gen_simple_select(rng, np)
    )
}

/// `WITH cte AS (<simple select>) SELECT * FROM cte`.
fn gen_cte(rng: &mut StdRng, np: &mut u32) -> String {
    format!(
        "WITH cte AS ({}) SELECT * FROM cte",
        gen_simple_select(rng, np)
    )
}

// ── DML ──────────────────────────────────────────────────────────────────────

fn gen_dml(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.gen_range(0..3) {
        0 => gen_insert(rng, np),
        1 => gen_update(rng, np),
        _ => gen_delete(rng, np),
    }
}

/// `RETURNING *` or a small projection over the affected table.
fn gen_returning(table: &'static Table, rng: &mut StdRng, np: &mut u32) -> String {
    if rng.gen_bool(0.3) {
        return " RETURNING *".to_string();
    }
    let n = rng.gen_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    format!(" RETURNING {}", projs.join(", "))
}

/// A value for `INSERT … VALUES` — no FROM scope, so column-typed literals,
/// parameters (inferred by assignment context), NULL, DEFAULT, or a
/// deliberately mistyped literal.
fn gen_insert_value(col: &Col, rng: &mut StdRng, np: &mut u32) -> String {
    match rng.gen_range(0..10) {
        0..=4 => literal_for(col.ty, rng),
        5..=6 => format!("${}", next_param(np)),
        7 => "NULL".to_string(),
        8 => "DEFAULT".to_string(),
        _ => scalar_literal(rng),
    }
}

fn gen_insert(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    // Skip the identity `id` so most rows are insertable.
    let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
    let k = rng.gen_range(1..=cols.len().min(4));
    let chosen = pick_cols(&cols, k, rng);
    let collist = chosen.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
    let vals = chosen
        .iter()
        .map(|c| gen_insert_value(c, rng, np))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("INSERT INTO {} ({collist}) VALUES ({vals})", table.name);
    if rng.gen_bool(0.4) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

fn gen_update(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
    let k = rng.gen_range(1..=cols.len().min(3));
    let chosen = pick_cols(&cols, k, rng);
    // In UPDATE, SET expressions can reference the table's columns.
    let sets = chosen
        .iter()
        .map(|c| {
            let v = match rng.gen_range(0..10) {
                0..=4 => literal_for(c.ty, rng),
                5..=6 => format!("${}", next_param(np)),
                7 => "NULL".to_string(),
                8 => "DEFAULT".to_string(),
                _ => gen_expr(table, 1, rng, np),
            };
            format!("{} = {v}", c.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("UPDATE {} SET {sets}", table.name);
    if rng.gen_bool(0.6) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    if rng.gen_bool(0.3) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

fn gen_delete(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = format!("DELETE FROM {}", table.name);
    if rng.gen_bool(0.7) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    if rng.gen_bool(0.3) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

// ──────────────────────────────────────────────────────────────────────────
// Strategy 3 — catalog-mined, type-directed generation (valid by construction).
//
// Build a "result type → producers" index from the *live* catalog — every
// builtin (and user) function/operator, not a hardcoded list — then generate
// an expression of a requested type by picking a producer and recursing on its
// declared argument types. Every sub-expression has exactly the type its
// parent expects, so the whole query type-checks; that is what lets the oracle
// compare *column types* (the highest-value divergence on valid queries), not
// just error wording. Leaves are NOT-NULL columns when available (to exercise
// nullability) else a typed `NULL::T` / `$pN::T` (param-type coverage).
//
// Concrete-only for now: producers whose result *or* any argument is a
// pseudo-type (polymorphic `anyelement`/`anyarray`, `record`, `cstring`, …) are
// skipped — polymorphic resolution is already covered by the template
// generator, and the concrete surface is the gap. Set-returning, variadic, and
// aggregate/window procs are skipped too (special placement rules).
// ──────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum Producer {
    /// `call(arg0, arg1, …)` — `call` is the (possibly schema-qualified) name.
    Func { call: String, args: Vec<PgTypeOid> },
    /// `(left <op> right)` — binary operator, rendered with its bare name
    /// (pg_catalog only, so search-path resolution is unambiguous).
    Op {
        name: String,
        left: PgTypeOid,
        right: PgTypeOid,
    },
}

struct TypedCat {
    /// result type → producers that yield it.
    by_result: std::collections::HashMap<PgTypeOid, Vec<Producer>>,
    /// type → SQL-renderable name (`pg_catalog.int4`, `public.email`, …).
    type_name: std::collections::HashMap<PgTypeOid, String>,
    /// result types that have at least one producer (for goal selection).
    producible: Vec<PgTypeOid>,
    /// user relations: `(relname, [(col, type, not_null)])`.
    relations: Vec<(String, Vec<(String, PgTypeOid, bool)>)>,
}

/// Build the type-directed generation index from the live catalog.
fn build_typed_cat(db: &PgCatalog) -> TypedCat {
    let is_pseudo = |t: PgTypeOid| {
        db.type_row(t)
            .map(|r| r.typtype == TypType::Pseudo)
            .unwrap_or(true)
    };
    let mut type_name = std::collections::HashMap::new();
    for ty in db.iter_types() {
        if let Some(ns) = db.namespace_name(ty.typnamespace) {
            type_name.insert(ty.oid, QualifiedName::new(ns, &ty.typname).to_string());
        }
    }

    let mut by_result: std::collections::HashMap<PgTypeOid, Vec<Producer>> =
        std::collections::HashMap::new();

    for p in db.iter_procs() {
        if p.prokind != ProKind::Function || p.proretset || p.provariadic.is_some() {
            continue;
        }
        if is_pseudo(p.prorettype) || p.proargtypes.iter().any(|&a| is_pseudo(a)) {
            continue;
        }
        if p.proargtypes.len() > 4 {
            continue; // keep call sizes (and recursion fan-out) sane
        }
        let ns = match db.namespace_name(p.pronamespace) {
            Some(n @ ("pg_catalog" | "public")) => n,
            _ => continue,
        };
        by_result
            .entry(p.prorettype)
            .or_default()
            .push(Producer::Func {
                call: QualifiedName::new(ns, &p.proname).to_string(),
                args: p.proargtypes.clone(),
            });
    }

    for o in db.iter_operators() {
        let (Some(left), Some(result)) = (o.oprleft, o.oprresult) else {
            continue; // prefix/postfix ops (no left) — skip for v1
        };
        let right = o.oprright;
        if is_pseudo(left) || is_pseudo(right) || is_pseudo(result) {
            continue;
        }
        // Only pg_catalog operators so the bare `a <op> b` rendering resolves
        // unambiguously (user operators would need `OPERATOR(schema.op)`).
        if db.namespace_name(o.oprnamespace) != Some("pg_catalog") {
            continue;
        }
        by_result.entry(result).or_default().push(Producer::Op {
            name: o.oprname.clone(),
            left,
            right,
        });
    }

    let producible = by_result.keys().copied().collect();
    TypedCat {
        by_result,
        type_name,
        producible,
        relations: db.iter_relations(),
    }
}

/// Generate an expression of exactly type `goal`. `depth` bounds recursion;
/// `cols` are the in-scope columns of the chosen relation.
fn gen_typed(
    cat: &TypedCat,
    cols: &[(String, PgTypeOid, bool)],
    goal: PgTypeOid,
    depth: u32,
    rng: &mut StdRng,
    np: &mut u32,
) -> String {
    if depth > 0 && !rng.gen_bool(0.4) {
        if let Some(prods) = cat.by_result.get(&goal) {
            if !prods.is_empty() {
                return match &prods[rng.gen_range(0..prods.len())] {
                    Producer::Func { call, args } => {
                        let a: Vec<String> = args
                            .iter()
                            .map(|&at| gen_typed(cat, cols, at, depth - 1, rng, np))
                            .collect();
                        format!("{call}({})", a.join(", "))
                    }
                    Producer::Op { name, left, right } => format!(
                        "({} {} {})",
                        gen_typed(cat, cols, *left, depth - 1, rng, np),
                        name,
                        gen_typed(cat, cols, *right, depth - 1, rng, np),
                    ),
                };
            }
        }
    }
    gen_typed_leaf(cat, cols, goal, rng, np)
}

/// A leaf of exactly type `goal`: a NOT-NULL column when one exists (to keep
/// nullability interesting), occasionally a pinned `$pN::T` (param coverage),
/// else a typed `NULL::T` — always a valid value of the goal type.
fn gen_typed_leaf(
    cat: &TypedCat,
    cols: &[(String, PgTypeOid, bool)],
    goal: PgTypeOid,
    rng: &mut StdRng,
    np: &mut u32,
) -> String {
    let matching: Vec<&(String, PgTypeOid, bool)> =
        cols.iter().filter(|(_, t, _)| *t == goal).collect();
    if !matching.is_empty() && rng.gen_bool(0.6) {
        return matching[rng.gen_range(0..matching.len())].0.clone();
    }
    let tname = match cat.type_name.get(&goal) {
        Some(n) => n.clone(),
        None => return "NULL".to_string(),
    };
    if rng.gen_bool(0.15) {
        return format!("(${}::{tname})", next_param(np));
    }
    format!("NULL::{tname}")
}

/// A full type-directed SELECT: 1..4 projections, each an expression of a
/// random producible / column type, over a random relation, with an optional
/// boolean WHERE (also type-directed).
fn gen_typed_select(cat: &TypedCat, rng: &mut StdRng, np: &mut u32) -> Option<String> {
    if cat.relations.is_empty() || cat.producible.is_empty() {
        return None;
    }
    let (tbl, cols) = &cat.relations[rng.gen_range(0..cat.relations.len())];

    // Goal types to draw from: the relation's own column types plus the global
    // producible set, so column leaves are reachable and the producer surface
    // is exercised.
    let pick_goal = |rng: &mut StdRng| -> PgTypeOid {
        if !cols.is_empty() && rng.gen_bool(0.5) {
            cols[rng.gen_range(0..cols.len())].1
        } else {
            cat.producible[rng.gen_range(0..cat.producible.len())]
        }
    };

    let n = rng.gen_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let goal = pick_goal(rng);
            format!("{} AS c{i}", gen_typed(cat, cols, goal, 3, rng, np))
        })
        .collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), tbl);

    // Optional WHERE — a boolean-typed expression (so it's a *valid* filter,
    // exercising nullability/type through predicates rather than syntax).
    if rng.gen_bool(0.5) {
        let bool_oid = cat
            .type_name
            .iter()
            .find(|(_, n)| n.as_str() == "pg_catalog.bool")
            .map(|(&o, _)| o);
        if let Some(b) = bool_oid {
            sql.push_str(&format!(" WHERE {}", gen_typed(cat, cols, b, 3, rng, np)));
        }
    }
    Some(sql)
}

// ──────────────────────────────────────────────────────────────────────────
// Schema fuzzing — a seeded, additive random schema (valid by construction).
//
// Extends the fixed base (users/posts) per run with domains, composites,
// multi-dimensional arrays, typmod'd columns (`varchar(n)`, `numeric(p,s)`,
// `char(n)`), generated columns, a view, and a second schema. The type-directed
// generator picks all of this up automatically via `iter_relations` /
// `iter_types`, so each seed explores a different type surface — exactly the
// corners (typmod propagation, composite fields, view nullability,
// cross-schema resolution) where PG's semantics get hairy. Applied through the
// non-panicking `apply_sql_checked`, so a DDL disagreement is itself a finding.
// ──────────────────────────────────────────────────────────────────────────

/// Build an additive random schema string. Objects are emitted in dependency
/// order (enums → domains → composites → tables → views → second schema) and
/// named with `en_/dom_/cmp_/r_/v_/s1` prefixes so they never collide with the
/// base `users`/`posts`.
fn gen_random_schema(rng: &mut StdRng) -> String {
    const BASES: &[&str] = &[
        "int4",
        "int8",
        "int2",
        "numeric",
        "float8",
        "text",
        "bool",
        "timestamptz",
        "date",
        "uuid",
        "jsonb",
    ];
    // A scalar type rendering, sometimes with a typmod — the typmod corners
    // (length/precision propagation) are a rich source of divergences.
    fn scalar_ty(rng: &mut StdRng) -> String {
        match rng.gen_range(0..7) {
            0 => "varchar(8)".into(),
            1 => "numeric(10, 2)".into(),
            2 => "char(4)".into(),
            3 => "varchar".into(),
            _ => BASES[rng.gen_range(0..BASES.len())].into(),
        }
    }

    let mut s = String::new();
    let n_en = rng.gen_range(0..=2);
    for i in 0..n_en {
        s.push_str(&format!("CREATE TYPE en_{i} AS ENUM ('a', 'b', 'c');\n"));
    }
    let n_dom = rng.gen_range(0..=2);
    for i in 0..n_dom {
        let base = scalar_ty(rng);
        let extra = if rng.gen_bool(0.4) { " NOT NULL" } else { "" };
        s.push_str(&format!("CREATE DOMAIN dom_{i} AS {base}{extra};\n"));
    }
    let n_cmp = rng.gen_range(0..=2);
    for i in 0..n_cmp {
        s.push_str(&format!(
            "CREATE TYPE cmp_{i} AS (f0 {}, f1 {});\n",
            scalar_ty(rng),
            scalar_ty(rng),
        ));
    }

    // A column type drawn from the full palette: scalars/typmod, arrays,
    // multi-dim arrays, and any user types created above.
    let col_ty = |rng: &mut StdRng| -> String {
        let mut choices = vec![
            scalar_ty(rng),
            format!("{}[]", BASES[rng.gen_range(0..BASES.len())]),
            "int4[][]".into(),
        ];
        if n_en > 0 {
            choices.push(format!("en_{}", rng.gen_range(0..n_en)));
        }
        if n_dom > 0 {
            choices.push(format!("dom_{}", rng.gen_range(0..n_dom)));
        }
        if n_cmp > 0 {
            choices.push(format!("cmp_{}", rng.gen_range(0..n_cmp)));
        }
        choices[rng.gen_range(0..choices.len())].clone()
    };

    let n_tbl = rng.gen_range(1..=3);
    for i in 0..n_tbl {
        let mut cols = vec!["id BIGINT PRIMARY KEY".to_string()];
        for c in 0..rng.gen_range(2..=5) {
            let nn = if rng.gen_bool(0.4) { " NOT NULL" } else { "" };
            cols.push(format!("c{c} {}{nn}", col_ty(rng)));
        }
        if rng.gen_bool(0.4) {
            cols.push("g BIGINT GENERATED ALWAYS AS (id * 2) STORED".into());
        }
        s.push_str(&format!(
            "CREATE TABLE r_{i} (\n  {}\n);\n",
            cols.join(",\n  ")
        ));
    }
    if n_tbl > 0 && rng.gen_bool(0.6) {
        s.push_str("CREATE VIEW v_0 AS SELECT id, id + 1 AS idp, c0 FROM r_0;\n");
    }
    if rng.gen_bool(0.5) {
        s.push_str("CREATE SCHEMA s1;\nCREATE TABLE s1.t (id BIGINT PRIMARY KEY, val TEXT);\n");
    }
    s
}

// ──────────────────────────────────────────────────────────────────────────
// Strategy 2 — AST mutation via pg_query parse → tweak → deparse.
// ──────────────────────────────────────────────────────────────────────────

/// Param-free seed queries for the AST mutator (it feeds them to `pg_query`,
/// which only understands positional `$N`, so we avoid named params here).
const SEEDS: &[&str] = &[
    "SELECT id, name FROM users WHERE age > 18",
    "SELECT count(*), max(score) FROM users GROUP BY st",
    "SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id",
    "SELECT COALESCE(age, 0) + 1 FROM users",
    "SELECT upper(name) || '!' FROM users WHERE active",
    "SELECT id FROM users ORDER BY created_at DESC LIMIT 10",
    "SELECT tags[1], nums[2] FROM users",
    "SELECT prefs->>'theme' FROM users WHERE prefs ? 'theme'",
    "SELECT avg(rating) FROM posts WHERE views > 100",
    "SELECT CASE WHEN age < 18 THEN 'minor' ELSE 'adult' END FROM users",
    "SELECT id::text, score::int FROM users",
    "SELECT extract(year FROM created_at) FROM users",
];

fn str_node(s: &str) -> protobuf::Node {
    protobuf::Node {
        node: Some(NodeEnum::String(protobuf::String {
            sval: s.to_string(),
        })),
    }
}

/// Parse `seed`, mutate exactly `n_edits` leaf nodes in the protobuf tree, and
/// deparse back to SQL. Returns `None` if parse or deparse fails (some
/// mutations produce un-deparseable trees — we just skip those).
///
/// With `n_edits == 1` over a *valid* seed this is the single-fault mode: the
/// one perturbation is usually the sole reason the query diverges, which
/// isolates genuine wording / coverage bugs from the error-ordering noise that
/// multi-fault queries produce (PG and the analyzer picking different "first"
/// errors).
fn mutate(seed: &str, rng: &mut StdRng, n_edits: u32) -> Option<String> {
    let mut parsed = pg_query::parse(seed).ok()?;

    // SAFETY: `nodes_mut` hands back raw pointers into `parsed`'s owned
    // protobuf tree. They stay valid because `parsed` outlives this block and
    // we don't move it; we mutate through them and then deparse. The pointers
    // carry no lifetime, so the `&mut parsed` borrow ends here and `deparse`
    // (an immutable borrow) is free to read the mutated tree afterwards.
    let nodes: Vec<pg_query::NodeMut> = unsafe { parsed.protobuf.nodes_mut() }
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    if nodes.is_empty() {
        return None;
    }

    for _ in 0..n_edits {
        let idx = rng.gen_range(0..nodes.len());
        apply_mutation(nodes[idx], rng);
    }

    parsed.deparse().ok()
}

/// One mutation on a single node, dispatched on its kind.
fn apply_mutation(node: pg_query::NodeMut, rng: &mut StdRng) {
    use pg_query::NodeMut;
    unsafe {
        match node {
            NodeMut::AConst(p) if !p.is_null() => {
                let c = &mut *p;
                c.isnull = false;
                c.val = Some(match rng.gen_range(0..5) {
                    0 => a_const::Val::Ival(protobuf::Integer {
                        ival: rng.gen_range(-5..1000),
                    }),
                    1 => a_const::Val::Sval(protobuf::String {
                        sval: ["x", "hello", ""][rng.gen_range(0..3)].to_string(),
                    }),
                    2 => a_const::Val::Boolval(protobuf::Boolean {
                        boolval: rng.gen_bool(0.5),
                    }),
                    3 => a_const::Val::Fval(protobuf::Float {
                        fval: "3.14".to_string(),
                    }),
                    _ => {
                        c.isnull = true;
                        c.val = None;
                        return;
                    }
                });
            }
            NodeMut::ColumnRef(p) if !p.is_null() => {
                let cr = &mut *p;
                if let Some(last) = cr.fields.last_mut()
                    && matches!(last.node, Some(NodeEnum::String(_)))
                {
                    let name = ALL_COLUMN_NAMES[rng.gen_range(0..ALL_COLUMN_NAMES.len())];
                    *last = str_node(name);
                }
            }
            NodeMut::AExpr(p) if !p.is_null() => {
                let e = &mut *p;
                // kind 0 == AEXPR_OP (a plain binary/unary operator).
                if e.kind == 0 && e.name.len() == 1 {
                    let op = OPERATORS[rng.gen_range(0..OPERATORS.len())];
                    e.name[0] = str_node(op);
                }
            }
            NodeMut::FuncCall(p) if !p.is_null() => {
                let fc = &mut *p;
                if !fc.agg_star {
                    let f = FUNCTIONS[rng.gen_range(0..FUNCTIONS.len())];
                    fc.funcname = vec![str_node(f)];
                }
            }
            NodeMut::TypeName(p) if !p.is_null() => {
                let tn = &mut *p;
                let t = BASE_TYPE_NAMES[rng.gen_range(0..BASE_TYPE_NAMES.len())];
                tn.names = vec![str_node(t)];
                tn.typmods.clear();
                tn.array_bounds.clear();
                tn.typemod = -1;
            }
            _ => {}
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Minimization — shrink a failing query while preserving the divergence kind.
// ──────────────────────────────────────────────────────────────────────────

/// Candidate reductions of `sql` produced by structural edits to the parsed
/// SELECT (drop a projection, drop a clause, unwrap a binary expr).
fn reductions(sql: &str) -> Vec<String> {
    let Ok(parsed) = pg_query::parse(sql) else {
        return Vec::new();
    };
    // The wrapper `ParseResult` isn't `Clone`, but the inner protobuf message
    // is — work on it and deparse via the free function.
    let proto = parsed.protobuf;
    let mut out = Vec::new();

    // Work on a fresh clone per reduction so edits don't compound.
    for stmt_idx in 0..proto.stmts.len() {
        let make = |edit: &dyn Fn(&mut protobuf::SelectStmt)| -> Option<String> {
            let mut clone = proto.clone();
            let raw = clone.stmts.get_mut(stmt_idx)?;
            let node = raw.stmt.as_mut()?.node.as_mut()?;
            if let NodeEnum::SelectStmt(sel) = node {
                edit(sel.as_mut());
            } else {
                return None;
            }
            pg_query::deparse(&clone).ok()
        };

        // Drop the last projection (keep at least one).
        if let Some(s) = make(&|sel| {
            if sel.target_list.len() > 1 {
                sel.target_list.pop();
            }
        }) {
            out.push(s);
        }
        // Clear each optional clause independently.
        for clear in [
            (|sel: &mut protobuf::SelectStmt| sel.where_clause = None)
                as fn(&mut protobuf::SelectStmt),
            |sel| sel.having_clause = None,
            |sel| sel.group_clause.clear(),
            |sel| sel.sort_clause.clear(),
            |sel| sel.distinct_clause.clear(),
            |sel| sel.limit_count = None,
            |sel| sel.limit_offset = None,
            |sel| sel.from_clause.clear(),
        ] {
            if let Some(s) = make(&clear) {
                out.push(s);
            }
        }
    }
    out
}

/// Greedily shrink `sql` while the same `kind` of divergence still reproduces.
/// Bounded by a fixed number of oracle calls to keep PG round-trips in check.
fn minimize(db: &PgCatalog, sql: &str, kind: DivergenceKind) -> String {
    let mut best = sql.to_string();
    let mut budget = 80u32;
    let mut improved = true;
    while improved && budget > 0 {
        improved = false;
        for cand in reductions(&best) {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if cand.len() >= best.len() {
                continue;
            }
            if let (_, Some(div)) = db.analyze_checked(&cand)
                && div.kind == kind
            {
                best = cand;
                improved = true;
                break;
            }
        }
    }
    best
}

// ──────────────────────────────────────────────────────────────────────────
// Findings: dedup by signature, record a minimized example.
// ──────────────────────────────────────────────────────────────────────────

/// A stable signature for a divergence: kind + the message with the
/// query-specific `SQL:\n---\n…\n---\n` block stripped, so two findings with
/// the same root cause but different triggering queries collapse to one.
fn signature(div: &Divergence) -> String {
    let stripped = match (div.message.find("SQL:\n---\n"), div.message.find("\n---\n")) {
        (Some(start), _) => {
            // Remove from "SQL:" up to and including the closing "---" line.
            let after = &div.message[start..];
            if let Some(end) = after.find("\n---\n") {
                let rest = &after[end + 5..];
                format!("{}{}", &div.message[..start], rest)
            } else {
                div.message.clone()
            }
        }
        _ => div.message.clone(),
    };
    format!("{:?}|{}", div.kind, stripped)
}

struct Finding {
    kind: DivergenceKind,
    example_sql: String,
    message: String,
    /// True when surfaced by the single-fault path (high signal — a genuine
    /// single-error divergence, not error-ordering noise).
    single_fault: bool,
}

#[test]
#[ignore = "differential fuzzer: long-running, needs Docker PG via run-pg-sanity.sh"]
fn fuzz_analyze_against_pg() {
    let iters: u32 = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xC0FFEE);
    let out_dir = std::env::var("FUZZ_OUT").unwrap_or_else(|_| "target/fuzz-findings".to_string());

    let mut rng = StdRng::seed_from_u64(seed);

    // Diagnostic: `FUZZ_DUMP=N` prints N generated statements (and a shape
    // histogram) without touching the DB, so you can see what the generators
    // actually produce. Returns before any Postgres connection is needed.
    if let Ok(n) = std::env::var("FUZZ_DUMP").map(|s| s.parse::<u32>().unwrap_or(20)) {
        let mut shapes: BTreeMap<&str, u32> = BTreeMap::new();
        for _ in 0..n {
            let sql = gen_statement(&mut rng);
            let shape = if sql.starts_with("INSERT") {
                "INSERT"
            } else if sql.starts_with("UPDATE") {
                "UPDATE"
            } else if sql.starts_with("DELETE") {
                "DELETE"
            } else if sql.starts_with("WITH") {
                "CTE"
            } else if sql.contains(" UNION ")
                || sql.contains(" INTERSECT ")
                || sql.contains(" EXCEPT ")
            {
                "SET-OP"
            } else {
                "SELECT"
            };
            *shapes.entry(shape).or_default() += 1;
            eprintln!("[{shape}] {sql}");
        }
        eprintln!("\n── shape histogram ({n}) ──");
        for (s, c) in &shapes {
            eprintln!("  {s:<8} {c}");
        }
        return;
    }

    let mut db = PgCatalog::new().expect("PgCatalog::new (pg_sanity must spawn the mirror)");
    db.apply_sql(SETUP_SQL)
        .expect("setup schema must apply cleanly");

    // Schema fuzzing: extend the base with a seeded random schema, applied via
    // the non-panicking checked path. A DDL disagreement is itself a finding;
    // on any non-clean outcome we rebuild a fresh base-only catalog so a
    // possible analyzer/mirror desync can't poison the query phase.
    let mut schema_divergence: Option<Divergence> = {
        let random_schema = gen_random_schema(&mut rng);
        let (res, div) = db.apply_sql_checked(&random_schema);
        if res.is_ok() && div.is_none() {
            eprintln!("fuzz: random schema applied cleanly (extends base)");
            None
        } else {
            eprintln!("fuzz: random schema reverted (DDL divergence or invalid); base only");
            db = PgCatalog::new().expect("PgCatalog::new");
            db.apply_sql(SETUP_SQL).expect("base schema reapply");
            div
        }
    };

    // Catalog-mined, type-directed generator index (Strategy 3). Built once
    // from the live catalog so it reflects the full builtin surface plus the
    // (possibly randomized) user schema above.
    let typed_cat = build_typed_cat(&db);
    eprintln!(
        "fuzz: type-directed index — {} producible types, {} relations",
        typed_cat.producible.len(),
        typed_cat.relations.len(),
    );

    // `FUZZ_TYPED_DUMP=N` prints N type-directed samples and reports the
    // valid-by-construction rate (how many analyze cleanly *and* agree with PG),
    // then exits — a quick check that the Strategy-3 generator is healthy.
    if let Ok(n) = std::env::var("FUZZ_TYPED_DUMP").map(|s| s.parse::<u32>().unwrap_or(300)) {
        let (mut okok, mut ok_div, mut rej) = (0u32, 0u32, 0u32);
        for k in 0..n {
            let Some(q) = gen_typed_select(&typed_cat, &mut rng, &mut 0u32) else {
                continue;
            };
            if k < 8 {
                eprintln!("  [sample] {q}");
            }
            match db.analyze_checked(&q) {
                (Ok(_), None) => okok += 1,
                (Ok(_), Some(_)) => ok_div += 1,
                (Err(_), _) => rej += 1,
            }
        }
        eprintln!(
            "FUZZ_TYPED_DUMP({n}): valid+agree={okok} valid+divergent={ok_div} rejected/diverged={rej}"
        );
        return;
    }

    let mut findings: BTreeMap<String, Finding> = BTreeMap::new();
    // A DDL disagreement from the random schema is a high-signal finding in its
    // own right — record it before the query loop.
    if let Some(div) = schema_divergence.take() {
        eprintln!(
            "\n[fuzz schema] NEW {:?}\n  {}",
            div.kind,
            div.message.lines().next().unwrap_or("")
        );
        findings.insert(
            signature(&div),
            Finding {
                kind: div.kind,
                example_sql: "-- random schema DDL (see message)".to_string(),
                message: div.message,
                single_fault: true,
            },
        );
    }
    // Reusable pool of parseable queries to seed the AST mutator (the static
    // seeds plus generated ones that round-tripped through pg_query).
    let mut live_seeds: Vec<String> = SEEDS.iter().map(|s| s.to_string()).collect();
    // Pool of queries known to analyze cleanly (Ok, no divergence). The
    // single-fault mode mutates these with exactly one edit, so any resulting
    // divergence has a single root cause — much higher signal than a query
    // riddled with simultaneous errors, where PG and the analyzer merely
    // disagree on which one to report first.
    let mut valid_seeds: Vec<String> = SEEDS
        .iter()
        .filter(|s| {
            let (r, d) = db.analyze_checked(s);
            r.is_ok() && d.is_none()
        })
        .map(|s| s.to_string())
        .collect();

    let mut metamorphic_checks = 0u32;
    for i in 0..iters {
        let roll = rng.gen_range(0..100);
        // `single_fault` marks the high-signal path: exactly one perturbation
        // of a valid query, so any divergence reflects a single error whose
        // report should match PG verbatim. Multi-fault queries can diverge
        // merely on *which* simultaneous error each side reports first — by
        // design we don't treat that ordering as a bug (see CLAUDE.md), so
        // those findings are kept separate and de-prioritized.
        let single_fault = (35..70).contains(&roll) && !valid_seeds.is_empty();
        let sql = if roll < 18 {
            // Type-directed generator (Strategy 3): valid by construction, so
            // the oracle can compare column types — the highest-value
            // divergence on accepted queries. Falls back to the template
            // generator if the index is empty.
            match gen_typed_select(&typed_cat, &mut rng, &mut 0u32) {
                Some(q) => q,
                None => gen_statement(&mut rng),
            }
        } else if roll < 35 {
            // Template generator (SELECT / set-op / CTE / DML).
            let q = gen_statement(&mut rng);
            if pg_query::parse(&q).is_ok() && live_seeds.len() < 400 {
                live_seeds.push(q.clone());
            }
            q
        } else if single_fault {
            // Single-fault: one edit over a known-valid base.
            let base = valid_seeds[rng.gen_range(0..valid_seeds.len())].clone();
            match mutate(&base, &mut rng, 1) {
                Some(m) => m,
                None => continue,
            }
        } else {
            // Multi-fault: 1..=3 edits over any parseable seed.
            let base = live_seeds[rng.gen_range(0..live_seeds.len())].clone();
            let n_edits = rng.gen_range(1..=3);
            match mutate(&base, &mut rng, n_edits) {
                Some(m) => m,
                None => continue,
            }
        };

        let (result, divergence) = db.analyze_checked(&sql);
        if let (Ok(q), None) = (&result, &divergence) {
            // Grow the valid-base pool with cleanly-analyzing queries we
            // generate, so single-fault has fresh material beyond the static
            // seeds. Require pg_query to parse it too: the AST mutator re-parses
            // these and only understands positional `$N`, not the `$pN` named
            // params the template generator emits — so a parametrized query
            // would just be skipped.
            if valid_seeds.len() < 400 && pg_query::parse(&sql).is_ok() {
                valid_seeds.push(sql.clone());
            }
            // Metamorphic self-consistency (Strategy 4): a pass-through wrap
            // must preserve the column type/nullability shape.
            if q.can_run_as_subquery && rng.gen_bool(0.35) {
                metamorphic_checks += 1;
                metamorphic_check(&db, &sql, q, &mut rng, &mut findings, i);
            }
        }
        if let Some(div) = divergence {
            let sig = signature(&div);
            // Only the first query per signature is minimized + recorded
            // (minimize is expensive — gate it behind the vacant slot).
            if let std::collections::btree_map::Entry::Vacant(slot) = findings.entry(sig) {
                let minimized = minimize(&db, &sql, div.kind);
                eprintln!(
                    "\n[fuzz iter {i}] NEW {}{:?}\n  query: {minimized}\n  {}",
                    if single_fault { "[single-fault] " } else { "" },
                    div.kind,
                    div.message.lines().next().unwrap_or("")
                );
                slot.insert(Finding {
                    kind: div.kind,
                    example_sql: minimized,
                    message: div.message.clone(),
                    single_fault,
                });
            }
        }
    }

    eprintln!("fuzz: ran {metamorphic_checks} metamorphic pass-through checks");
    write_findings(&out_dir, &findings);
    print_summary(iters, &findings);

    if std::env::var("FUZZ_STRICT").is_ok() && !findings.is_empty() {
        panic!(
            "fuzz: {} unique divergence(s) found (FUZZ_STRICT). See {out_dir}/",
            findings.len()
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Strategy 4 — metamorphic self-consistency.
//
// Wrapping a subquery-able query in a *pass-through* subquery / CTE
// (`SELECT * FROM (<q>) m`, `WITH m AS (<q>) SELECT * FROM m`) preserves every
// output column's type AND nullability — that's a semantic identity in PG. The
// wire protocol doesn't expose nullability, so the differential oracle can't
// check nullability propagation directly; this metamorphic relation is the only
// way to test it. Any shape change the analyzer reports across the wrap is a
// genuine self-inconsistency (e.g. losing a PK functional-dependency NOT NULL
// through a subquery), independent of whether PG agrees.
// ──────────────────────────────────────────────────────────────────────────

/// The positional (type, nullable) shape of a query's output columns — the
/// invariant a pass-through wrap must preserve.
fn col_shape(q: &AnalyzedQuery) -> Vec<(Type, bool)> {
    q.columns
        .iter()
        .map(|c| (c.pg_type.clone(), c.nullable))
        .collect()
}

/// Check that wrapping `base` (a query that analyzed cleanly and is
/// subquery-able) in a pass-through subquery/CTE preserves its column shape.
/// Records a finding on any divergence.
fn metamorphic_check(
    db: &PgCatalog,
    base_sql: &str,
    base: &AnalyzedQuery,
    rng: &mut StdRng,
    findings: &mut BTreeMap<String, Finding>,
    iter: u32,
) {
    let wrapped = if rng.gen_bool(0.5) {
        format!("SELECT * FROM ({base_sql}) AS _m")
    } else {
        format!("WITH _m AS ({base_sql}) SELECT * FROM _m")
    };
    // Only the analyzer's *own* result matters here (self-consistency); the
    // PG cross-check on generated queries is handled by the main loop.
    let (wrapped_result, _) = db.analyze_checked(&wrapped);

    let base_shape = col_shape(base);
    let wrapped_shape = wrapped_result.as_ref().ok().map(col_shape);
    if wrapped_shape.as_ref() == Some(&base_shape) {
        return; // shape preserved — good
    }

    let wrapped_desc = match &wrapped_result {
        Ok(q) => format!("{:?}", col_shape(q)),
        Err(e) => format!("Err({e})"),
    };
    let sig = format!("metamorphic:{base_shape:?}=>{wrapped_desc}");
    if let std::collections::btree_map::Entry::Vacant(slot) = findings.entry(sig) {
        eprintln!(
            "\n[fuzz iter {iter}] NEW [metamorphic] column shape changed under pass-through wrap\n  base: {base_sql}"
        );
        slot.insert(Finding {
            kind: DivergenceKind::ColumnType,
            example_sql: base_sql.to_string(),
            message: format!(
                "metamorphic: wrapping a valid query in a pass-through subquery/CTE changed its \
                 column type/nullability shape (analyzer self-inconsistency).\n\
                 base SQL:\n---\n{base_sql}\n---\nwrapped SQL:\n---\n{wrapped}\n---\n\
                 base shape:    {base_shape:?}\nwrapped shape: {wrapped_desc}"
            ),
            single_fault: true,
        });
    }
}

fn write_findings(out_dir: &str, findings: &BTreeMap<String, Finding>) {
    if findings.is_empty() {
        return;
    }
    if std::fs::create_dir_all(out_dir).is_err() {
        eprintln!("fuzz: could not create {out_dir}; skipping file output");
        return;
    }
    for (n, f) in findings.values().enumerate() {
        // High-signal single-fault findings get a `single-` prefix so they
        // sort first and are easy to triage; ordering-prone multi-fault ones
        // get `multi-`.
        let tier = if f.single_fault { "single" } else { "multi" };
        let path = format!("{out_dir}/{tier}-{:?}-{n:03}.sql", f.kind);
        let body = format!(
            "-- divergence kind: {:?}{}\n-- {}\n--\n-- full report:\n{}\n\n{};\n",
            f.kind,
            if f.single_fault {
                " (single-fault, high signal)"
            } else {
                " (multi-fault — may be error-ordering, not a bug)"
            },
            f.message.lines().next().unwrap_or(""),
            f.message
                .lines()
                .map(|l| format!("-- {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
            f.example_sql,
        );
        let _ = std::fs::write(&path, body);
    }
    eprintln!("\nfuzz: wrote {} finding(s) to {out_dir}/", findings.len());
}

/// Coarse "family" of a finding for triage: the PG-side message (for
/// `ErrorPrefix`, the expected prefix; otherwise the first line) with quoted
/// literals collapsed to `"_"` and digit runs to `N`, so findings that differ
/// only in identifiers / constants group together.
fn family(f: &Finding) -> String {
    let line = f
        .message
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("PG (expected prefix): "))
        .or_else(|| f.message.lines().next())
        .unwrap_or("")
        .to_string();

    // Collapse "<quoted>" → "_" and runs of digits → N.
    let mut out = String::with_capacity(line.len());
    let mut in_quote = false;
    let mut prev_digit = false;
    for c in line.chars() {
        if c == '"' {
            if !in_quote {
                out.push_str("\"_\"");
            }
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            continue;
        }
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('N');
            }
            prev_digit = true;
        } else {
            prev_digit = false;
            out.push(c);
        }
    }
    out
}

fn print_summary(iters: u32, findings: &BTreeMap<String, Finding>) {
    let mut by_kind: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_family: BTreeMap<String, u32> = BTreeMap::new();
    for f in findings.values() {
        *by_kind.entry(format!("{:?}", f.kind)).or_default() += 1;
        *by_family.entry(family(f)).or_default() += 1;
    }
    let single = findings.values().filter(|f| f.single_fault).count();
    eprintln!("\n──── fuzz summary ────");
    eprintln!("iterations:        {iters}");
    eprintln!("unique divergences: {}", findings.len());
    eprintln!(
        "  single-fault (high signal): {single}   multi-fault (may be ordering): {}",
        findings.len() - single
    );
    for (kind, count) in &by_kind {
        eprintln!("  {kind:<24} {count}");
    }
    // Top families (most frequent root-cause messages) to guide triage.
    let mut fams: Vec<(&String, &u32)> = by_family.iter().collect();
    fams.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    eprintln!("── top families ──");
    for (fam, count) in fams.into_iter().take(20) {
        let fam = if fam.len() > 80 { &fam[..80] } else { fam };
        eprintln!("  {count:>4}  {fam}");
    }
    eprintln!("──────────────────────");
}

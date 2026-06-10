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
//!      propagation, since PG's wire protocol doesn't expose it; and
//!   5. a **literal-content probe** (`gen_literal_probe`) that pushes a pool of
//!      valid/invalid literal strings (`'0x1F'`, `'1_'`, `'NaN'`, `'[1,]'`, …)
//!      through every coercion context (cast, operator, COALESCE, INSERT)
//!      against a wide type surface — stressing the analyzer's parse-time
//!      input validation (`literal_input`) in both directions.
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
use rand::{RngExt, SeedableRng};

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

/// Cast targets for the literal-probe generator (Strategy 5): the base types
/// plus types whose *input syntax* the analyzer validates (or deliberately
/// doesn't) — ranges, arrays, json, datetimes, geometrics, reg*, and the
/// schema's own enum/domain. Probing `'<content>'::<type>` across this
/// surface checks both rejection wording and acceptance agreement.
const PROBE_TYPE_NAMES: &[&str] = &[
    "int2",
    "int4",
    "int8",
    "oid",
    "float4",
    "float8",
    "numeric",
    "bool",
    "uuid",
    "json",
    "jsonb",
    "int4range",
    "numrange",
    "tstzrange",
    "int4[]",
    "text[]",
    "date",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
    "interval",
    "macaddr",
    "inet",
    "point",
    "box",
    "bit",
    "varbit",
    "money",
    "regclass",
    "regtype",
    "regproc",
    "bytea",
    "text",
    "status", // enum from the fixed schema
    "email",  // domain over text
];

/// Literal contents for the probe generator — a mix of valid and invalid
/// inputs per type family: integer radix/underscore forms (PG 16+), float
/// specials, malformed numerics, array/range/json shapes, uuid variants,
/// datetime keywords, enum labels. Every entry is interesting against
/// *several* of the types above.
const LITERAL_PROBES: &[&str] = &[
    "",
    " ",
    "x",
    "hello",
    "42",
    "-7",
    " 42 ",
    "+42",
    "- 42",
    "0x1F",
    "0o17",
    "0b101",
    "1_000",
    "1__0",
    "1_",
    "_1",
    "42abc",
    "2147483648",
    "9999999999999999999999",
    "3.14",
    ".5",
    "5.",
    "1e3",
    "1e",
    "1.2.3",
    "NaN",
    "inf",
    "-Infinity",
    "t",
    "tr",
    "ye",
    "of",
    "o",
    "10",
    "{}",
    "{1,2}",
    "{\"a\": 1}",
    "[1,2]",
    "[1,2)",
    "(1,2]",
    "empty",
    " EMPTY ",
    "[1,]",
    "01",
    "nullx",
    "1.5e3",
    "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
    "a0eebc999c0b4ef8bb6d6bb9bd380a11",
    "a0-eebc999c0b4ef8bb6d6bb9bd380a11",
    "now",
    "2024-01-01",
    "1 day",
    "draft",
    "published",
    "bogus_label",
    "users",
    "no_such_relation",
];

// ──────────────────────────────────────────────────────────────────────────
// Strategy 1 — schema-driven template generation.
// ──────────────────────────────────────────────────────────────────────────

fn literal_for(ty: Ty, rng: &mut StdRng) -> String {
    match ty {
        Ty::Int => ["0", "42", "-7", "2147483647"][rng.random_range(0..4)].to_string(),
        Ty::BigInt => ["0", "9999999999", "-1"][rng.random_range(0..3)].to_string(),
        Ty::Numeric => ["3.14", "0.0", "100.5"][rng.random_range(0..3)].to_string(),
        Ty::Float => ["2.5", "1e3", "0.0"][rng.random_range(0..3)].to_string(),
        Ty::Text => ["'hello'", "'x'", "''"][rng.random_range(0..3)].to_string(),
        Ty::Bool => ["true", "false"][rng.random_range(0..2)].to_string(),
        Ty::Timestamptz => "now()".to_string(),
        Ty::Date => "current_date".to_string(),
        Ty::Uuid => "'00000000-0000-0000-0000-000000000000'::uuid".to_string(),
        Ty::Jsonb => "'{}'::jsonb".to_string(),
        Ty::Enum => ["'draft'", "'published'", "'archived'"][rng.random_range(0..3)].to_string(),
        Ty::IntArr => "ARRAY[1, 2, 3]".to_string(),
        Ty::TextArr => "ARRAY['a', 'b']".to_string(),
    }
}

/// A column reference from `table`, optionally a deliberately-wrong type.
fn random_col<'a>(table: &'a Table, rng: &mut StdRng) -> &'a Col {
    &table.cols[rng.random_range(0..table.cols.len())]
}

/// Generate an expression. `depth` bounds recursion. The generator is only
/// loosely type-aware: it freely mixes types so the oracle sees both
/// well-typed queries (type-inference bugs) and ill-typed ones (error-message
/// bugs). `np` is the running parameter counter (see [`next_param`]).
fn gen_expr(table: &Table, depth: u32, rng: &mut StdRng, np: &mut u32) -> String {
    if depth == 0 || rng.random_bool(0.35) {
        // Leaf: column ref or literal.
        return if rng.random_bool(0.6) {
            random_col(table, rng).name.to_string()
        } else {
            let ty = [
                Ty::Int,
                Ty::Text,
                Ty::Bool,
                Ty::Numeric,
                Ty::Float,
                Ty::Timestamptz,
            ][rng.random_range(0..6)];
            // ~15% of leaves are a query parameter rather than a column /
            // literal, so the oracle's input-parameter-type comparison gets
            // exercised. A third are pinned with an explicit cast (checks
            // the trivially-known type round-trips); the rest are *bare* so
            // the enclosing context — function argument, operator operand,
            // CASE branch — must infer them exactly like PG does.
            if rng.random_bool(0.05) {
                let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
                return format!("(${}::{t})", next_param(np));
            }
            if rng.random_bool(0.105) {
                return format!("${}", next_param(np));
            }
            literal_for(ty, rng)
        };
    }
    match rng.random_range(0..13) {
        0 => {
            // Binary op.
            let op = OPERATORS[rng.random_range(0..OPERATORS.len())];
            format!(
                "({} {} {})",
                gen_expr(table, depth - 1, rng, np),
                op,
                gen_expr(table, depth - 1, rng, np)
            )
        }
        8 => {
            // NULL test / IS DISTINCT FROM — always-boolean predicates that
            // accept any operand type. The DISTINCT rhs may be a bare
            // param (typed from the lhs, like `=`).
            let col = random_col(table, rng);
            match rng.random_range(0..3) {
                0 => format!("({} IS NULL)", col.name),
                1 => format!("({} IS NOT NULL)", gen_expr(table, depth - 1, rng, np)),
                _ => format!(
                    "({} IS DISTINCT FROM {})",
                    col.name,
                    lit_or_param(col.ty, rng, np)
                ),
            }
        }
        9 => {
            // BETWEEN / IN-list / = ANY(array) over a column, with literals
            // of the column's own type or bare params (typed per-bound by
            // PG); the mutator mistypes the literals later, exercising
            // per-bound coercion errors.
            let col = random_col(table, rng);
            match rng.random_range(0..3) {
                0 => format!(
                    "({} BETWEEN {} AND {})",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
                1 => format!(
                    "({} IN ({}, {}))",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
                _ => format!(
                    "({} = ANY(ARRAY[{}, {}]))",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
            }
        }
        10 => {
            // Array subscript / slice over one of the array columns.
            let arr = if rng.random_bool(0.5) { "tags" } else { "nums" };
            if table.cols.iter().any(|c| c.name == arr) {
                if rng.random_bool(0.3) {
                    format!("({arr}[1:2])")
                } else {
                    format!("({}[{}])", arr, gen_expr(table, depth - 1, rng, np))
                }
            } else {
                gen_expr(table, depth - 1, rng, np)
            }
        }
        11 => {
            // Literal-content probe in expression position: stresses the
            // analyzer's parse-time input validation (`literal_input`).
            let lit = LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())];
            let ty = PROBE_TYPE_NAMES[rng.random_range(0..PROBE_TYPE_NAMES.len())];
            format!("('{}'::{})", lit.replace('\'', "''"), ty)
        }
        12 => {
            // COLLATE decoration over a text-ish operand — sometimes on a
            // non-string type or with a bogus collation name, both PG error
            // paths the analyzer mirrors.
            let col = random_col(table, rng);
            let coll = ["\"C\"", "\"POSIX\"", "\"C\"", "\"nope\""][rng.random_range(0..4)];
            format!("({} COLLATE {coll})", col.name)
        }
        7 => {
            // Type-aware comparison: a column against a literal — or a bare
            // parameter — of its own type. The parameter form stresses param
            // type *inference* from operator context (PG should report the
            // param as the column's type), the highest-value param case.
            let col = random_col(table, rng);
            let op = ["=", "<>", "<", ">", "<=", ">="][rng.random_range(0..6)];
            let rhs = if rng.random_bool(0.4) {
                format!("${}", next_param(np))
            } else {
                literal_for(col.ty, rng)
            };
            format!("({} {} {})", col.name, op, rhs)
        }
        1 => {
            // Function call with 0..3 args.
            let f = FUNCTIONS[rng.random_range(0..FUNCTIONS.len())];
            let nargs = rng.random_range(0..3);
            let args: Vec<String> = (0..nargs)
                .map(|_| gen_expr(table, depth - 1, rng, np))
                .collect();
            format!("{}({})", f, args.join(", "))
        }
        2 => {
            // Cast.
            let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
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
            // Aggregate (valid only with/without GROUP BY; the oracle
            // judges), occasionally with a FILTER clause — placement and
            // FILTER-must-be-boolean rules get exercised for free.
            let agg = ["count", "sum", "avg", "min", "max"][rng.random_range(0..5)];
            let call = format!("{}({})", agg, gen_expr(table, depth - 1, rng, np));
            if rng.random_bool(0.2) {
                format!(
                    "({call} FILTER (WHERE {}))",
                    gen_expr(table, depth - 1, rng, np)
                )
            } else {
                call
            }
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

/// A literal of `ty` — or, some of the time, a bare `$pN` parameter in its
/// place. Bare params in rich positions (BETWEEN bounds, IN items, function
/// args, CASE branches, …) are what exercise PG's parameter-type inference;
/// this is exactly the surface where the analyzer's typing has to match
/// PG's Describe.
fn lit_or_param(ty: Ty, rng: &mut StdRng, np: &mut u32) -> String {
    if rng.random_bool(0.3) {
        format!("${}", next_param(np))
    } else {
        literal_for(ty, rng)
    }
}

/// Convert the fuzzer's named placeholders (`$pN`, the form the analyzer
/// accepts) into PG-native positional ones (`$N`) so `pg_query` can parse
/// the statement — the mutation/minimization pipeline operates on the
/// positional form. Quote-aware enough for fuzzer-generated SQL.
fn named_to_positional(sql: &str) -> String {
    rewrite_params(sql, |digits, out| {
        out.push('$');
        out.push_str(digits);
    })
}

/// Inverse of [`named_to_positional`]: deparsed/mutated SQL carries `$N`;
/// the analyzer wants `$pN`.
fn positional_to_named(sql: &str) -> String {
    rewrite_params(sql, |digits, out| {
        out.push_str("$p");
        out.push_str(digits);
    })
}

/// Shared scanner: find `$p?<digits>` outside single-quoted strings and let
/// `emit` rewrite each occurrence.
fn rewrite_params(sql: &str, emit: impl Fn(&str, &mut String)) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && c == '$' {
            let mut j = i + 1;
            if j < chars.len() && chars[j] == 'p' {
                j += 1;
            }
            let digits_start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start {
                let digits: String = chars[digits_start..j].iter().collect();
                emit(&digits, &mut out);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn pick_table(rng: &mut StdRng) -> &'static Table {
    &TABLES[rng.random_range(0..TABLES.len())]
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
    ][rng.random_range(0..13)];
    literal_for(ty, rng)
}

/// Pick `k` distinct columns from `cols` (partial Fisher-Yates).
fn pick_cols<'a>(cols: &[&'a Col], k: usize, rng: &mut StdRng) -> Vec<&'a Col> {
    let mut idxs: Vec<usize> = (0..cols.len()).collect();
    let k = k.min(idxs.len());
    for i in 0..k {
        let j = rng.random_range(i..idxs.len());
        idxs.swap(i, j);
    }
    idxs[..k].iter().map(|&i| cols[i]).collect()
}

/// Top-level statement dispatcher. Each invocation owns its parameter counter.
fn gen_statement(rng: &mut StdRng) -> String {
    let np = &mut 0u32;
    match rng.random_range(0..100) {
        0..=46 => gen_select(rng, np),
        47..=57 => gen_set_op(rng, np),
        58..=66 => gen_cte(rng, np),
        67..=72 => gen_values_select(rng, np),
        73..=78 => gen_merge(rng, np),
        _ => gen_dml(rng, np),
    }
}

/// Full-featured SELECT: DISTINCT [ON], varied joins, subquery predicates,
/// scalar subqueries, GROUP BY / ORDER BY / LIMIT.
fn gen_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = String::from("SELECT ");

    match rng.random_range(0..10) {
        0 => sql.push_str("DISTINCT "),
        1 => sql.push_str(&format!("DISTINCT ON ({}) ", gen_expr(table, 2, rng, np))),
        _ => {}
    }

    // Projection: 1..4 expressions, occasionally a scalar subquery or a
    // window function call (placement + frame rules judged by the oracle).
    let n = rng.random_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let e = if rng.random_bool(0.15) {
                gen_scalar_subquery(rng, np)
            } else if rng.random_bool(0.12) {
                gen_window_call(table, rng, np)
            } else {
                gen_expr(table, 3, rng, np)
            };
            if rng.random_bool(0.4) {
                format!("{e} AS c{i}")
            } else {
                e
            }
        })
        .collect();
    sql.push_str(&projs.join(", "));
    sql.push_str(&format!(" FROM {}", table.name));

    // Optional join — varied type. CROSS JOIN takes no ON clause; LATERAL
    // subqueries may reference the left table's columns.
    if rng.random_bool(0.25) {
        let other = pick_table(rng);
        match rng.random_range(0..6) {
            0 => sql.push_str(&format!(" CROSS JOIN {} AS j", other.name)),
            1 => sql.push_str(&format!(
                ", LATERAL (SELECT {} AS lx FROM {} WHERE {}) AS l",
                gen_expr(other, 2, rng, np),
                other.name,
                gen_expr(table, 2, rng, np),
            )),
            r => {
                let jt = ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"][r - 2];
                sql.push_str(&format!(
                    " {jt} {} AS j ON {}",
                    other.name,
                    gen_expr(table, 2, rng, np)
                ));
            }
        }
    }

    if rng.random_bool(0.6) {
        let pred = if rng.random_bool(0.25) {
            gen_subquery_predicate(table, rng, np)
        } else {
            gen_expr(table, 3, rng, np)
        };
        sql.push_str(&format!(" WHERE {pred}"));
    }

    if rng.random_bool(0.2) {
        let c = random_col(table, rng);
        // Plain column, or the grouping-set family (ROLLUP/CUBE/GROUPING
        // SETS incl. the empty set) — exercises grouping expansion and
        // aggregate-nullability under partially-grouped rows.
        match rng.random_range(0..6) {
            0 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(" GROUP BY ROLLUP ({}, {})", c.name, c2.name));
            }
            1 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(" GROUP BY CUBE ({}, {})", c.name, c2.name));
            }
            2 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(
                    " GROUP BY GROUPING SETS (({}), ({}), ())",
                    c.name, c2.name
                ));
            }
            _ => sql.push_str(&format!(" GROUP BY {}", c.name)),
        }
        // HAVING — sometimes an aggregate predicate (valid), sometimes an
        // arbitrary expression (exercises HAVING placement / boolean rules).
        if rng.random_bool(0.4) {
            let pred = if rng.random_bool(0.5) {
                format!("count(*) > {}", rng.random_range(0..5))
            } else {
                gen_expr(table, 2, rng, np)
            };
            sql.push_str(&format!(" HAVING {pred}"));
        }
    }

    if rng.random_bool(0.2) {
        sql.push_str(&format!(" ORDER BY {}", gen_expr(table, 2, rng, np)));
        match rng.random_range(0..4) {
            0 => sql.push_str(" DESC"),
            1 => sql.push_str(" NULLS FIRST"),
            2 => sql.push_str(" DESC NULLS LAST"),
            _ => {}
        }
    }

    if rng.random_bool(0.2) {
        let lim = match rng.random_range(0..3) {
            0 => rng.random_range(0..100).to_string(),
            1 => format!("${}", next_param(np)),
            _ => gen_expr(table, 1, rng, np),
        };
        sql.push_str(&format!(" LIMIT {lim}"));
        if rng.random_bool(0.3) {
            sql.push_str(&format!(" OFFSET {}", rng.random_range(0..10)));
        }
    }

    sql
}

/// A window-function call for a projection slot: ranking functions,
/// aggregates with OVER, and the value-window family (`lag`/`lead`, whose
/// edge-NULL semantics make nullability interesting).
fn gen_window_call(table: &Table, rng: &mut StdRng, np: &mut u32) -> String {
    let over = {
        let mut parts = Vec::new();
        if rng.random_bool(0.5) {
            parts.push(format!("PARTITION BY {}", random_col(table, rng).name));
        }
        let has_order = rng.random_bool(0.7);
        if has_order {
            parts.push(format!("ORDER BY {}", random_col(table, rng).name));
            // Frame clauses (need an ORDER BY to be meaningful; RANGE with
            // an offset additionally needs a sortable single key — the
            // oracle judges). A $pN offset exercises frame-bound param
            // typing (int8 for ROWS).
            if rng.random_bool(0.3) {
                parts.push(match rng.random_range(0..4) {
                    0 => "ROWS BETWEEN 1 PRECEDING AND CURRENT ROW".to_string(),
                    1 => "RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING".to_string(),
                    2 => format!("ROWS BETWEEN ${} PRECEDING AND CURRENT ROW", next_param(np)),
                    _ => "ROWS 2 PRECEDING".to_string(),
                });
            }
        }
        parts.join(" ")
    };
    match rng.random_range(0..4) {
        0 => format!(
            "{}() OVER ({over})",
            ["row_number", "rank", "dense_rank"][rng.random_range(0..3)]
        ),
        1 => format!(
            "{}({}) OVER ({over})",
            ["sum", "avg", "min", "max", "count"][rng.random_range(0..5)],
            random_col(table, rng).name
        ),
        2 if rng.random_bool(0.35) => format!(
            // Two-arg lag/lead: the offset is int4 in PG's signature — a
            // bare param here is typed through function-argument inference.
            "{}({}, ${}) OVER ({over})",
            ["lag", "lead"][rng.random_range(0..2)],
            random_col(table, rng).name,
            next_param(np)
        ),
        2 => format!(
            "{}({}) OVER ({over})",
            ["lag", "lead", "first_value", "last_value"][rng.random_range(0..4)],
            random_col(table, rng).name
        ),
        _ => format!("ntile({}) OVER ({over})", gen_expr(table, 1, rng, np)),
    }
}

/// `SELECT <projs> FROM <table> [WHERE <expr>]` — no clauses that would be
/// syntactically awkward inside a set-operation branch / CTE body / subquery.
fn gen_simple_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let n = rng.random_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), table.name);
    if rng.random_bool(0.5) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    sql
}

/// `(SELECT <agg>(<col>) FROM <table>)` — a scalar subquery for a projection
/// slot. Mostly single-column so PG accepts it as a scalar.
fn gen_scalar_subquery(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let agg = ["count", "sum", "avg", "min", "max"][rng.random_range(0..5)];
    let arg = gen_expr(table, 1, rng, np);
    format!("(SELECT {agg}({arg}) FROM {})", table.name)
}

/// `col IN (SELECT …)` / `[NOT] EXISTS (SELECT …)` for a WHERE clause.
fn gen_subquery_predicate(table: &'static Table, rng: &mut StdRng, np: &mut u32) -> String {
    let other = pick_table(rng);
    match rng.random_range(0..3) {
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
    let op = ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"][rng.random_range(0..4)];
    format!(
        "{} {op} {}",
        gen_simple_select(rng, np),
        gen_simple_select(rng, np)
    )
}

/// `WITH …` statements: pass-through CTEs, RECURSIVE ones, data-modifying
/// CTEs (`WITH x AS (DELETE … RETURNING …) SELECT …`), and a CTE feeding an
/// INSERT.
fn gen_cte(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..10) {
        // Recursive CTE: numeric or text accumulation, sometimes with a
        // param in the recursion bound.
        0..=2 => {
            let bound = if rng.random_bool(0.25) {
                format!("${}", next_param(np))
            } else {
                rng.random_range(2..20).to_string()
            };
            if rng.random_bool(0.5) {
                format!(
                    "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL \
                     SELECT n + 1 FROM r WHERE n < {bound}) SELECT * FROM r"
                )
            } else {
                format!(
                    "WITH RECURSIVE r AS (SELECT 'a'::text AS s, 1 AS n UNION ALL \
                     SELECT s || 'x', n + 1 FROM r WHERE n < {bound}) SELECT s FROM r"
                )
            }
        }
        // Data-modifying CTE: the statement's rows come from a DML's
        // RETURNING list.
        3..=4 => {
            let table = pick_table(rng);
            match rng.random_range(0..2) {
                0 => format!(
                    "WITH moved AS (DELETE FROM {} WHERE {} RETURNING id) \
                     SELECT count(*) FROM moved",
                    table.name,
                    gen_expr(table, 2, rng, np),
                ),
                _ => {
                    let col = random_col(table, rng);
                    format!(
                        "WITH up AS (UPDATE {} SET {} = {} RETURNING id, {}) \
                         SELECT * FROM up",
                        table.name,
                        col.name,
                        lit_or_param(col.ty, rng, np),
                        col.name,
                    )
                }
            }
        }
        // WITH feeding a DML.
        5 => format!(
            "WITH src AS (SELECT id, name FROM users WHERE {}) \
             INSERT INTO posts (user_id, title) SELECT id, name FROM src",
            gen_expr(&TABLES[0], 2, rng, np),
        ),
        _ => format!(
            "WITH cte AS ({}) SELECT * FROM cte",
            gen_simple_select(rng, np)
        ),
    }
}

/// `MERGE INTO … USING … ON … WHEN [NOT] MATCHED …` — exercises the merge
/// resolver: join-condition typing, per-action assignment coercion, the
/// source relation's scope inside UPDATE SET / INSERT VALUES, and action
/// conditions.
fn gen_merge(rng: &mut StdRng, np: &mut u32) -> String {
    // Fixed direction (posts ← users) so the ON join makes sense; the
    // expressions inside perturb freely.
    let mut sql = String::from("MERGE INTO posts p USING users u ON p.user_id = u.id");

    let matched_cond = if rng.random_bool(0.3) {
        format!(" AND {}", gen_expr(&TABLES[1], 1, rng, np))
    } else {
        String::new()
    };
    match rng.random_range(0..3) {
        0 => {
            // Skip col 0 (`id`, the identity PK).
            let col = &TABLES[1].cols[rng.random_range(1..TABLES[1].cols.len())];
            sql.push_str(&format!(
                " WHEN MATCHED{matched_cond} THEN UPDATE SET {} = {}",
                col.name,
                lit_or_param(col.ty, rng, np),
            ));
        }
        1 => sql.push_str(&format!(" WHEN MATCHED{matched_cond} THEN DELETE")),
        _ => sql.push_str(&format!(" WHEN MATCHED{matched_cond} THEN DO NOTHING")),
    }
    if rng.random_bool(0.7) {
        match rng.random_range(0..2) {
            0 => sql.push_str(&format!(
                " WHEN NOT MATCHED THEN INSERT (user_id, title) VALUES (u.id, {})",
                if rng.random_bool(0.4) {
                    format!("${}", next_param(np))
                } else {
                    "u.name".to_string()
                },
            )),
            _ => sql.push_str(" WHEN NOT MATCHED THEN DO NOTHING"),
        }
    }
    sql
}

// ── DML ──────────────────────────────────────────────────────────────────────

fn gen_dml(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..3) {
        0 => gen_insert(rng, np),
        1 => gen_update(rng, np),
        _ => gen_delete(rng, np),
    }
}

/// `RETURNING *` or a small projection over the affected table.
fn gen_returning(table: &'static Table, rng: &mut StdRng, np: &mut u32) -> String {
    if rng.random_bool(0.3) {
        return " RETURNING *".to_string();
    }
    let n = rng.random_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    format!(" RETURNING {}", projs.join(", "))
}

/// A value for `INSERT … VALUES` — no FROM scope, so column-typed literals,
/// parameters (inferred by assignment context), NULL, DEFAULT, or a
/// deliberately mistyped literal.
fn gen_insert_value(col: &Col, rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..10) {
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
    let k = rng.random_range(1..=cols.len().min(4));
    let chosen = pick_cols(&cols, k, rng);
    let collist = chosen.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
    let mut sql = if rng.random_bool(0.2) {
        // INSERT … SELECT — arity and per-column assignment coercion across
        // a query source instead of a VALUES list.
        let src = pick_table(rng);
        let exprs: Vec<String> = (0..k).map(|_| gen_expr(src, 2, rng, np)).collect();
        format!(
            "INSERT INTO {} ({collist}) SELECT {} FROM {}",
            table.name,
            exprs.join(", "),
            src.name
        )
    } else {
        let vals = chosen
            .iter()
            .map(|c| gen_insert_value(c, rng, np))
            .collect::<Vec<_>>()
            .join(", ");
        format!("INSERT INTO {} ({collist}) VALUES ({vals})", table.name)
    };
    // ON CONFLICT over the PK — DO NOTHING or DO UPDATE with a (sometimes
    // mistyped) SET, plus the EXCLUDED pseudo-relation occasionally.
    if rng.random_bool(0.15) {
        match rng.random_range(0..3) {
            0 => sql.push_str(" ON CONFLICT (id) DO NOTHING"),
            1 => {
                let c = chosen[rng.random_range(0..chosen.len())];
                sql.push_str(&format!(
                    " ON CONFLICT (id) DO UPDATE SET {} = {}",
                    c.name,
                    literal_for(c.ty, rng)
                ));
            }
            _ => {
                let c = chosen[rng.random_range(0..chosen.len())];
                sql.push_str(&format!(
                    " ON CONFLICT (id) DO UPDATE SET {} = EXCLUDED.{}",
                    c.name, c.name
                ));
            }
        }
    }
    if rng.random_bool(0.4) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

/// `SELECT … FROM (VALUES …) AS v(a, b)` — a derived VALUES table: column
/// aliasing, cross-row common-type resolution, and the unknown-literal
/// column case all live here.
fn gen_values_select(rng: &mut StdRng, np: &mut u32) -> String {
    let n_rows = rng.random_range(1..=3);
    let n_cols = rng.random_range(1..=3);
    let rows: Vec<String> = (0..n_rows)
        .map(|_| {
            let vals: Vec<String> = (0..n_cols)
                .map(|_| {
                    if rng.random_bool(0.15) {
                        format!("${}", next_param(np))
                    } else {
                        scalar_literal(rng)
                    }
                })
                .collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    let aliases: Vec<String> = (0..n_cols).map(|i| format!("a{i}")).collect();
    let proj = if rng.random_bool(0.5) {
        "*".to_string()
    } else {
        aliases[rng.random_range(0..aliases.len())].clone()
    };
    format!(
        "SELECT {proj} FROM (VALUES {}) AS v({})",
        rows.join(", "),
        aliases.join(", ")
    )
}

/// A standalone literal-content probe (Strategy 5): `'<content>'::<type>`
/// in one of several coercion contexts — explicit cast, comparison against
/// a typed column, COALESCE branch, INSERT assignment. Directly stresses
/// the analyzer's parse-time input validation (`literal_input`) in both
/// directions: rejections must match PG's wording, acceptances must agree
/// on the result type.
fn gen_literal_probe(rng: &mut StdRng) -> String {
    let lit = LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())].replace('\'', "''");
    let ty = PROBE_TYPE_NAMES[rng.random_range(0..PROBE_TYPE_NAMES.len())];
    match rng.random_range(0..5) {
        0 => format!("SELECT '{lit}'::{ty} AS c0"),
        1 => format!("SELECT '{lit}'::{ty} AS c0 FROM users"),
        2 => {
            // Comparison against a typed column — the literal is coerced by
            // operator resolution, not an explicit cast.
            let table = pick_table(rng);
            let col = random_col(table, rng);
            format!("SELECT id FROM {} WHERE {} = '{lit}'", table.name, col.name)
        }
        3 => {
            let table = pick_table(rng);
            let col = random_col(table, rng);
            format!("SELECT COALESCE({}, '{lit}') FROM {}", col.name, table.name)
        }
        _ => {
            // INSERT assignment context.
            let table = pick_table(rng);
            let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
            let col = cols[rng.random_range(0..cols.len())];
            format!("INSERT INTO {} ({}) VALUES ('{lit}')", table.name, col.name)
        }
    }
}

fn gen_update(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
    let k = rng.random_range(1..=cols.len().min(3));
    let chosen = pick_cols(&cols, k, rng);
    // In UPDATE, SET expressions can reference the table's columns.
    let sets = chosen
        .iter()
        .map(|c| {
            let v = match rng.random_range(0..10) {
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
    if rng.random_bool(0.6) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    if rng.random_bool(0.3) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

fn gen_delete(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = format!("DELETE FROM {}", table.name);
    if rng.random_bool(0.7) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    if rng.random_bool(0.3) {
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
    if depth > 0 && !rng.random_bool(0.4) {
        if let Some(prods) = cat.by_result.get(&goal) {
            if !prods.is_empty() {
                return match &prods[rng.random_range(0..prods.len())] {
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
    if !matching.is_empty() && rng.random_bool(0.6) {
        return matching[rng.random_range(0..matching.len())].0.clone();
    }
    let tname = match cat.type_name.get(&goal) {
        Some(n) => n.clone(),
        None => return "NULL".to_string(),
    };
    if rng.random_bool(0.15) {
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
    let (tbl, cols) = &cat.relations[rng.random_range(0..cat.relations.len())];

    // Goal types to draw from: the relation's own column types plus the global
    // producible set, so column leaves are reachable and the producer surface
    // is exercised.
    let pick_goal = |rng: &mut StdRng| -> PgTypeOid {
        if !cols.is_empty() && rng.random_bool(0.5) {
            cols[rng.random_range(0..cols.len())].1
        } else {
            cat.producible[rng.random_range(0..cat.producible.len())]
        }
    };

    let n = rng.random_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let goal = pick_goal(rng);
            format!("{} AS c{i}", gen_typed(cat, cols, goal, 3, rng, np))
        })
        .collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), tbl);

    // Optional WHERE — a boolean-typed expression (so it's a *valid* filter,
    // exercising nullability/type through predicates rather than syntax).
    if rng.random_bool(0.5) {
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
        match rng.random_range(0..7) {
            0 => "varchar(8)".into(),
            1 => "numeric(10, 2)".into(),
            2 => "char(4)".into(),
            3 => "varchar".into(),
            _ => BASES[rng.random_range(0..BASES.len())].into(),
        }
    }

    let mut s = String::new();
    let n_en = rng.random_range(0..=2);
    for i in 0..n_en {
        s.push_str(&format!("CREATE TYPE en_{i} AS ENUM ('a', 'b', 'c');\n"));
    }
    let n_dom = rng.random_range(0..=2);
    for i in 0..n_dom {
        let base = scalar_ty(rng);
        let extra = if rng.random_bool(0.4) {
            " NOT NULL"
        } else {
            ""
        };
        s.push_str(&format!("CREATE DOMAIN dom_{i} AS {base}{extra};\n"));
    }
    let n_cmp = rng.random_range(0..=2);
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
            format!("{}[]", BASES[rng.random_range(0..BASES.len())]),
            "int4[][]".into(),
        ];
        if n_en > 0 {
            choices.push(format!("en_{}", rng.random_range(0..n_en)));
        }
        if n_dom > 0 {
            choices.push(format!("dom_{}", rng.random_range(0..n_dom)));
        }
        if n_cmp > 0 {
            choices.push(format!("cmp_{}", rng.random_range(0..n_cmp)));
        }
        choices[rng.random_range(0..choices.len())].clone()
    };

    let n_tbl = rng.random_range(1..=3);
    for i in 0..n_tbl {
        let mut cols = vec!["id BIGINT PRIMARY KEY".to_string()];
        for c in 0..rng.random_range(2..=5) {
            let nn = if rng.random_bool(0.4) {
                " NOT NULL"
            } else {
                ""
            };
            cols.push(format!("c{c} {}{nn}", col_ty(rng)));
        }
        if rng.random_bool(0.4) {
            cols.push("g BIGINT GENERATED ALWAYS AS (id * 2) STORED".into());
        }
        s.push_str(&format!(
            "CREATE TABLE r_{i} (\n  {}\n);\n",
            cols.join(",\n  ")
        ));
    }
    if n_tbl > 0 && rng.random_bool(0.6) {
        s.push_str("CREATE VIEW v_0 AS SELECT id, id + 1 AS idp, c0 FROM r_0;\n");
    }
    if rng.random_bool(0.5) {
        s.push_str("CREATE SCHEMA s1;\nCREATE TABLE s1.t (id BIGINT PRIMARY KEY, val TEXT);\n");
    }
    s
}

// ──────────────────────────────────────────────────────────────────────────
// Strategy 2 — AST mutation via pg_query parse → tweak → deparse.
// ──────────────────────────────────────────────────────────────────────────

/// Seed queries for the AST mutator. Named `$pN` params are welcome: the
/// pools store the positional (`$N`) form pg_query understands, and the
/// mutation boundary converts back — mutating *around* a bare param is the
/// canonical single-fault probe of parameter-type inference.
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
    // Window functions / FILTER / HAVING / set-returning shapes.
    "SELECT row_number() OVER (ORDER BY id) FROM users",
    "SELECT sum(views) OVER (PARTITION BY user_id ORDER BY id) FROM posts",
    "SELECT lag(title) OVER (ORDER BY published_at) FROM posts",
    "SELECT count(*) FILTER (WHERE active) FROM users",
    "SELECT st, count(*) FROM users GROUP BY st HAVING count(*) > 1",
    // Predicate special forms.
    "SELECT id FROM users WHERE age BETWEEN 18 AND 65",
    "SELECT id FROM users WHERE st IN ('draft', 'published')",
    "SELECT id FROM users WHERE id = ANY(ARRAY[1, 2, 3])",
    "SELECT name FROM users WHERE addr IS DISTINCT FROM 'x'",
    "SELECT id FROM posts WHERE published_at IS NOT NULL",
    // Conditional / minmax functions.
    "SELECT GREATEST(age, 5), LEAST(views, 10) FROM users, posts",
    "SELECT NULLIF(score, 0) FROM users",
    "SELECT COALESCE(body, title, 'untitled') FROM posts",
    // DML beyond single-row VALUES.
    "INSERT INTO posts (user_id, title) SELECT id, name FROM users RETURNING id",
    "INSERT INTO users (name) VALUES ('x') ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    // Parametrized shapes — params in operator, function-argument,
    // conditional, special-form, and set-op positions.
    "SELECT id FROM users WHERE id = $p1 AND name = $p2",
    "SELECT age + $p1 FROM users",
    "SELECT name || $p1 FROM users",
    "SELECT coalesce($p1, age) FROM users",
    "SELECT substr(name, $p1) FROM users",
    "SELECT id FROM users WHERE age BETWEEN $p1 AND $p2",
    "SELECT id FROM users WHERE id = ANY($p1)",
    "SELECT CASE WHEN active THEN $p1 ELSE age END FROM users",
    "SELECT $p1 UNION ALL SELECT age FROM users",
    "SELECT prefs -> $p1 FROM users",
    // MERGE / recursive CTE / data-modifying CTE / window frames.
    "MERGE INTO posts p USING users u ON p.user_id = u.id \
     WHEN MATCHED THEN UPDATE SET views = 0 \
     WHEN NOT MATCHED THEN INSERT (user_id, title) VALUES (u.id, u.name)",
    "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) \
     SELECT * FROM r",
    "WITH moved AS (DELETE FROM posts WHERE views = 0 RETURNING id) \
     SELECT count(*) FROM moved",
    "SELECT sum(views) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM posts",
    "SELECT st, count(*) FROM users GROUP BY ROLLUP (st)",
    "SELECT name COLLATE \"C\" FROM users ORDER BY name COLLATE \"C\"",
    // Derived tables.
    "SELECT v.a FROM (VALUES (1, 'x'), (2, 'y')) AS v(a, b)",
    "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
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
        let idx = rng.random_range(0..nodes.len());
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
                c.val = Some(match rng.random_range(0..5) {
                    0 => a_const::Val::Ival(protobuf::Integer {
                        ival: rng.random_range(-5..1000),
                    }),
                    1 => a_const::Val::Sval(protobuf::String {
                        // Drawn from the literal-probe pool so constant
                        // mutations stress the input-syntax validators too
                        // (radix/underscore ints, float specials, array /
                        // range / json shapes, datetime keywords, …).
                        sval: LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())].to_string(),
                    }),
                    2 => a_const::Val::Boolval(protobuf::Boolean {
                        boolval: rng.random_bool(0.5),
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
                    let name = ALL_COLUMN_NAMES[rng.random_range(0..ALL_COLUMN_NAMES.len())];
                    *last = str_node(name);
                }
            }
            NodeMut::AExpr(p) if !p.is_null() => {
                let e = &mut *p;
                // kind 0 == AEXPR_OP (a plain binary/unary operator).
                if e.kind == 0 && e.name.len() == 1 {
                    let op = OPERATORS[rng.random_range(0..OPERATORS.len())];
                    e.name[0] = str_node(op);
                }
            }
            NodeMut::FuncCall(p) if !p.is_null() => {
                let fc = &mut *p;
                if !fc.agg_star {
                    let f = FUNCTIONS[rng.random_range(0..FUNCTIONS.len())];
                    fc.funcname = vec![str_node(f)];
                }
            }
            NodeMut::TypeName(p) if !p.is_null() => {
                let tn = &mut *p;
                let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
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
    // `sql` is in the analyzer's named form; pg_query needs positional.
    let Ok(parsed) = pg_query::parse(&named_to_positional(sql)) else {
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
            pg_query::deparse(&clone)
                .ok()
                .map(|s| positional_to_named(&s))
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
/// query-specific `SQL:\n---\n…\n---\n` block stripped and content
/// placeholders normalized (quoted strings → `"_"`, digit runs → `N`), so
/// two findings with the same root cause but different triggering queries /
/// literal contents collapse to one. Twenty probes of `'<garbage>'::date`
/// are one missing validator, not twenty findings.
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
    format!("{:?}|{}", div.kind, collapse_content(&stripped))
}

/// Collapse `"quoted"` spans to `"_"` and digit runs to `N` — shared by the
/// dedup signature and the summary's family bucketing.
fn collapse_content(line: &str) -> String {
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
        // Pools hold the positional (`$N`) form pg_query can parse.
        .map(|s| named_to_positional(s))
        .collect();

    let mut metamorphic_checks = 0u32;
    for i in 0..iters {
        let roll = rng.random_range(0..100);
        // `single_fault` marks the high-signal path: exactly one perturbation
        // of a valid query, so any divergence reflects a single error whose
        // report should match PG verbatim. Multi-fault queries can diverge
        // merely on *which* simultaneous error each side reports first — by
        // design we don't treat that ordering as a bug (see CLAUDE.md), so
        // those findings are kept separate and de-prioritized.
        // Literal probes (32..40) are single-coercion by construction, so
        // they share the high-signal tier with the single-edit mutations.
        let single_fault =
            (32..40).contains(&roll) || ((40..80).contains(&roll) && !valid_seeds.is_empty());
        let sql = if roll < 15 {
            // Type-directed generator (Strategy 3): valid by construction, so
            // the oracle can compare column types — the highest-value
            // divergence on accepted queries. Falls back to the template
            // generator if the index is empty.
            match gen_typed_select(&typed_cat, &mut rng, &mut 0u32) {
                Some(q) => q,
                None => gen_statement(&mut rng),
            }
        } else if roll < 32 {
            // Template generator (SELECT / set-op / CTE / VALUES / DML).
            // Pool the *positional* form (`$N`) — pg_query can parse it, so
            // parametrized statements feed the mutation pipeline too.
            let q = gen_statement(&mut rng);
            let positional = named_to_positional(&q);
            if pg_query::parse(&positional).is_ok() && live_seeds.len() < 400 {
                live_seeds.push(positional);
            }
            q
        } else if roll < 40 {
            // Literal-content probe (Strategy 5): single coercion of a
            // string literal into a typed context. By construction these
            // have at most one fault, so the report must match PG verbatim
            // — treated as high-signal below via `single_fault`'s sibling
            // branch in the recording (probe shapes are single-coercion).
            gen_literal_probe(&mut rng)
        } else if single_fault {
            // Single-fault: one edit over a known-valid base (pooled in
            // positional form; the analyzer wants named `$pN` back).
            let base = valid_seeds[rng.random_range(0..valid_seeds.len())].clone();
            match mutate(&base, &mut rng, 1) {
                Some(m) => positional_to_named(&m),
                None => continue,
            }
        } else {
            // Multi-fault: 1..=3 edits over any parseable seed.
            let base = live_seeds[rng.random_range(0..live_seeds.len())].clone();
            let n_edits = rng.random_range(1..=3);
            match mutate(&base, &mut rng, n_edits) {
                Some(m) => positional_to_named(&m),
                None => continue,
            }
        };

        let (result, divergence) = db.analyze_checked(&sql);
        if let (Ok(q), None) = (&result, &divergence) {
            // Grow the valid-base pool with cleanly-analyzing queries we
            // generate, so single-fault has fresh material beyond the static
            // seeds. Pool the *positional* (`$N`) form: pg_query parses it,
            // so parametrized queries join the mutation pipeline — mutating
            // *around* a bare param is exactly the single-fault shape that
            // stresses parameter-type inference.
            let positional = named_to_positional(&sql);
            if valid_seeds.len() < 400 && pg_query::parse(&positional).is_ok() {
                valid_seeds.push(positional);
            }
            // Metamorphic self-consistency (Strategy 4): a pass-through wrap
            // must preserve the column type/nullability shape.
            if q.can_run_as_subquery && rng.random_bool(0.35) {
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
    let wrapped = if rng.random_bool(0.5) {
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
        .unwrap_or("");
    collapse_content(line)
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

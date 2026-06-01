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
//! Query generation uses two strategies:
//!   1. a schema-driven **template generator** (random SELECTs over a fixed
//!      schema, with mistypes deliberately allowed), and
//!   2. an **AST mutator** that parses a seed with `pg_query`, tweaks leaf
//!      nodes in the protobuf tree (constants, column refs, operators,
//!      function names, cast targets), and deparses back to SQL — which keeps
//!      the output syntactically plausible while perturbing types.
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
//! finding was discovered).
#![cfg(feature = "pg_sanity")]

use std::collections::BTreeMap;

use pg_query::NodeEnum;
use pg_query::protobuf::{self, a_const};
use pgsafe_analyzer::{Divergence, DivergenceKind, PgCatalog};
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
    "=", "<>", "<", ">", "<=", ">=", "+", "-", "*", "/", "%", "||", "->", "->>", "@>", "#>",
];

/// Functions with assorted signatures — calling them on the wrong argument
/// types is exactly what surfaces error-message divergences.
const FUNCTIONS: &[&str] = &[
    "length",
    "upper",
    "lower",
    "char_length",
    "octet_length",
    "trim",
    "abs",
    "round",
    "ceil",
    "floor",
    "sqrt",
    "coalesce",
    "nullif",
    "greatest",
    "least",
    "cardinality",
    "array_length",
    "jsonb_typeof",
    "to_char",
    "date_trunc",
    "lower",
    "concat",
    "now",
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
/// bugs).
fn gen_expr(table: &Table, depth: u32, rng: &mut StdRng) -> String {
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
            literal_for(ty, rng)
        };
    }
    match rng.gen_range(0..8) {
        0 => {
            // Binary op.
            let op = OPERATORS[rng.gen_range(0..OPERATORS.len())];
            format!(
                "({} {} {})",
                gen_expr(table, depth - 1, rng),
                op,
                gen_expr(table, depth - 1, rng)
            )
        }
        7 => {
            // Type-aware comparison: a column against a literal of its own
            // type. Mostly well-typed, so it stresses type *inference* rather
            // than error wording.
            let col = random_col(table, rng);
            let op = ["=", "<>", "<", ">", "<=", ">="][rng.gen_range(0..6)];
            format!("({} {} {})", col.name, op, literal_for(col.ty, rng))
        }
        1 => {
            // Function call with 0..3 args.
            let f = FUNCTIONS[rng.gen_range(0..FUNCTIONS.len())];
            let nargs = rng.gen_range(0..3);
            let args: Vec<String> = (0..nargs)
                .map(|_| gen_expr(table, depth - 1, rng))
                .collect();
            format!("{}({})", f, args.join(", "))
        }
        2 => {
            // Cast.
            let t = BASE_TYPE_NAMES[rng.gen_range(0..BASE_TYPE_NAMES.len())];
            format!("({})::{}", gen_expr(table, depth - 1, rng), t)
        }
        3 => format!(
            "COALESCE({}, {})",
            gen_expr(table, depth - 1, rng),
            gen_expr(table, depth - 1, rng)
        ),
        4 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            gen_expr(table, depth - 1, rng),
            gen_expr(table, depth - 1, rng),
            gen_expr(table, depth - 1, rng)
        ),
        5 => {
            // Aggregate (valid only with/without GROUP BY; the oracle judges).
            let agg = ["count", "sum", "avg", "min", "max"][rng.gen_range(0..5)];
            format!("{}({})", agg, gen_expr(table, depth - 1, rng))
        }
        _ => format!("(NOT {})", gen_expr(table, depth - 1, rng)),
    }
}

fn gen_query(rng: &mut StdRng) -> String {
    let table = &TABLES[rng.gen_range(0..TABLES.len())];
    let mut sql = String::from("SELECT ");

    if rng.gen_bool(0.15) {
        sql.push_str("DISTINCT ");
    }

    // Projection: 1..4 expressions.
    let n = rng.gen_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let e = gen_expr(table, 3, rng);
            if rng.gen_bool(0.4) {
                format!("{} AS c{}", e, i)
            } else {
                e
            }
        })
        .collect();
    sql.push_str(&projs.join(", "));

    sql.push_str(&format!(" FROM {}", table.name));

    // Optional self-or-other join.
    if rng.gen_bool(0.2) {
        let other = &TABLES[rng.gen_range(0..TABLES.len())];
        sql.push_str(&format!(
            " JOIN {} AS j ON {}",
            other.name,
            gen_expr(table, 2, rng)
        ));
    }

    if rng.gen_bool(0.6) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 3, rng)));
    }

    if rng.gen_bool(0.2) {
        let c = random_col(table, rng);
        sql.push_str(&format!(" GROUP BY {}", c.name));
    }

    if rng.gen_bool(0.2) {
        sql.push_str(&format!(" ORDER BY {}", gen_expr(table, 2, rng)));
    }

    if rng.gen_bool(0.2) {
        // Sometimes a wrong-typed LIMIT to exercise error wording.
        let lim = if rng.gen_bool(0.5) {
            rng.gen_range(0..100).to_string()
        } else {
            gen_expr(table, 1, rng)
        };
        sql.push_str(&format!(" LIMIT {}", lim));
    }

    sql
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

/// Parse `seed`, randomly mutate 1..=3 leaf nodes in the protobuf tree, and
/// deparse back to SQL. Returns `None` if parse or deparse fails (some
/// mutations produce un-deparseable trees — we just skip those).
fn mutate(seed: &str, rng: &mut StdRng) -> Option<String> {
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

    let n_edits = rng.gen_range(1..=3);
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
                if let Some(last) = cr.fields.last_mut() {
                    if matches!(last.node, Some(NodeEnum::String(_))) {
                        let name = ALL_COLUMN_NAMES[rng.gen_range(0..ALL_COLUMN_NAMES.len())];
                        *last = str_node(name);
                    }
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
            if let (_, Some(div)) = db.analyze_checked(&cand) {
                if div.kind == kind {
                    best = cand;
                    improved = true;
                    break;
                }
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

    let mut db = PgCatalog::new().expect("PgCatalog::new (pg_sanity must spawn the mirror)");
    db.apply_sql(SETUP_SQL)
        .expect("setup schema must apply cleanly");

    let mut findings: BTreeMap<String, Finding> = BTreeMap::new();
    // Reusable pool of parseable queries to seed the AST mutator (the static
    // seeds plus generated ones that round-tripped through pg_query).
    let mut live_seeds: Vec<String> = SEEDS.iter().map(|s| s.to_string()).collect();

    for i in 0..iters {
        // ~60% template, ~40% AST mutation.
        let sql = if rng.gen_bool(0.6) {
            let q = gen_query(&mut rng);
            if pg_query::parse(&q).is_ok() && live_seeds.len() < 400 {
                live_seeds.push(q.clone());
            }
            q
        } else {
            let base = live_seeds[rng.gen_range(0..live_seeds.len())].clone();
            match mutate(&base, &mut rng) {
                Some(m) => m,
                None => continue,
            }
        };

        let (_result, divergence) = db.analyze_checked(&sql);
        if let Some(div) = divergence {
            let sig = signature(&div);
            if !findings.contains_key(&sig) {
                let minimized = minimize(&db, &sql, div.kind);
                eprintln!(
                    "\n[fuzz iter {i}] NEW {:?}\n  query: {minimized}\n  {}",
                    div.kind,
                    div.message.lines().next().unwrap_or("")
                );
                findings.insert(
                    sig,
                    Finding {
                        kind: div.kind,
                        example_sql: minimized,
                        message: div.message.clone(),
                    },
                );
            }
        }
    }

    write_findings(&out_dir, &findings);
    print_summary(iters, &findings);

    if std::env::var("FUZZ_STRICT").is_ok() && !findings.is_empty() {
        panic!(
            "fuzz: {} unique divergence(s) found (FUZZ_STRICT). See {out_dir}/",
            findings.len()
        );
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
        let path = format!("{out_dir}/{:?}-{n:03}.sql", f.kind);
        let body = format!(
            "-- divergence kind: {:?}\n-- {}\n--\n-- full report:\n{}\n\n{};\n",
            f.kind,
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

fn print_summary(iters: u32, findings: &BTreeMap<String, Finding>) {
    let mut by_kind: BTreeMap<String, u32> = BTreeMap::new();
    for f in findings.values() {
        *by_kind.entry(format!("{:?}", f.kind)).or_default() += 1;
    }
    eprintln!("\n──── fuzz summary ────");
    eprintln!("iterations:        {iters}");
    eprintln!("unique divergences: {}", findings.len());
    for (kind, count) in &by_kind {
        eprintln!("  {kind:<24} {count}");
    }
    eprintln!("──────────────────────");
}

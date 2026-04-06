# cubos_sql

Compile-time verified PostgreSQL queries for Rust. **PostgreSQL only** -- no abstraction layer, no lowest-common-denominator SQL.

Write plain SQL, get full type safety. The `sql!` macro verifies every query against a real PostgreSQL schema during `cargo build` -- column types, parameter types, nullability, and type coercion errors are all caught before your code ships.

## Features

- **Compile-time checked** -- every query is validated against your actual schema. Typos in column names, wrong parameter types, invalid SQL, and type mismatches are all compiler errors.
- **Real SQL** -- any syntax PostgreSQL accepts, `cubos_sql` accepts. CTEs, window functions, lateral joins, `DISTINCT ON`, `RETURNING`, `FOR UPDATE` -- if Postgres can parse and execute it, the macro will verify it. No restricted SQL subset, no Rust DSL.
- **Nullability-aware** -- the analyzer tracks nullability through JOINs, COALESCE, CASE, subqueries, and aggregates. `NOT NULL` columns become `T`, nullable columns become `Option<T>`.
- **Static type analysis** -- parameter types are inferred following PostgreSQL's own type resolution rules (operator/function resolution, implicit/assignment casts, common-type resolution). Type mismatches produce clear errors at compile time.
- **Zero runtime overhead** -- the macro generates concrete Rust structs with named fields. No runtime reflection, no `Box<dyn Any>`, no string-based column access.
- **PostgreSQL-native** -- first-class support for JSONB domains (`CREATE DOMAIN ... AS JSONB` mapped to Rust structs), enums (`CREATE TYPE ... AS ENUM` mapped to Rust enums), arrays, composite types, and advisory locks.

## Quick start

Add to your `Cargo.toml`:

```toml
[dependencies]
cubos_sql = "0.1"
deadpool-postgres = "0.14"
tokio-postgres = "0.7"
tokio = { version = "1", features = ["full"] }

[package.metadata.cubos_sql.database]
migrations = "./migrations"
```

Create `migrations/0001_create_users.sql`:

```sql
CREATE TABLE users (
    id    SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    age   INT,
    bio   TEXT
);
```

Use it:

```rust
let users = sql!(pool, "SELECT id, name, age FROM users")
    .fetch_all()
    .await?;

for user in &users {
    // user.id  : i32       (NOT NULL column → plain type)
    // user.name: String    (NOT NULL column → plain type)
    // user.age : Option<i32>  (nullable column → Option)
    println!("{}: {}", user.id, user.name);
}
```

The macro spins up a disposable Docker Postgres container at compile time, runs your migrations, and type-checks the query. The generated struct has correctly typed fields with proper nullability.

## Compile-time error detection

Errors in your SQL are caught at build time, not at runtime:

```rust
// Column doesn't exist → compile error
sql!(pool, "SELECT nonexistent FROM users").fetch_all().await?;
//  error: column "nonexistent" does not exist

// Wrong type in WHERE → compile error
sql!(pool, "SELECT id FROM users WHERE name").fetch_all().await?;
//  error: type mismatch: text cannot be coerced to bool

// Wrong type in LIMIT → compile error
sql!(pool, "SELECT id FROM users LIMIT true").fetch_all().await?;
//  error: type mismatch: bool cannot be coerced to int8

// Parameter type mismatch → compile error (expects i32 for age column)
let flag: bool = true;
sql!(pool, "UPDATE users SET age = $flag").execute().await?;
//  error: type mismatch: bool cannot be coerced to int4
```

## The `sql!` macro

Four terminal methods for different use cases:

```rust
// fetch_all -- returns Vec<Row>
let users = sql!(pool, "SELECT id, name FROM users")
    .fetch_all().await?;

// fetch_one -- returns a single Row (errors if empty or >1)
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;

// fetch_optional -- returns Option<Row>
let maybe = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 42)
    .fetch_optional().await?;

// execute -- returns u64 (affected rows)
let n = sql!(pool, "DELETE FROM users WHERE id = $id", id = 1)
    .execute().await?;
```

### `fetch_value` -- single-column shortcut

When your query returns a single column, `fetch_value` and `fetch_value_optional` extract the scalar directly -- no struct wrapping:

```rust
// Returns i64 directly, not a struct with a `count` field
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// Returns Option<String>
let name = sql!(pool, "SELECT name FROM users WHERE id = $id", id = 42)
    .fetch_value_optional().await?;
// name: Option<String>

// Also works with aggregates — nullable when no GROUP BY
let max_age = sql!(pool, "SELECT max(age) FROM users")
    .fetch_value().await?;
// max_age: Option<i32>
```

`fetch_value` is only generated when the query returns exactly one column. Multi-column queries use `fetch_one`/`fetch_all` with struct access.

## Named parameters

Parameters use `$name` syntax. Values can be explicitly assigned or captured from scope:

```rust
// Explicit assignment
sql!(pool, "SELECT id FROM users WHERE email = $email", email = "alice@example.com")
    .fetch_one().await?;

// Scope capture — if a variable named `email` exists, just use $email
let email = "alice@example.com";
sql!(pool, "SELECT id FROM users WHERE email = $email")
    .fetch_one().await?;
```

Parameter types are inferred from context, following PostgreSQL's rules:

```rust
// $name → String (inferred from users.name column type)
// $min_age → i32 (inferred from users.age column type)
// $limit → i64 (LIMIT requires bigint)
sql!(pool,
    "SELECT id, name FROM users WHERE name = $name AND age > $min_age LIMIT $limit",
    name = "Alice", min_age = 18, limit = 10)
    .fetch_all().await?;
```

## Nullability

The analyzer tracks nullability precisely through the entire query:

```rust
// NOT NULL columns → plain types
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id   : i64
// user.name : String

// Nullable columns → Option
let user = sql!(pool, "SELECT id, age, bio FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id  : i64
// user.age : Option<i32>   (age is nullable)
// user.bio : Option<String> (bio is nullable)

// LEFT JOIN makes the right side nullable
let row = sql!(pool,
    "SELECT u.name, p.title
     FROM users u LEFT JOIN posts p ON p.user_id = u.id
     WHERE u.id = $id", id = 1)
    .fetch_one().await?;
// row.name  : String         (left side, NOT NULL)
// row.title : Option<String> (right side of LEFT JOIN → nullable)

// COALESCE removes nullability
let row = sql!(pool, "SELECT COALESCE(age, 0) AS age FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// row.age : i32  (COALESCE with non-null fallback → NOT NULL)

// COUNT is never null, even without GROUP BY
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// But SUM/AVG/MAX without GROUP BY are nullable (empty table → NULL)
let total = sql!(pool, "SELECT sum(age) FROM users")
    .fetch_value().await?;
// total: Option<i64>
```

### Nullability annotations

Override the inferred nullability when you know better than the analyzer. Use `!` to force non-nullable and `?` to force nullable.

**Columns** -- append `!` or `?` to the alias:

```rust
// p.title comes from a LEFT JOIN, so it's inferred as Option<String>.
// But if you know the join always matches, force it with "!":
let row = sql!(pool,
    r#"SELECT u.name, p.title as "title!"
       FROM users u LEFT JOIN posts p ON p.user_id = u.id
       WHERE u.id = $id"#, id = 1)
    .fetch_one().await?;
// row.title : String  (forced NOT NULL)

// name is NOT NULL, but you can force it nullable with "?":
let row = sql!(pool, r#"SELECT name as "name?" FROM users WHERE id = $id"#, id = 1)
    .fetch_one().await?;
// row.name : Option<String>  (forced nullable)
```

**Parameters** -- append `!` or `?` to the parameter name:

```rust
// Inferred from target column:
// age is nullable → $age accepts Option<i32>
sql!(pool, "UPDATE users SET age = $age WHERE id = $id", age = Some(25), id = 1)
    .execute().await?;

// name is NOT NULL → $name requires String (not Option)
sql!(pool, "UPDATE users SET name = $name WHERE id = $id", name = "Alice", id = 1)
    .execute().await?;

// Override:
// name is NOT NULL, but $name? forces it to accept Option<String>
sql!(pool, "UPDATE users SET name = $name? WHERE id = $id",
    name = Some("Alice"), id = 1)
    .execute().await?;

// age is nullable, but $age! forces it to require i32 (not Option)
sql!(pool, "UPDATE users SET age = $age! WHERE id = $id",
    age = 25, id = 1)
    .execute().await?;
```

## Bulk insert with `$..spread`

Insert multiple rows in a single statement:

```rust
struct NewUser { name: String, email: String }

let new_users = vec![
    NewUser { name: "Alice".into(), email: "alice@example.com".into() },
    NewUser { name: "Bob".into(),   email: "bob@example.com".into() },
];

sql!(pool, "INSERT INTO users (name, email) VALUES $..new_users { name, email }")
    .execute().await?;
```

The macro expands `$..new_users { name, email }` into a multi-row `VALUES` clause with proper parameter numbering.

## Enum types

PostgreSQL enums (`CREATE TYPE ... AS ENUM`) are supported out of the box. Without configuration, they map to `String`:

```sql
CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

CREATE TABLE users (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    role user_role NOT NULL DEFAULT 'viewer'
);
```

```rust
// role is typed as String
let user = sql!(pool, "SELECT id, name, role FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
println!("Role: {}", user.role); // "admin", "editor", or "viewer"

// Parameters for enum columns also accept String
sql!(pool, "UPDATE users SET role = $role WHERE id = $id", role = "editor", id = 1)
    .execute().await?;
```

To get type-safe enum values instead of raw strings, map them in `Cargo.toml`:

```toml
[package.metadata.cubos_sql.enums]
user_role = "crate::UserRole"
```

The macro will use your Rust type for serialization/deserialization. Your type must convert to/from `String`.

## Domain types (JSONB)

PostgreSQL domains (`CREATE DOMAIN`) are supported. A domain over `JSONB` can be mapped to a Rust struct that implements `serde::Serialize` and `serde::Deserialize`:

```sql
CREATE DOMAIN user_preferences AS JSONB;

CREATE TABLE profiles (
    user_id INT PRIMARY KEY REFERENCES users(id),
    preferences user_preferences
);
```

Without configuration, JSONB domains resolve to `serde_json::Value`:

```rust
// preferences: Option<serde_json::Value> (nullable JSONB domain)
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;
```

With configuration, the macro automatically serializes/deserializes through your Rust type:

```toml
[package.metadata.cubos_sql.domains]
user_preferences = "crate::domains::UserPreferences"
```

```rust
#[derive(Serialize, Deserialize)]
struct UserPreferences { theme: String, lang: String }

// preferences: Option<UserPreferences> -- automatic deserialization
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;

if let Some(prefs) = &profile.preferences {
    println!("Theme: {}", prefs.theme);
}

// Parameters are also automatically serialized
let prefs = UserPreferences { theme: "dark".into(), lang: "pt-BR".into() };
sql!(pool, "UPDATE profiles SET preferences = $prefs WHERE user_id = $id", prefs = prefs, id = 1)
    .execute().await?;
```

Non-JSONB domains (e.g. `CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)`) are transparently unwrapped to their base type.

## Transactions

Pass a transaction directly to `sql!`:

```rust
let mut client = pool.get().await?;
let tx = client.transaction().await?;

sql!(&tx, "INSERT INTO users (name, email) VALUES ($name, $email)",
    name = "Charlie", email = "charlie@example.com")
    .execute().await?;

sql!(&tx, "UPDATE users SET name = $name WHERE email = $email",
    name = "Charles", email = "charlie@example.com")
    .execute().await?;

tx.commit().await?;
```

The `Executor` trait is implemented for:

| Type | Feature | Behavior |
|------|---------|----------|
| `deadpool_postgres::Pool` | `deadpool` (default) | Acquires a connection per query |
| `deadpool_postgres::Object` | `deadpool` (default) | Uses the pooled connection |
| `bb8::Pool<PostgresConnectionManager>` | `bb8` | Acquires a connection per query |
| `tokio_postgres::Client` | always | Uses the raw client directly |
| `tokio_postgres::Transaction<'_>` | always | Executes within the transaction |

## Migrations

### CLI

Install the CLI and manage migrations from the terminal:

```bash
cargo install cubos_sql_cli
```

```bash
# Create a new migration (generates .sql and .down.sql files)
cargo sql migrate create add_posts_table

# Apply all pending migrations
cargo sql migrate up

# Show migration status
cargo sql migrate status

# Revert the last applied migration
cargo sql migrate down

# Revert a specific migration
cargo sql migrate down 20260406120000_add_posts_table

# Force revert even without a .down.sql file
cargo sql migrate down --force
```

The CLI reads configuration from `[package.metadata.cubos_sql]` in your `Cargo.toml` and connects using the `DATABASE_URL` environment variable (supports `.env` files).

### Programmatic

You can also run migrations programmatically at application startup:

```rust
use cubos_sql::migrate::{MigrationSource, run};
use cubos_sql_core::config::MigrationsConfig;

let source = MigrationSource::from_dir(Path::new("./migrations"))?;
let config = MigrationsConfig::default();
let applied = run(&mut client, &source, &config).await?;
```

Migrations use advisory locks for safe concurrent deploys and wrap each migration in a transaction by default (opt-out with `-- no-transaction` as the first line of a migration file).

## Configuration

All configuration lives in your `Cargo.toml`:

```toml
[package.metadata.cubos_sql.database]
migrations = "./migrations"        # required — path to migration files
docker_image = "postgres"          # optional — Docker image for compile-time PG

[package.metadata.cubos_sql.migrations]
table = "public._migrations"       # optional — migration tracking table
lock_id = 713705                   # optional — advisory lock ID
use_transaction = true             # optional — wrap each migration in a tx

[package.metadata.cubos_sql.domains]
user_preferences = "crate::UserPrefs"  # optional — JSONB domain → Rust struct

[package.metadata.cubos_sql.enums]
user_role = "crate::UserRole"          # optional — PG enum → Rust enum
```

Add `.cubos_sql/` to your `.gitignore`:

```text
.cubos_sql/
```

## Requirements

- Rust 1.85+
- Docker (for compile-time query verification)
- PostgreSQL 12+ (via Docker; no local install needed)

## License

Proprietary -- Cubos Tecnologia.

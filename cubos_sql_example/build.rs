//! Re-run the `sql!` macro whenever a migration file changes.
//!
//! See `cubos_sql::build::track_migrations` for details.
fn main() {
    cubos_sql::build::track_migrations();
}

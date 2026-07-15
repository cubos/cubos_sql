//! Re-run the `sql!` macro whenever a migration file changes.
//!
//! See `typedpg::build::track_migrations` for details.
fn main() {
    typedpg::build::track_migrations();
}

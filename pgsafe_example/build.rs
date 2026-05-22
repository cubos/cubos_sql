//! Re-run the `sql!` macro whenever a migration file changes.
//!
//! See `pgsafe::build::track_migrations` for details.
fn main() {
    pgsafe::build::track_migrations();
}

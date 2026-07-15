//! Shared driver for the pg_sanity parity batteries.
//!
//! Each `tests/*.rs` file is its own crate, so this helper is pulled in via a
//! `#[path = "common/parity.rs"] mod parity;` include rather than a normal
//! module declaration.

use typedpg_analyzer::PgCatalog;

/// Run every battery entry through `analyze_checked` against a fresh mirror
/// loaded with `setup`, print each divergence (the analyzer/PG lines of the
/// report), and assert none were found. `what` names the battery in the
/// failure message.
pub fn assert_battery_parity(setup: &str, battery: &[&str], what: &str) {
    let mut db = PgCatalog::new().expect("mirror");
    db.apply_sql(setup).expect("setup");
    let mut divergences = 0;
    for sql in battery {
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
        "{divergences}/{} {what} diverged from PG",
        battery.len()
    );
}

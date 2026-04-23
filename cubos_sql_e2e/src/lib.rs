//! Shared types for the cubos_sql end-to-end tests.
//!
//! These are referenced from `[package.metadata.cubos_sql.domains]` and
//! `[package.metadata.cubos_sql.enums]` in this crate's Cargo.toml. The
//! `sql!` macro emits the paths declared there literally into generated
//! code, so they must be reachable via `::cubos_sql_e2e::…` from both the
//! crate itself and from `tests/*.rs` integration binaries.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// JSONB-backed domain value. The macro serializes via `serde_json::to_value`
/// for binding and `serde_json::from_value` when reading rows, so the only
/// trait bounds required are `Serialize + Deserialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserPreferences {
    pub theme: String,
    pub newsletter: bool,
    pub daily_digest_limit: u32,
}

/// Enum mapped to the `post_status` PG enum. The macro calls `.to_string()`
/// on outbound values and `.parse::<PostStatus>()` on inbound values, so we
/// need `Display` and `FromStr` implementations whose textual form matches
/// the PG labels exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostStatus {
    Draft,
    Published,
    Archived,
}

impl fmt::Display for PostStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PostStatus::Draft => "draft",
            PostStatus::Published => "published",
            PostStatus::Archived => "archived",
        };
        f.write_str(s)
    }
}

impl FromStr for PostStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(PostStatus::Draft),
            "published" => Ok(PostStatus::Published),
            "archived" => Ok(PostStatus::Archived),
            other => Err(format!("unknown post_status: {other}")),
        }
    }
}

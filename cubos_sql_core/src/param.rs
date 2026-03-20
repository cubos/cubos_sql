/// A named parameter extracted from SQL (e.g., `$user_id`).
///
/// Parameters are deduplicated: each unique name appears once, in order of
/// first appearance. The index in the `params` vec (+1) is the positional
/// placeholder used in the rewritten SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The parameter name without the `$` prefix (e.g. `"user_id"`).
    pub name: String,
}

/// A spread parameter extracted from SQL (e.g., `$..items` or `$..items { name, email }`).
///
/// The spread token is removed from the output SQL. The `offset` field marks
/// the byte position in the output SQL where the proc macro must insert the
/// expanded positional placeholders (e.g. `($1,$2,$3),($4,$5,$6)`).
///
/// Positional indices for spreads start at `params.len() + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadParam {
    /// The spread name without the `$..` prefix (e.g. `"items"`).
    pub name: String,
    /// Explicit field names when provided inline (e.g. `{ name, email }`), or `None` if omitted.
    pub fields: Option<Vec<String>>,
    /// Byte offset in the output SQL where the expanded placeholders should be inserted.
    pub offset: usize,
}

/// The result of lexing a SQL query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexOutput {
    /// SQL with named params rewritten to positional placeholders ($1, $2, ...).
    /// Spread tokens are removed — the caller must insert expanded placeholders
    /// at each spread's `offset`.
    pub sql: String,
    /// Unique named parameters in order of first appearance.
    /// `params[0]` corresponds to `$1`, `params[1]` to `$2`, etc.
    pub params: Vec<Param>,
    /// Spread parameters in order of appearance.
    pub spreads: Vec<SpreadParam>,
}

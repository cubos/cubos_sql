//! Data structures representing parameters extracted from SQL templates.
//!
//! These types are produced by the [lexer](crate::lexer) and consumed by the
//! proc macro during code generation. They are not typically used directly by
//! end users of the `cubos_sql` library.

/// A named parameter extracted from a SQL template (e.g., `$user_id`).
///
/// Parameters are deduplicated: each unique name appears once, in order of
/// first appearance. The index in the `params` vec (+1) is the positional
/// placeholder used in the rewritten SQL (i.e., `params[0]` becomes `$1`).
///
/// Nullability annotation: `$foo` is non-nullable (the default), `$foo?` is
/// nullable (the generated Rust type will be `Option<T>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The parameter name without the `$` prefix and `?` suffix (e.g., `"user_id"`).
    pub name: String,
    /// Whether this parameter is nullable (`$foo?` syntax).
    /// When true, the generated Rust type is `Option<T>`.
    pub nullable: bool,
}

/// A field inside a spread parameter, with optional nullable annotation.
///
/// `email?` in `$..items { name, email?, age }` produces
/// `SpreadField { name: "email", nullable: true }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadField {
    /// Field name without the `?` suffix.
    pub name: String,
    /// Whether this field is nullable (`field?` syntax).
    pub nullable: bool,
}

impl From<&str> for SpreadField {
    fn from(s: &str) -> Self {
        SpreadField {
            name: s.to_string(),
            nullable: false,
        }
    }
}

/// A spread parameter extracted from a SQL template.
///
/// Spread parameters use the `$..name` or `$..name { field1, field2? }` syntax
/// and are used for bulk inserts. The spread token is removed from the output
/// SQL, and the [`offset`](SpreadParam::offset) field marks the byte position
/// where the proc macro will insert the expanded positional placeholders
/// (e.g., `($1,$2,$3),($4,$5,$6)`).
///
/// Positional indices for spread placeholders start after the last named
/// parameter index (`params.len() + 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadParam {
    /// The spread name without the `$..` prefix (e.g., `"items"`).
    pub name: String,
    /// Explicit fields when provided inline (e.g., `{ name, email?, age }`),
    /// or `None` if the spread was written without a field list.
    pub fields: Option<Vec<SpreadField>>,
    /// Byte offset in the output SQL where the expanded placeholders should be
    /// inserted by the code generator.
    pub offset: usize,
}

/// The result of lexing a SQL template via [`crate::lexer::lex`].
///
/// Contains the rewritten SQL (with positional placeholders), the list of
/// deduplicated named parameters, and any spread parameters found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexOutput {
    /// SQL with named params rewritten to positional placeholders (`$1`, `$2`, ...).
    ///
    /// Spread tokens are removed from this string. The caller must insert
    /// expanded placeholders at each spread's [`offset`](SpreadParam::offset).
    pub sql: String,
    /// Unique named parameters in order of first appearance.
    ///
    /// `params[0]` corresponds to `$1`, `params[1]` to `$2`, etc.
    pub params: Vec<Param>,
    /// Spread parameters in order of appearance in the SQL.
    pub spreads: Vec<SpreadParam>,
}

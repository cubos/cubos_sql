//! SQL template lexer for `cubos_sql`.
//!
//! This module provides the [`lex`] function, which takes a SQL string
//! containing `$name` parameters and `$..spread` syntax and produces a
//! [`LexOutput`] with the SQL rewritten to use PostgreSQL positional
//! placeholders (`$1`, `$2`, ...).
//!
//! The lexer is **not** a full SQL parser. It tracks just enough state to
//! distinguish between normal SQL context and string literals, comments,
//! dollar-quoted strings, and quoted identifiers. Parameters are only
//! extracted in normal context -- `$name` inside a string literal or comment
//! is left untouched.

use crate::param::{LexOutput, Param, SpreadField, SpreadParam};

/// An error produced when the lexer encounters invalid SQL syntax.
///
/// All variants include the byte `position` where the problem was detected.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum LexError {
    /// A single-quoted string literal was opened but never closed.
    UnclosedString { position: usize },
    /// A `/* ... */` block comment was opened but never closed.
    UnclosedBlockComment { position: usize },
    /// A dollar-quoted string (`$$...$$` or `$tag$...$tag$`) was opened but
    /// never closed.
    UnclosedDollarQuote { tag: String, position: usize },
    /// A double-quoted identifier (`"..."`) was opened but never closed.
    UnclosedQuotedIdentifier { position: usize },
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedString { position } => {
                write!(f, "unclosed string literal at byte {position}")
            }
            Self::UnclosedBlockComment { position } => {
                write!(f, "unclosed block comment at byte {position}")
            }
            Self::UnclosedDollarQuote { tag, position } => {
                write!(f, "unclosed dollar-quote ${tag}$ at byte {position}")
            }
            Self::UnclosedQuotedIdentifier { position } => {
                write!(f, "unclosed quoted identifier at byte {position}")
            }
        }
    }
}

impl std::error::Error for LexError {}

/// Internal state machine states for the lexer.
enum LexState {
    Normal,
    StringLiteral(usize),
    DollarQuote(String, usize),
    LineComment,
    /// Block comment with nesting depth and start position of the outermost `/*`.
    BlockComment(usize, usize),
    QuotedIdentifier(usize),
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn read_ident(chars: &[char], start: usize) -> &[char] {
    let mut end = start;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    &chars[start..end]
}

/// Lex a SQL template string, extracting named and spread parameters.
///
/// Rewrites `$name` placeholders to positional PostgreSQL placeholders
/// (`$1`, `$2`, ...) and removes `$..spread` tokens, recording their byte
/// offsets for later expansion by the code generator.
///
/// Parameters inside string literals, comments, dollar-quoted strings, and
/// quoted identifiers are ignored.
///
/// # Errors
///
/// Returns a [`LexError`] if the SQL contains an unclosed string literal,
/// block comment, dollar-quoted string, or quoted identifier.
///
/// # Examples
///
/// ```ignore
/// let output = lex("SELECT * FROM users WHERE id = $id AND name = $name").unwrap();
/// assert_eq!(output.sql, "SELECT * FROM users WHERE id = $1 AND name = $2");
/// assert_eq!(output.params.len(), 2);
/// assert_eq!(output.params[0].name, "id");
/// assert_eq!(output.params[1].name, "name");
/// ```
pub(crate) fn lex(sql: &str) -> Result<LexOutput, LexError> {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut state = LexState::Normal;
    let mut out = String::with_capacity(sql.len());
    let mut params: Vec<Param> = Vec::new();
    let mut spreads: Vec<SpreadParam> = Vec::new();
    // Map from param name to its 1-based positional index
    let mut param_indices: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut i = 0;
    // Track byte position incrementally (avoids a separate Vec<usize> allocation).
    let char_byte_lens: Vec<u8> = sql.chars().map(|c| c.len_utf8() as u8).collect();
    let byte_offset_of =
        |idx: usize, lens: &[u8]| -> usize { lens[..idx].iter().map(|&b| b as usize).sum() };

    while i < len {
        match &state {
            LexState::Normal => {
                if chars[i] == '\'' {
                    state = LexState::StringLiteral(byte_offset_of(i, &char_byte_lens));
                    out.push('\'');
                    i += 1;
                } else if chars[i] == '"' {
                    state = LexState::QuotedIdentifier(byte_offset_of(i, &char_byte_lens));
                    out.push('"');
                    i += 1;
                } else if chars[i] == '-' && i + 1 < len && chars[i + 1] == '-' {
                    state = LexState::LineComment;
                    out.push('-');
                    out.push('-');
                    i += 2;
                } else if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
                    state = LexState::BlockComment(1, byte_offset_of(i, &char_byte_lens));
                    out.push('/');
                    out.push('*');
                    i += 2;
                } else if chars[i] == '$' {
                    let pos = byte_offset_of(i, &char_byte_lens);
                    // Check for spread: $..
                    if i + 2 < len && chars[i + 1] == '.' && chars[i + 2] == '.' {
                        let ident_start = i + 3;
                        if ident_start < len && is_ident_start(chars[ident_start]) {
                            let ident_chars = read_ident(&chars, ident_start);
                            let name: String = ident_chars.iter().collect();
                            let after_ident = ident_start + ident_chars.len();
                            // Check for optional { fields }
                            let mut fi = after_ident;
                            while fi < len && chars[fi].is_ascii_whitespace() {
                                fi += 1;
                            }
                            let out_offset = out.len();
                            if fi < len && chars[fi] == '{' {
                                fi += 1; // skip {
                                let mut fields = Vec::new();
                                loop {
                                    while fi < len && chars[fi].is_ascii_whitespace() {
                                        fi += 1;
                                    }
                                    if fi >= len {
                                        break;
                                    }
                                    if chars[fi] == '}' {
                                        fi += 1;
                                        break;
                                    }
                                    if is_ident_start(chars[fi]) {
                                        let fc = read_ident(&chars, fi);
                                        let field_name: String = fc.iter().collect();
                                        fi += fc.len();
                                        // Check for nullable annotation `?` on field.
                                        let field_nullable = fi < len && chars[fi] == '?';
                                        if field_nullable {
                                            fi += 1;
                                        }
                                        fields.push(SpreadField {
                                            name: field_name,
                                            nullable: field_nullable,
                                        });
                                        while fi < len && chars[fi].is_ascii_whitespace() {
                                            fi += 1;
                                        }
                                        if fi < len && chars[fi] == ',' {
                                            fi += 1;
                                        }
                                    } else {
                                        fi += 1;
                                    }
                                }
                                // Don't emit spread token — just record the offset
                                spreads.push(SpreadParam {
                                    name,
                                    fields: Some(fields),
                                    offset: out_offset,
                                });
                                i = fi;
                            } else {
                                spreads.push(SpreadParam {
                                    name,
                                    fields: None,
                                    offset: out_offset,
                                });
                                i = after_ident;
                            }
                        } else {
                            out.push('$');
                            i += 1;
                        }
                    } else if i + 1 < len && chars[i + 1] == '$' {
                        // Dollar-quote $$
                        state = LexState::DollarQuote(String::new(), pos);
                        out.push('$');
                        out.push('$');
                        i += 2;
                    } else if i + 1 < len && is_ident_start(chars[i + 1]) {
                        // Could be $tag$ (dollar-quote) or $param
                        let ident_chars = read_ident(&chars, i + 1);
                        let ident: String = ident_chars.iter().collect();
                        let after = i + 1 + ident_chars.len();
                        if after < len && chars[after] == '$' {
                            // Dollar-quote $tag$
                            state = LexState::DollarQuote(ident.clone(), pos);
                            let span: String = chars[i..=after].iter().collect();
                            out.push_str(&span);
                            i = after + 1;
                        } else {
                            // Named param $ident — check for nullability annotation `?` or `!`
                            let (nullable, consume_to) = if after < len && chars[after] == '?' {
                                (Some(true), after + 1)
                            } else if after < len && chars[after] == '!' {
                                (Some(false), after + 1)
                            } else {
                                (None, after)
                            };

                            // Deduplicate by name
                            let next_idx = if let Some(&idx) = param_indices.get(&ident) {
                                idx
                            } else {
                                let idx = param_indices.len() + 1;
                                param_indices.insert(ident.clone(), idx);
                                params.push(Param {
                                    name: ident,
                                    nullable,
                                    sql_offsets: Vec::new(),
                                });
                                idx
                            };
                            let placeholder = format!("${next_idx}");
                            out.push_str(&placeholder);
                            // Record the byte offset right after this placeholder.
                            params[next_idx - 1].sql_offsets.push(out.len());
                            i = consume_to;
                        }
                    } else {
                        // Literal $ (e.g., $1 native PG placeholder)
                        out.push('$');
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            LexState::StringLiteral(start_pos) => {
                let start_pos = *start_pos;
                if chars[i] == '\'' {
                    if i + 1 < len && chars[i + 1] == '\'' {
                        out.push('\'');
                        out.push('\'');
                        i += 2;
                    } else {
                        out.push('\'');
                        state = LexState::Normal;
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::StringLiteral(_) = state
                {
                    return Err(LexError::UnclosedString {
                        position: start_pos,
                    });
                }
            }
            LexState::DollarQuote(tag, start_pos) => {
                let tag = tag.clone();
                let start_pos = *start_pos;
                if chars[i] == '$' {
                    if tag.is_empty() {
                        if i + 1 < len && chars[i + 1] == '$' {
                            out.push('$');
                            out.push('$');
                            state = LexState::Normal;
                            i += 2;
                            continue;
                        }
                    } else {
                        let close: Vec<char> = format!("${tag}$").chars().collect();
                        if i + close.len() <= len && chars[i..i + close.len()] == close[..] {
                            let s: String = close.iter().collect();
                            out.push_str(&s);
                            state = LexState::Normal;
                            i += close.len();
                            continue;
                        }
                    }
                }
                out.push(chars[i]);
                i += 1;
                if i >= len
                    && let LexState::DollarQuote(_, _) = state
                {
                    return Err(LexError::UnclosedDollarQuote {
                        tag,
                        position: start_pos,
                    });
                }
            }
            LexState::LineComment => {
                out.push(chars[i]);
                if chars[i] == '\n' {
                    state = LexState::Normal;
                }
                i += 1;
            }
            LexState::BlockComment(depth, start_pos) => {
                let depth = *depth;
                let start_pos = *start_pos;
                if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
                    // Nested block comment
                    out.push('/');
                    out.push('*');
                    state = LexState::BlockComment(depth + 1, start_pos);
                    i += 2;
                } else if chars[i] == '*' && i + 1 < len && chars[i + 1] == '/' {
                    out.push('*');
                    out.push('/');
                    if depth == 1 {
                        state = LexState::Normal;
                    } else {
                        state = LexState::BlockComment(depth - 1, start_pos);
                    }
                    i += 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::BlockComment(_, _) = state
                {
                    return Err(LexError::UnclosedBlockComment {
                        position: start_pos,
                    });
                }
            }
            LexState::QuotedIdentifier(start_pos) => {
                let start_pos = *start_pos;
                if chars[i] == '"' {
                    if i + 1 < len && chars[i + 1] == '"' {
                        // Escaped double-quote inside quoted identifier
                        out.push('"');
                        out.push('"');
                        i += 2;
                    } else {
                        out.push('"');
                        state = LexState::Normal;
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
                if i >= len
                    && let LexState::QuotedIdentifier(_) = state
                {
                    return Err(LexError::UnclosedQuotedIdentifier {
                        position: start_pos,
                    });
                }
            }
        }
    }

    match &state {
        LexState::Normal | LexState::LineComment => {}
        LexState::StringLiteral(p) => return Err(LexError::UnclosedString { position: *p }),
        LexState::DollarQuote(tag, p) => {
            return Err(LexError::UnclosedDollarQuote {
                tag: tag.clone(),
                position: *p,
            });
        }
        LexState::BlockComment(_, p) => {
            return Err(LexError::UnclosedBlockComment { position: *p });
        }
        LexState::QuotedIdentifier(p) => {
            return Err(LexError::UnclosedQuotedIdentifier { position: *p });
        }
    }

    Ok(LexOutput {
        sql: out,
        params,
        spreads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_params() {
        let out = lex("SELECT 1").unwrap();
        assert_eq!(out.sql, "SELECT 1");
        assert!(out.params.is_empty());
        assert!(out.spreads.is_empty());
    }

    #[test]
    fn single_param() {
        let out = lex("SELECT * FROM users WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT * FROM users WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn multiple_params() {
        let out = lex("INSERT INTO t (a, b, c) VALUES ($a, $b, $c)").unwrap();
        assert_eq!(out.sql, "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)");
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "a");
        assert_eq!(out.params[1].name, "b");
        assert_eq!(out.params[2].name, "c");
    }

    #[test]
    fn repeated_param_is_deduplicated() {
        let out = lex("SELECT * FROM t WHERE a = $x AND b = $x").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE a = $1 AND b = $1");
        // params has only one entry for "x"
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "x");
    }

    #[test]
    fn repeated_param_mixed_with_others() {
        let out = lex("SELECT $a, $b, $a, $c, $b").unwrap();
        assert_eq!(out.sql, "SELECT $1, $2, $1, $3, $2");
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "a");
        assert_eq!(out.params[1].name, "b");
        assert_eq!(out.params[2].name, "c");
    }

    #[test]
    fn param_in_string_literal_ignored() {
        let out = lex("SELECT '$falso' FROM t WHERE id = $id").unwrap();
        assert_eq!(out.sql, "SELECT '$falso' FROM t WHERE id = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_line_comment_ignored() {
        let out = lex("SELECT 1 -- $comentario\nWHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_block_comment_ignored() {
        let out = lex("SELECT /* $comentario */ 1 WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_dollar_quote_ignored() {
        let out = lex("SELECT $$body $falso$$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_tagged_dollar_quote_ignored() {
        let out = lex("SELECT $fn$body $falso$fn$ WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn param_in_quoted_identifier_ignored() {
        let out = lex("SELECT \"$coluna\" FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
    }

    #[test]
    fn string_with_escape() {
        let out = lex("SELECT 'it''s $falso' FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
        assert!(out.sql.contains("'it''s $falso'"));
    }

    #[test]
    fn spread_without_fields() {
        let out = lex("INSERT INTO t VALUES $..items").unwrap();
        assert!(out.params.is_empty());
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        assert_eq!(out.spreads[0].fields, None);
        // Spread is removed from output SQL
        assert_eq!(out.sql, "INSERT INTO t VALUES ");
        assert_eq!(out.spreads[0].offset, out.sql.len());
    }

    #[test]
    fn spread_with_fields() {
        let out = lex("INSERT INTO t VALUES $..items { name, email, age }").unwrap();
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        assert_eq!(
            out.spreads[0].fields,
            Some(vec!["name".into(), "email".into(), "age".into()])
        );
        assert_eq!(out.sql, "INSERT INTO t VALUES ");
    }

    #[test]
    fn spread_offset_after_param() {
        let out = lex("INSERT INTO t (org) VALUES ($org), $..items { name }").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "org");
        assert_eq!(out.spreads.len(), 1);
        assert_eq!(out.spreads[0].name, "items");
        // $org becomes $1, then spread is removed
        assert_eq!(out.sql, "INSERT INTO t (org) VALUES ($1), ");
        assert_eq!(out.spreads[0].offset, out.sql.len());
    }

    #[test]
    fn multiple_spreads() {
        let out = lex("INSERT INTO t VALUES $..a { x }, $..b { y }").unwrap();
        assert_eq!(out.spreads.len(), 2);
        assert_eq!(out.spreads[0].name, "a");
        assert_eq!(out.spreads[1].name, "b");
        // Both spreads removed, offsets mark insertion points
        assert_eq!(out.sql, "INSERT INTO t VALUES , ");
        assert!(out.spreads[0].offset < out.spreads[1].offset);
    }

    #[test]
    fn native_pg_placeholder_preserved() {
        let out = lex("SELECT * FROM t WHERE id = $1 AND name = $2").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE id = $1 AND name = $2");
        assert!(out.params.is_empty());
    }

    #[test]
    fn unclosed_string_error() {
        let err = lex("SELECT 'unclosed").unwrap_err();
        assert!(matches!(err, LexError::UnclosedString { .. }));
    }

    #[test]
    fn unclosed_block_comment_error() {
        let err = lex("SELECT /* unclosed").unwrap_err();
        assert!(matches!(err, LexError::UnclosedBlockComment { .. }));
    }

    #[test]
    fn nullable_param() {
        let out = lex("SELECT * FROM t WHERE age = $age?").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE age = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "age");
        assert_eq!(out.params[0].nullable, Some(true));
    }

    #[test]
    fn force_not_null_param() {
        let out = lex("SELECT * FROM t WHERE age = $age!").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE age = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "age");
        assert_eq!(out.params[0].nullable, Some(false));
    }

    #[test]
    fn non_nullable_param_default() {
        let out = lex("SELECT * FROM t WHERE id = $id").unwrap();
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "id");
        assert_eq!(out.params[0].nullable, None);
    }

    #[test]
    fn mixed_nullable_params() {
        let out = lex("SELECT * FROM t WHERE id = $id AND age = $age? AND name = $name").unwrap();
        assert_eq!(
            out.sql,
            "SELECT * FROM t WHERE id = $1 AND age = $2 AND name = $3"
        );
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].name, "id");
        assert_eq!(out.params[0].nullable, None);
        assert_eq!(out.params[1].name, "age");
        assert_eq!(out.params[1].nullable, Some(true));
        assert_eq!(out.params[2].name, "name");
        assert_eq!(out.params[2].nullable, None);
    }

    #[test]
    fn mixed_all_three_annotations() {
        let out = lex("SELECT * FROM t WHERE a = $a AND b = $b? AND c = $c!").unwrap();
        assert_eq!(
            out.sql,
            "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $3"
        );
        assert_eq!(out.params.len(), 3);
        assert_eq!(out.params[0].nullable, None); // $a — auto
        assert_eq!(out.params[1].nullable, Some(true)); // $b? — force nullable
        assert_eq!(out.params[2].nullable, Some(false)); // $c! — force non-null
    }

    #[test]
    fn nullable_param_deduplicated() {
        let out = lex("SELECT * FROM t WHERE a = $x? OR b = $x?").unwrap();
        assert_eq!(out.sql, "SELECT * FROM t WHERE a = $1 OR b = $1");
        assert_eq!(out.params.len(), 1);
        assert_eq!(out.params[0].name, "x");
        assert_eq!(out.params[0].nullable, Some(true));
    }

    #[test]
    fn spread_with_nullable_fields() {
        let out = lex("INSERT INTO t VALUES $..items { name, email?, age }").unwrap();
        assert_eq!(out.spreads.len(), 1);
        let fields = out.spreads[0].fields.as_ref().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].name, "name");
        assert!(!fields[0].nullable);
        assert_eq!(fields[1].name, "email");
        assert!(fields[1].nullable);
        assert_eq!(fields[2].name, "age");
        assert!(!fields[2].nullable);
    }

    #[test]
    fn spread_all_nullable_fields() {
        let out = lex("INSERT INTO t VALUES $..items { a?, b? }").unwrap();
        let fields = out.spreads[0].fields.as_ref().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields[0].nullable);
        assert!(fields[1].nullable);
    }
}

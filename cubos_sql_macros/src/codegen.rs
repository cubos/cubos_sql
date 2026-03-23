//! Code generation for the `query!` macro.
//!
//! Receives the lexer output ([`cubos_sql_core::param::LexOutput`]) and the
//! introspection result ([`cubos_sql_core::query_info::QueryInfo`]) and produces a
//! [`proc_macro2::TokenStream`] that implements the typed query builder.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse_str;

use cubos_sql_core::param::LexOutput;
use cubos_sql_core::query_info::{ColumnInfo, QueryInfo};

/// Rust keywords that cannot be used as identifiers without the `r#` prefix.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "try", "typeof", "unsized", "virtual",
];

/// Sanitize a SQL column name into a valid Rust identifier.
///
/// Replaces non-alphanumeric characters with `_`, ensures the name starts with
/// a letter or `_`, and escapes Rust keywords with `r#`.
fn make_field_ident(name: &str) -> proc_macro2::Ident {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let sanitized = if sanitized.is_empty() {
        "_unnamed".to_string()
    } else if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{sanitized}")
    } else {
        sanitized
    };

    if RUST_KEYWORDS.contains(&sanitized.as_str()) {
        proc_macro2::Ident::new_raw(&sanitized, proc_macro2::Span::call_site())
    } else {
        format_ident!("{}", sanitized)
    }
}

/// Qualify external crate types so the generated code references them through
/// `cubos_sql::__private::` instead of requiring the consumer to declare them
/// as direct dependencies.
fn qualify_rust_type(rust_type: &str) -> String {
    rust_type
        .replace("chrono::", "::cubos_sql::__private::chrono::")
        .replace("uuid::", "::cubos_sql::__private::uuid::")
        .replace("rust_decimal::", "::cubos_sql::__private::rust_decimal::")
        .replace("serde_json::", "::cubos_sql::__private::serde_json::")
}

/// If the SQL is a SELECT-like query, wrap it in a subquery with `LIMIT 1`
/// so that `fetch_one` / `fetch_optional` don't fetch all rows.
///
/// Returns `None` for DML (INSERT/UPDATE/DELETE) where subquery wrapping is
/// not valid SQL.
fn wrap_with_limit(sql: &str) -> Option<String> {
    let upper = sql.trim_start().to_uppercase();
    if upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("VALUES")
        || upper.starts_with("TABLE")
    {
        Some(format!(
            "SELECT * FROM ({sql}) AS __cubos_sql_limit LIMIT 1"
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A named parameter assignment from the macro invocation.
///
/// Examples:
/// - `min_age = 25`  → `ParamAssignment { name: "min_age", expr: Some(25) }`
/// - `min_age`       → `ParamAssignment { name: "min_age", expr: None }` (scope capture)
pub struct ParamAssignment {
    /// Parameter name — must match a `$name` token found in the SQL.
    pub name: String,
    /// The value expression. `None` means the variable with the same name is
    /// captured from the enclosing scope.
    pub expr: Option<syn::Expr>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Generate the `TokenStream` for a `query!` invocation.
///
/// # Arguments
///
/// * `lex_output`    – Result of lexing the SQL template. Contains the
///   rewritten SQL (with `$1`, `$2`, … placeholders) and
///   the ordered list of named parameters.
/// * `query_info`    – Introspection result: parameter types and output column
///   metadata.
/// * `executor_expr` – The executor expression passed as the first argument to
///   the macro (e.g. `pool` or `&tx`).
/// * `assignments`   – Named parameter assignments from the macro invocation.
///   Order does not need to match the SQL order; lookup is
///   done by name.
///
/// # Errors
///
/// Returns a [`syn::Error`] when a parameter referenced in the SQL has no
/// matching assignment and cannot be resolved.
pub fn generate(
    lex_output: &LexOutput,
    query_info: &QueryInfo,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    if lex_output.spreads.is_empty() {
        generate_regular(lex_output, query_info, executor_expr, assignments)
    } else {
        generate_spread(lex_output, query_info, executor_expr, assignments)
    }
}

/// Generate code for a regular query (no spreads).
fn generate_regular(
    lex_output: &LexOutput,
    query_info: &QueryInfo,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    // ------------------------------------------------------------------
    // 1. Build the output struct definition (always generated, even when
    //    there are no columns — an empty struct is valid and consistent).
    // ------------------------------------------------------------------
    let output_struct = build_output_struct(&query_info.columns)?;

    // ------------------------------------------------------------------
    // 2. Build the query struct fields — one per SQL parameter.
    // ------------------------------------------------------------------
    let (param_field_defs, param_field_inits) =
        build_param_fields(lex_output, query_info, assignments)?;

    // ------------------------------------------------------------------
    // 3. Build the `&[&(dyn ToSql + Sync)]` slice literal used in every
    //    method body.
    // ------------------------------------------------------------------
    let params_slice = build_params_slice(lex_output);

    // ------------------------------------------------------------------
    // 4. Row-mapping expression used in fetch methods.
    // ------------------------------------------------------------------
    let row_mapping = build_row_mapping(&query_info.columns)?;

    // ------------------------------------------------------------------
    // 5. The SQL string literal (+ limited variant for fetch_one/optional).
    // ------------------------------------------------------------------
    let sql_str = &lex_output.sql;
    let sql_limited = wrap_with_limit(sql_str).unwrap_or_else(|| sql_str.clone());

    // ------------------------------------------------------------------
    // 6. Assemble the full block.
    // ------------------------------------------------------------------
    let ts = quote! {
        {
            // ----- output type -----
            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __cubos_sql_output {
                #output_struct
            }

            // ----- query builder struct -----
            #[allow(non_camel_case_types)]
            struct __cubos_sql_query<'__e, __E: cubos_sql::Executor> {
                __executor: &'__e __E,
                #param_field_defs
            }

            // ----- method implementations -----
            impl<'__e, __E: cubos_sql::Executor> __cubos_sql_query<'__e, __E> {
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__cubos_sql_output>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__cubos_sql_output {
                            #row_mapping
                        })
                    }).collect()
                }

                async fn fetch_one(self) -> ::std::result::Result<__cubos_sql_output, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let __row = __rows.into_iter().next()
                        .ok_or_else(|| cubos_sql::Error::NoRows)?;
                    ::std::result::Result::Ok(__cubos_sql_output {
                        #row_mapping
                    })
                }

                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__cubos_sql_output>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    match __rows.into_iter().next() {
                        Some(__row) => ::std::result::Result::Ok(Some(__cubos_sql_output {
                            #row_mapping
                        })),
                        None => ::std::result::Result::Ok(None),
                    }
                }

                async fn execute(self) -> ::std::result::Result<u64, cubos_sql::Error> {
                    cubos_sql::Executor::execute(
                        self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await
                }
            }

            // ----- construct and return the query builder -----
            __cubos_sql_query {
                __executor: &#executor_expr,
                #param_field_inits
            }
        }
    };

    Ok(ts)
}

// ---------------------------------------------------------------------------
// Spread query code generation
// ---------------------------------------------------------------------------

/// Generate code for a query with one or more `$..spread` bulk inserts.
///
/// Supports both struct mode (`$..items { a, b }` → `.a`, `.b` access)
/// and tuple mode (`$..items` → `.0`, `.1` access).
///
/// Since the number of items is unknown at compile time, the generated code
/// builds the SQL string dynamically at runtime, expanding placeholders for
/// each spread based on the slice length.
fn generate_spread(
    lex_output: &LexOutput,
    query_info: &QueryInfo,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let output_struct = build_output_struct(&query_info.columns)?;
    let row_mapping = build_row_mapping(&query_info.columns)?;
    let num_regular_params = lex_output.params.len();
    let num_spreads = lex_output.spreads.len();

    // ── Regular param fields ────────────────────────────────────────────
    let mut regular_param_fields = TokenStream::new();
    let mut regular_param_inits = TokenStream::new();
    let mut regular_param_pushes = TokenStream::new();
    for (idx, param) in lex_output.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);
        let param_info = query_info.params.get(idx);
        let field_type: syn::Type = if let Some(pi) = param_info {
            if pi.domain_rust_type.is_some() {
                parse_str("::cubos_sql::__private::serde_json::Value")?
            } else {
                parse_str::<syn::Type>(&qualify_rust_type(&pi.rust_type))?
            }
        } else {
            parse_str("::cubos_sql::__private::serde_json::Value")?
        };
        let value_expr = resolve_param_value(
            &param.name,
            param_info.and_then(|p| p.domain_rust_type.as_deref()),
            param_info.and_then(|p| p.enum_rust_type.as_deref()),
            assignments,
        )?;
        regular_param_fields.extend(quote! { #field_name: #field_type, });
        regular_param_inits.extend(quote! { #field_name: #value_expr, });
        regular_param_pushes.extend(quote! {
            __params.push(Box::new(self.#field_name.clone())
                as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
        });
    }

    // ── Per-spread: generics, fields, inits, push exprs, SQL pieces ────
    let mut spread_generic_types = Vec::new();
    let mut spread_struct_fields = TokenStream::new();
    let mut spread_struct_inits = TokenStream::new();
    let mut spread_empty_checks = TokenStream::new();
    let mut spread_param_pushes = TokenStream::new();
    let mut spread_size_args = TokenStream::new();
    let mut spread_size_params = TokenStream::new();

    // SQL pieces: the text between spread offsets
    let mut sql_pieces: Vec<&str> = Vec::new();
    let mut fields_per_row_lits = Vec::new();
    let mut last_offset = 0;

    // Track which introspected param index we're at for spread columns
    let mut spread_param_offset = num_regular_params;

    for (si, spread) in lex_output.spreads.iter().enumerate() {
        let fields = spread.fields.as_ref().expect("spread must have fields");
        let col_count = fields.len();
        let type_ident = format_ident!("__S{}", si);
        let field_ident = format_ident!("__spread_{}", si);
        let size_ident = format_ident!("__size_{}", si);

        spread_generic_types.push(type_ident.clone());

        // SQL piece before this spread
        sql_pieces.push(&lex_output.sql[last_offset..spread.offset]);
        last_offset = spread.offset;
        fields_per_row_lits.push(proc_macro2::Literal::usize_unsuffixed(col_count));

        // Struct field + init (all spreads share the '__s lifetime)
        spread_struct_fields.extend(quote! {
            #field_ident: &'__s [#type_ident],
        });

        // Resolve spread value expression
        let spread_value_expr: TokenStream = {
            let assignment = assignments.iter().find(|a| a.name == spread.name);
            match assignment {
                Some(ParamAssignment { expr: Some(e), .. }) => quote! { #e },
                _ => {
                    let ident = format_ident!("{}", spread.name);
                    quote! { #ident }
                }
            }
        };
        spread_struct_inits.extend(quote! {
            #field_ident: &(#spread_value_expr)[..],
        });

        // Empty check
        spread_empty_checks.extend(quote! {
            if self.#field_ident.is_empty() { __any_empty = true; }
        });

        // Size args for __build_spread_sql call
        spread_size_args.extend(quote! { self.#field_ident.len(), });
        spread_size_params.extend(quote! { #size_ident: usize, });

        // Param push expressions: iterate spread items and push field values
        let mut item_pushes = TokenStream::new();
        for ci in 0..col_count {
            let param_idx = spread_param_offset + ci;
            let param_info = query_info.params.get(param_idx);
            let is_domain = param_info
                .map(|pi| pi.domain_rust_type.is_some())
                .unwrap_or(false);

            let accessor_ident = format_ident!("{}", fields[ci].name);
            let accessor: TokenStream = quote! { __item.#accessor_ident };

            if is_domain {
                item_pushes.extend(quote! {
                    __params.push(
                        Box::new(::cubos_sql::__private::serde_json::to_value(&#accessor)
                            .map_err(|e| cubos_sql::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}")))?)
                            as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>
                    );
                });
            } else {
                item_pushes.extend(quote! {
                    __params.push(
                        Box::new(#accessor.clone())
                            as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>
                    );
                });
            }
        }

        spread_param_pushes.extend(quote! {
            for __item in self.#field_ident.iter() {
                #item_pushes
            }
        });

        spread_param_offset += col_count;
    }

    // Final SQL piece (after last spread)
    sql_pieces.push(&lex_output.sql[last_offset..]);

    // ── Generate the __build_spread_sql function body ────────────────────
    // It takes one size arg per spread and builds the SQL dynamically.
    let num_regular_lit = proc_macro2::Literal::usize_unsuffixed(num_regular_params);
    let mut sql_builder_body = TokenStream::new();
    sql_builder_body.extend(quote! {
        let mut __sql = String::new();
        let mut __p: usize = #num_regular_lit + 1;
    });

    for si in 0..num_spreads {
        let piece = sql_pieces[si];
        let fpr = &fields_per_row_lits[si];
        let size_ident = format_ident!("__size_{}", si);
        sql_builder_body.extend(quote! {
            __sql.push_str(#piece);
            for __r in 0..#size_ident {
                if __r > 0 { __sql.push_str(", "); }
                __sql.push('(');
                for __c in 0..#fpr {
                    if __c > 0 { __sql.push_str(", "); }
                    __sql.push('$');
                    __sql.push_str(&__p.to_string());
                    __p += 1;
                }
                __sql.push(')');
            }
        });
    }
    // Final piece
    let final_piece = sql_pieces[num_spreads];
    sql_builder_body.extend(quote! {
        __sql.push_str(#final_piece);
        __sql
    });

    // ── Capacity estimate ───────────────────────────────────────────────
    let mut capacity_expr = quote! { #num_regular_lit };
    for si in 0..num_spreads {
        let field_ident = format_ident!("__spread_{}", si);
        let fpr = &fields_per_row_lits[si];
        capacity_expr.extend(quote! { + self.#field_ident.len() * #fpr });
    }

    // ── Method body (shared logic) ──────────────────────────────────────
    // Generate a macro-like token block for the common query execution preamble
    let query_preamble = quote! {
        let mut __any_empty = false;
        #spread_empty_checks
        let __sql = __build_spread_sql(#spread_size_args);
        let mut __params: Vec<Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>>
            = Vec::with_capacity(#capacity_expr);
        #regular_param_pushes
        #spread_param_pushes
        let __params_ref: Vec<&(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync)>
            = __params.iter().map(|p| p.as_ref()).collect();
    };

    // ── Assemble the full block ─────────────────────────────────────────
    let ts = quote! {
        {
            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __cubos_sql_output {
                #output_struct
            }

            #[allow(non_camel_case_types)]
            struct __cubos_sql_query<'__e, '__s, __E: cubos_sql::Executor, #(#spread_generic_types,)*> {
                __executor: &'__e __E,
                #spread_struct_fields
                #regular_param_fields
            }

            fn __build_spread_sql(#spread_size_params) -> String {
                #sql_builder_body
            }

            impl<'__e, '__s, __E: cubos_sql::Executor, #(#spread_generic_types,)*>
                __cubos_sql_query<'__e, '__s, __E, #(#spread_generic_types,)*>
            {
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__cubos_sql_output>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let __rows = cubos_sql::Executor::query(self.__executor, &__sql, &__params_ref).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__cubos_sql_output { #row_mapping })
                    }).collect()
                }

                async fn fetch_one(self) -> ::std::result::Result<__cubos_sql_output, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Err(cubos_sql::Error::NoRows);
                    }
                    let __rows = cubos_sql::Executor::query(self.__executor, &__sql, &__params_ref).await?;
                    let __row = __rows.into_iter().next().ok_or_else(|| cubos_sql::Error::NoRows)?;
                    ::std::result::Result::Ok(__cubos_sql_output { #row_mapping })
                }

                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__cubos_sql_output>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::option::Option::None);
                    }
                    let __rows = cubos_sql::Executor::query(self.__executor, &__sql, &__params_ref).await?;
                    match __rows.into_iter().next() {
                        Some(__row) => ::std::result::Result::Ok(Some(__cubos_sql_output { #row_mapping })),
                        None => ::std::result::Result::Ok(None),
                    }
                }

                async fn execute(self) -> ::std::result::Result<u64, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(0);
                    }
                    cubos_sql::Executor::execute(self.__executor, &__sql, &__params_ref).await
                }
            }

            __cubos_sql_query {
                __executor: &#executor_expr,
                #spread_struct_inits
                #regular_param_inits
            }
        }
    };

    Ok(ts)
}

// ---------------------------------------------------------------------------
// Helper: output struct fields
// ---------------------------------------------------------------------------

/// Generates the field list for `__cubos_sql_output`.
///
/// For example:
/// ```text
/// pub id: i64,
/// pub name: String,
/// pub tag: Option<String>,
/// ```
fn build_output_struct(columns: &[ColumnInfo]) -> Result<TokenStream, syn::Error> {
    let mut fields = TokenStream::new();

    for col in columns {
        let field_name = make_field_ident(&col.name);
        let field_type = column_rust_type(col)?;

        fields.extend(quote! {
            pub #field_name: #field_type,
        });
    }

    Ok(fields)
}

// ---------------------------------------------------------------------------
// Helper: query struct param fields + initializer
// ---------------------------------------------------------------------------

/// Returns `(field_definitions, field_initializers)` for the query struct.
///
/// Field definitions:
/// ```text
/// p0: i32,
/// p1: String,
/// ```
///
/// Field initializers (used in the struct literal at the end of the block):
/// ```text
/// p0: 25,
/// p1: some_var,
/// ```
fn build_param_fields(
    lex_output: &LexOutput,
    query_info: &QueryInfo,
    assignments: &[ParamAssignment],
) -> Result<(TokenStream, TokenStream), syn::Error> {
    let mut defs = TokenStream::new();
    let mut inits = TokenStream::new();

    for (idx, param) in lex_output.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);

        // Resolve the Rust type for this parameter from introspection.
        let param_info = query_info.params.get(idx);
        let is_nullable = param_info.map(|pi| pi.nullable).unwrap_or(false);
        let inner_type_str = if let Some(pi) = param_info {
            // Domain types are serialized to serde_json::Value before sending.
            if pi.domain_rust_type.is_some() {
                "::cubos_sql::__private::serde_json::Value".to_string()
            } else {
                qualify_rust_type(&pi.rust_type)
            }
        } else {
            "::cubos_sql::__private::serde_json::Value".to_string()
        };

        let field_type: syn::Type = if is_nullable {
            parse_str(&format!("::std::option::Option<{inner_type_str}>"))?
        } else {
            parse_str(&inner_type_str)?
        };

        // Resolve the value expression for this parameter.
        let value_expr: TokenStream = resolve_param_value(
            &param.name,
            param_info.and_then(|p| p.domain_rust_type.as_deref()),
            param_info.and_then(|p| p.enum_rust_type.as_deref()),
            assignments,
        )?;

        defs.extend(quote! {
            #field_name: #field_type,
        });

        inits.extend(quote! {
            #field_name: #value_expr,
        });
    }

    Ok((defs, inits))
}

/// Produces the value expression that will be stored in the query struct field.
///
/// If `domain_rust_type` is `Some`, the user-supplied expression is wrapped
/// with `serde_json::to_value(...)` so it is stored as a JSON value.
fn resolve_param_value(
    param_name: &str,
    domain_rust_type: Option<&str>,
    enum_rust_type: Option<&str>,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let assignment = assignments.iter().find(|a| a.name == param_name);

    let raw_expr: TokenStream = match assignment {
        Some(ParamAssignment { expr: Some(e), .. }) => quote! { #e },
        Some(ParamAssignment { expr: None, .. }) | None => {
            let ident = format_ident!("{}", param_name);
            quote! { #ident }
        }
    };

    if domain_rust_type.is_some() {
        Ok(quote! {
            ::cubos_sql::__private::serde_json::to_value(&#raw_expr)
                .map_err(|e| cubos_sql::Error::Serialize(format!("failed to serialize domain type to JSON: {e}")))?
        })
    } else if enum_rust_type.is_some() {
        // Enum params: convert to String via ToString
        Ok(quote! { (#raw_expr).to_string() })
    } else {
        Ok(raw_expr)
    }
}

// ---------------------------------------------------------------------------
// Helper: `&[&(dyn ToSql + Sync)]` slice
// ---------------------------------------------------------------------------

/// Builds the comma-separated list of `&self.pN as &(dyn ToSql + Sync)` used
/// inside the `&[...]` slice literal passed to `Executor::query` /
/// `Executor::execute`.
fn build_params_slice(lex_output: &LexOutput) -> TokenStream {
    let mut elems = TokenStream::new();

    for idx in 0..lex_output.params.len() {
        let field_name = format_ident!("p{}", idx);
        elems.extend(quote! {
            &self.#field_name as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
        });
    }

    elems
}

// ---------------------------------------------------------------------------
// Helper: row mapping inside `map(|__row| { ... })`
// ---------------------------------------------------------------------------

/// Produces the field assignments for `__cubos_sql_output { ... }` from a
/// `tokio_postgres::Row`.
fn build_row_mapping(columns: &[ColumnInfo]) -> Result<TokenStream, syn::Error> {
    let mut mappings = TokenStream::new();

    for (idx, col) in columns.iter().enumerate() {
        let field_name = make_field_ident(&col.name);
        let get_expr = column_get_expr(col, idx)?;

        mappings.extend(quote! {
            #field_name: #get_expr,
        });
    }

    Ok(mappings)
}

// ---------------------------------------------------------------------------
// Type helpers
// ---------------------------------------------------------------------------

/// Returns the Rust type token for a column field in `__cubos_sql_output`.
///
/// Domain types override the base type. Nullable columns are wrapped in
/// `Option<T>`.
fn column_rust_type(col: &ColumnInfo) -> Result<syn::Type, syn::Error> {
    // The concrete inner type: domain type takes priority over the base type.
    // Domain/enum types are user-provided paths; base types need qualification.
    let inner_type_str = match col.domain_rust_type.as_deref() {
        Some(domain) => domain.to_string(),
        None => qualify_rust_type(&col.rust_type),
    };

    let inner_type: syn::Type = parse_str(&inner_type_str)?;

    if col.nullable {
        // Wrap in Option<T>.
        Ok(parse_str(&format!(
            "::std::option::Option<{}>",
            inner_type_str
        ))?)
    } else {
        Ok(inner_type)
    }
}

/// Returns the expression used to extract a column value from a
/// `tokio_postgres::Row` (bound to the name `__row` in the generated code).
///
/// Three cases:
/// 1. **Domain (JSONB) column** – read as `serde_json::Value` then deserialize.
/// 2. **Nullable plain column** – `__row.get::<_, Option<T>>(idx)`.
/// 3. **Non-nullable plain column** – `__row.get::<_, T>(idx)`.
fn column_get_expr(col: &ColumnInfo, idx: usize) -> Result<TokenStream, syn::Error> {
    let idx_lit = proc_macro2::Literal::usize_unsuffixed(idx);

    if let Some(domain_type_str) = &col.domain_rust_type {
        let domain_type: syn::Type = parse_str(domain_type_str)?;

        if col.nullable {
            Ok(quote! {
                {
                    let __json_val = __row.get::<_, ::std::option::Option<::cubos_sql::__private::serde_json::Value>>(#idx_lit);
                    match __json_val {
                        Some(__v) => Some(::cubos_sql::__private::serde_json::from_value::<#domain_type>(__v)
                            .map_err(|e| cubos_sql::Error::Deserialize(
                                format!("failed to deserialize {}: {e}", stringify!(#domain_type))))?),
                        None => None,
                    }
                }
            })
        } else {
            Ok(quote! {
                ::cubos_sql::__private::serde_json::from_value::<#domain_type>(
                    __row.get::<_, ::cubos_sql::__private::serde_json::Value>(#idx_lit)
                ).map_err(|e| cubos_sql::Error::Deserialize(
                    format!("failed to deserialize {}: {e}", stringify!(#domain_type))))?
            })
        }
    } else if let Some(enum_type_str) = &col.enum_rust_type {
        // Enum column: read as String from PG, parse into the mapped Rust type.
        let enum_type: syn::Type = parse_str(enum_type_str)?;

        if col.nullable {
            Ok(quote! {
                {
                    let __str_val = __row.get::<_, ::std::option::Option<String>>(#idx_lit);
                    match __str_val {
                        Some(__v) => Some(__v.parse::<#enum_type>()
                            .map_err(|e| cubos_sql::Error::Deserialize(
                                format!("failed to parse enum {}: {e}", stringify!(#enum_type))))?),
                        None => None,
                    }
                }
            })
        } else {
            Ok(quote! {
                {
                    let __str_val = __row.get::<_, String>(#idx_lit);
                    __str_val.parse::<#enum_type>()
                        .map_err(|e| cubos_sql::Error::Deserialize(
                            format!("failed to parse enum {}: {e}", stringify!(#enum_type))))?
                }
            })
        }
    } else {
        // Plain column.
        let base_type: syn::Type = parse_str(&qualify_rust_type(&col.rust_type))?;

        if col.nullable {
            Ok(quote! {
                __row.get::<_, ::std::option::Option<#base_type>>(#idx_lit)
            })
        } else {
            Ok(quote! {
                __row.get::<_, #base_type>(#idx_lit)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubos_sql_core::lexer::lex;
    use cubos_sql_core::query_info::{ColumnInfo, ParamInfo, QueryInfo};

    fn make_query_info(param_types: &[&str], columns: Vec<ColumnInfo>) -> QueryInfo {
        QueryInfo {
            params: param_types
                .iter()
                .map(|t| ParamInfo {
                    pg_type_oid: 0,
                    rust_type: t.to_string(),
                    nullable: false,
                    domain_rust_type: None,
                    enum_rust_type: None,
                })
                .collect(),
            columns,
        }
    }

    fn make_column(name: &str, rust_type: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            pg_type_oid: 0,
            rust_type: rust_type.to_string(),
            nullable,
            domain_rust_type: None,
            enum_rust_type: None,
        }
    }

    fn parse_executor_expr() -> syn::Expr {
        syn::parse_str::<syn::Expr>("pool").unwrap()
    }

    #[test]
    fn regular_query_no_spread_code() {
        let lo = lex("SELECT id, name FROM users WHERE age > $min_age").unwrap();
        let qi = make_query_info(
            &["i32"],
            vec![
                make_column("id", "i64", false),
                make_column("name", "String", false),
            ],
        );
        let executor = parse_executor_expr();
        let assignments = vec![ParamAssignment {
            name: "min_age".into(),
            expr: Some(syn::parse_str("25").unwrap()),
        }];

        let ts = generate(&lo, &qi, &executor, &assignments).unwrap();
        let code = ts.to_string();

        assert!(code.contains("fetch_all"), "should have fetch_all method");
        assert!(code.contains("fetch_one"), "should have fetch_one method");
        assert!(code.contains("execute"), "should have execute method");
        assert!(
            !code.contains("__spread"),
            "regular query should not have spread code"
        );
        assert!(
            !code.contains("__build_spread_sql"),
            "regular query should not have spread SQL builder"
        );
    }

    #[test]
    fn spread_generates_dynamic_sql_builder() {
        let lo =
            lex("INSERT INTO users (name, email) VALUES $..new_users { name, email }").unwrap();
        assert_eq!(lo.spreads.len(), 1);
        assert_eq!(lo.spreads[0].name, "new_users");

        let qi = make_query_info(&["String", "String"], vec![]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            code.contains("__build_spread_sql"),
            "spread query should have SQL builder fn"
        );
        assert!(
            code.contains("__spread"),
            "spread query should have __spread field"
        );
        assert!(code.contains("is_empty"), "should handle empty spread case");
    }

    #[test]
    fn spread_has_all_four_methods() {
        let lo = lex("INSERT INTO t (x) VALUES $..data { x }").unwrap();
        let qi = make_query_info(&["i32"], vec![]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(code.contains("fetch_all"), "should have fetch_all");
        assert!(code.contains("fetch_one"), "should have fetch_one");
        assert!(
            code.contains("fetch_optional"),
            "should have fetch_optional"
        );
        assert!(code.contains("execute"), "should have execute");
    }

    #[test]
    fn spread_with_returning_has_output_struct() {
        let lo =
            lex("INSERT INTO users (name, email) VALUES $..users { name, email } RETURNING id")
                .unwrap();
        let qi = make_query_info(&["String", "String"], vec![make_column("id", "i64", false)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        // Output struct should have the id field from RETURNING.
        assert!(
            code.contains("__cubos_sql_output"),
            "should define output struct"
        );
        assert!(
            code.contains("__row"),
            "should have row mapping for RETURNING columns"
        );
    }

    #[test]
    fn spread_with_regular_params() {
        let lo = lex(
            "INSERT INTO items (org_id, name) SELECT $org_id, name FROM (VALUES $..items { name }) AS t(name)",
        )
        .unwrap();
        assert_eq!(lo.params.len(), 1);
        assert_eq!(lo.spreads.len(), 1);

        let qi = make_query_info(&["i64", "String"], vec![]);
        let executor = parse_executor_expr();
        let assignments = vec![ParamAssignment {
            name: "org_id".into(),
            expr: Some(syn::parse_str("42i64").unwrap()),
        }];

        let ts = generate(&lo, &qi, &executor, &assignments).unwrap();
        let code = ts.to_string();

        // Should have both regular param field and spread.
        assert!(code.contains("p0"), "should have regular param p0");
        assert!(code.contains("__spread"), "should have spread");
    }

    #[test]
    fn spread_sql_prefix_and_suffix() {
        let lo =
            lex("INSERT INTO users (name, email) VALUES $..users { name, email } RETURNING id")
                .unwrap();
        let qi = make_query_info(&["String", "String"], vec![make_column("id", "i64", false)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        // The SQL prefix should include everything before the spread offset.
        assert!(
            code.contains("INSERT INTO users (name, email) VALUES"),
            "SQL prefix should be in generated code"
        );
        // The suffix should include RETURNING.
        assert!(
            code.contains("RETURNING id"),
            "SQL suffix should be in generated code"
        );
    }

    #[test]
    fn spread_empty_fetch_all_returns_empty_vec() {
        let lo = lex("INSERT INTO t (x) VALUES $..data { x }").unwrap();
        let qi = make_query_info(&["i32"], vec![]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        // The generated code should check is_empty and return early with
        // an empty Vec for fetch_all.
        assert!(
            code.contains("Vec :: new"),
            "fetch_all empty should return empty vec"
        );
    }

    #[test]
    fn multiple_spreads_generates_multiple_fields() {
        let lo = lex("WITH a AS (INSERT INTO t1 (x) VALUES $..s1 { x }) \
             INSERT INTO t2 (y) VALUES $..s2 { y }")
        .unwrap();
        assert_eq!(lo.spreads.len(), 2);

        let qi = make_query_info(&["i32", "String"], vec![]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(code.contains("__spread_0"), "should have __spread_0");
        assert!(code.contains("__spread_1"), "should have __spread_1");
        assert!(code.contains("__S0"), "should have generic __S0");
        assert!(code.contains("__S1"), "should have generic __S1");
        assert!(
            code.contains("__build_spread_sql"),
            "should have SQL builder"
        );
    }
}

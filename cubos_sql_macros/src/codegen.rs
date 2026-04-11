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

/// If the SQL is a SELECT-like query, wrap it in a subquery with `LIMIT 2`
/// so that `fetch_one` / `fetch_optional` can detect more-than-one-row without
/// fetching the entire result set.
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
            "SELECT * FROM ({sql}) AS __cubos_sql_limit LIMIT 2"
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
    let params_slice = build_params_slice(lex_output, query_info)?;

    // ------------------------------------------------------------------
    // 4. Row-mapping expression used in fetch methods.
    // ------------------------------------------------------------------
    let row_mapping = build_row_mapping(&query_info.columns)?;

    // ------------------------------------------------------------------
    // 5. The SQL string literal (+ limited variant for fetch_one/optional).
    //    Domain params get `::jsonb` cast, enum params get `::text` cast,
    //    so tokio-postgres sends the base type OID that PG can coerce.
    // ------------------------------------------------------------------
    let sql_str = cast_params(lex_output, query_info);
    let sql_limited = wrap_with_limit(&sql_str).unwrap_or_else(|| sql_str.clone());

    // ------------------------------------------------------------------
    // 6. fetch_value() — only when there is exactly one column.
    // ------------------------------------------------------------------
    let fetch_value_method = build_fetch_value_method(&query_info.columns)?;

    // ------------------------------------------------------------------
    // 7. Assemble the full block.
    // ------------------------------------------------------------------
    let ts = quote! {
        {
            // ----- output type -----
            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __sql_output {
                #output_struct
            }

            // ----- query builder struct -----
            #[allow(non_camel_case_types)]
            struct __cubos_sql_query<__E: cubos_sql::Executor> {
                __executor: __E,
                #param_field_defs
            }

            // ----- method implementations -----
            impl<__E: cubos_sql::Executor> __cubos_sql_query<__E> {
                /// Execute the query and return all resulting rows.
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__sql_output>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__sql_output {
                            #row_mapping
                        })
                    }).collect()
                }

                /// Execute the query and return exactly one row.
                ///
                /// Returns [`Error::NoRows`] if the query returns no rows, or
                /// [`Error::TooManyRows`] if it returns more than one.
                async fn fetch_one(self) -> ::std::result::Result<__sql_output, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next()
                        .ok_or_else(|| cubos_sql::Error::NoRows)?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                    }
                    ::std::result::Result::Ok(__sql_output {
                        #row_mapping
                    })
                }

                /// Execute the query and return at most one row.
                ///
                /// Returns `Ok(None)` if the query returns no rows, or
                /// [`Error::TooManyRows`] if it returns more than one.
                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__sql_output>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                            }
                            ::std::result::Result::Ok(Some(__sql_output {
                                #row_mapping
                            }))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }

                /// Execute the statement and return the number of affected rows.
                ///
                /// Intended for `INSERT`, `UPDATE`, and `DELETE` statements.
                async fn execute(self) -> ::std::result::Result<u64, cubos_sql::Error> {
                    cubos_sql::Executor::execute(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await
                }

                #fetch_value_method

                /// Execute the query and return all resulting rows mapped to `T`.
                async fn fetch_all_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<::std::vec::Vec<__T>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    __rows.into_iter().map(|__row| {
                        __T::from_row(&__row)
                    }).collect()
                }

                /// Execute the query and return exactly one row mapped to `T`.
                async fn fetch_one_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<__T, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next()
                        .ok_or_else(|| cubos_sql::Error::NoRows)?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                    }
                    __T::from_row(&__row)
                }

                /// Execute the query and return at most one row mapped to `T`.
                async fn fetch_optional_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<::std::option::Option<__T>, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        &self.__executor,
                        #sql_limited,
                        &[#params_slice],
                    ).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                            }
                            ::std::result::Result::Ok(Some(__T::from_row(&__row)?))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }
            }

            // ----- construct and return the query builder -----
            __cubos_sql_query {
                __executor: #executor_expr,
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
        let pi = query_info.params.get(idx).ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!("could not infer type for parameter `${}`", param.name),
            )
        })?;
        let is_nullable = pi.nullable;
        let is_domain = pi.domain_rust_type.is_some();
        let is_enum = pi.enum_rust_type.is_some();

        // The field type exposed to the user: domain/enum types use their Rust
        // path directly; the internal serialization happens at push time.
        let inner_type_str = if let Some(domain) = &pi.domain_rust_type {
            domain.clone()
        } else if let Some(enum_type) = &pi.enum_rust_type {
            enum_type.clone()
        } else {
            pi.rust_type.clone()
        };
        let field_type: syn::Type = if is_nullable {
            parse_str(&format!("::std::option::Option<{inner_type_str}>"))?
        } else {
            parse_str(&inner_type_str)?
        };

        // Value expression: accept the user's value directly (no conversion).
        let value_expr = resolve_param_value(&param.name, assignments)?;
        // For String fields (non-domain, non-enum), wrap with `.into()`.
        let is_string_field = pi.rust_type == "String" && !is_domain && !is_enum;
        let value_expr = if is_string_field {
            if is_nullable {
                quote! { (#value_expr).map(Into::<String>::into) }
            } else {
                quote! { Into::<String>::into(#value_expr) }
            }
        } else {
            value_expr
        };
        regular_param_fields.extend(quote! { #field_name: #field_type, });
        // Type-annotated let binding using the original parameter name so that
        // any type mismatch error shows e.g. `let health: Option<HealthData> = ...`
        // instead of an opaque `p0` field.
        let param_ident = format_ident!("__{}", param.name);
        regular_param_inits.extend(quote! {
            #field_name: { let #param_ident: #field_type = #value_expr; #param_ident },
        });

        // Push: domain types are serialized to JSON, enum types to String.
        if is_domain {
            if is_nullable {
                regular_param_pushes.extend(quote! {
                    __params.push(Box::new(match &self.#field_name {
                        Some(__v) => Some(::serde_json::to_value(__v)
                            .map_err(|e| cubos_sql::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}")))?),
                        None => None,
                    }) as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
                });
            } else {
                regular_param_pushes.extend(quote! {
                    __params.push(Box::new(::serde_json::to_value(&self.#field_name)
                        .map_err(|e| cubos_sql::Error::Serialize(
                            format!("failed to serialize domain type to JSON: {e}")))?)
                        as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
                });
            }
        } else if is_enum {
            if is_nullable {
                regular_param_pushes.extend(quote! {
                    __params.push(Box::new(self.#field_name.as_ref().map(|__v| __v.to_string()))
                        as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
                });
            } else {
                regular_param_pushes.extend(quote! {
                    __params.push(Box::new(self.#field_name.to_string())
                        as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
                });
            }
        } else {
            regular_param_pushes.extend(quote! {
                __params.push(Box::new(self.#field_name.clone())
                    as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
            });
        }
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
        for (ci, field) in fields.iter().enumerate().take(col_count) {
            let param_idx = spread_param_offset + ci;
            let param_info = query_info.params.get(param_idx);
            let is_domain = param_info
                .map(|pi| pi.domain_rust_type.is_some())
                .unwrap_or(false);

            let accessor_ident = format_ident!("{}", field.name);
            let accessor: TokenStream = quote! { __item.#accessor_ident };

            if is_domain {
                item_pushes.extend(quote! {
                    __params.push(
                        Box::new(::serde_json::to_value(&#accessor)
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
    for (si, fpr) in fields_per_row_lits.iter().enumerate() {
        let field_ident = format_ident!("__spread_{}", si);
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

    // ── fetch_value() — only when there is exactly one column ────────
    let fetch_value_method = build_fetch_value_method(&query_info.columns)?;

    // ── Assemble the full block ─────────────────────────────────────────
    let ts = quote! {
        {
            #[derive(Debug, Clone)]
            #[allow(non_camel_case_types)]
            struct __sql_output {
                #output_struct
            }

            #[allow(non_camel_case_types)]
            struct __cubos_sql_query<'__s, __E: cubos_sql::Executor, #(#spread_generic_types,)*> {
                __executor: __E,
                #spread_struct_fields
                #regular_param_fields
            }

            fn __build_spread_sql(#spread_size_params) -> String {
                #sql_builder_body
            }

            impl<'__s, __E: cubos_sql::Executor, #(#spread_generic_types,)*>
                __cubos_sql_query<'__s, __E, #(#spread_generic_types,)*>
            {
                /// Execute the query and return all resulting rows.
                async fn fetch_all(self) -> ::std::result::Result<::std::vec::Vec<__sql_output>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    __rows.into_iter().map(|__row| {
                        ::std::result::Result::Ok(__sql_output { #row_mapping })
                    }).collect()
                }

                /// Execute the query and return exactly one row.
                ///
                /// Returns [`Error::NoRows`] if the query returns no rows, or
                /// [`Error::TooManyRows`] if it returns more than one.
                async fn fetch_one(self) -> ::std::result::Result<__sql_output, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Err(cubos_sql::Error::NoRows);
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next().ok_or_else(|| cubos_sql::Error::NoRows)?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                    }
                    ::std::result::Result::Ok(__sql_output { #row_mapping })
                }

                /// Execute the query and return at most one row.
                ///
                /// Returns `Ok(None)` if the query returns no rows, or
                /// [`Error::TooManyRows`] if it returns more than one.
                async fn fetch_optional(self) -> ::std::result::Result<::std::option::Option<__sql_output>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::option::Option::None);
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                            }
                            ::std::result::Result::Ok(Some(__sql_output { #row_mapping }))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }

                /// Execute the statement and return the number of affected rows.
                ///
                /// Intended for `INSERT`, `UPDATE`, and `DELETE` statements.
                async fn execute(self) -> ::std::result::Result<u64, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(0);
                    }
                    cubos_sql::Executor::execute(&self.__executor, &__sql, &__params_ref).await
                }

                #fetch_value_method

                /// Execute the query and return all resulting rows mapped to `T`.
                async fn fetch_all_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<::std::vec::Vec<__T>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::vec::Vec::new());
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    __rows.into_iter().map(|__row| {
                        __T::from_row(&__row)
                    }).collect()
                }

                /// Execute the query and return exactly one row mapped to `T`.
                async fn fetch_one_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<__T, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Err(cubos_sql::Error::NoRows);
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    let __row = __iter.next().ok_or_else(|| cubos_sql::Error::NoRows)?;
                    if __iter.next().is_some() {
                        return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                    }
                    __T::from_row(&__row)
                }

                /// Execute the query and return at most one row mapped to `T`.
                async fn fetch_optional_as<__T: cubos_sql::FromRow>(self) -> ::std::result::Result<::std::option::Option<__T>, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(::std::option::Option::None);
                    }
                    let __rows = cubos_sql::Executor::query(&self.__executor, &__sql, &__params_ref).await?;
                    let mut __iter = __rows.into_iter();
                    match __iter.next() {
                        Some(__row) => {
                            if __iter.next().is_some() {
                                return ::std::result::Result::Err(cubos_sql::Error::TooManyRows);
                            }
                            ::std::result::Result::Ok(Some(__T::from_row(&__row)?))
                        },
                        None => ::std::result::Result::Ok(None),
                    }
                }
            }

            __cubos_sql_query {
                __executor: #executor_expr,
                #spread_struct_inits
                #regular_param_inits
            }
        }
    };

    Ok(ts)
}

// ---------------------------------------------------------------------------
// Helper: fetch_value() method (single-column queries only)
// ---------------------------------------------------------------------------

/// When the query returns exactly one column, generates `fetch_value()` and
/// `fetch_value_optional()` methods that return the column's Rust type directly
/// instead of a struct wrapper.
///
/// Returns an empty `TokenStream` when there are zero or more than one column.
fn build_fetch_value_method(columns: &[ColumnInfo]) -> Result<TokenStream, syn::Error> {
    if columns.len() != 1 {
        return Ok(TokenStream::new());
    }

    let col = &columns[0];
    let return_type = column_rust_type(col)?;
    let field_name = make_field_ident(&col.name);

    // For fetch_value_optional: when the column is nullable, the field is
    // already Option<T>, so we return it directly to avoid Option<Option<T>>.
    let optional_body = if col.nullable {
        quote! {
            match self.fetch_optional().await? {
                Some(__v) => ::std::result::Result::Ok(__v.#field_name),
                None => ::std::result::Result::Ok(None),
            }
        }
    } else {
        quote! {
            match self.fetch_optional().await? {
                Some(__v) => ::std::result::Result::Ok(Some(__v.#field_name)),
                None => ::std::result::Result::Ok(None),
            }
        }
    };

    // Return type for fetch_value_optional is always Option<T> — same as the
    // field type when nullable, or Option-wrapped when not nullable.
    let optional_return_type = if col.nullable {
        quote! { #return_type }
    } else {
        quote! { ::std::option::Option<#return_type> }
    };

    Ok(quote! {
        /// Execute the query and return the single column value from exactly one row.
        ///
        /// This method is only available when the query returns a single column.
        /// Returns [`Error::NoRows`] if the query returns no rows, or
        /// [`Error::TooManyRows`] if it returns more than one.
        async fn fetch_value(self) -> ::std::result::Result<#return_type, cubos_sql::Error> {
            let __v = self.fetch_one().await?;
            ::std::result::Result::Ok(__v.#field_name)
        }

        /// Execute the query and return the single column value from at most one row.
        ///
        /// This method is only available when the query returns a single column.
        /// Returns `Ok(None)` if the query returns no rows (or the value is `NULL`
        /// for nullable columns), or [`Error::TooManyRows`] if it returns more than one.
        async fn fetch_value_optional(self) -> ::std::result::Result<#optional_return_type, cubos_sql::Error> {
            #optional_body
        }
    })
}

// ---------------------------------------------------------------------------
// Helper: output struct fields
// ---------------------------------------------------------------------------

/// Generates the field list for `__sql_output`.
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
        let pi = query_info.params.get(idx).ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!("could not infer type for parameter `${}`", param.name),
            )
        })?;
        let is_nullable = pi.nullable;
        let is_domain = pi.domain_rust_type.is_some();
        let is_enum = pi.enum_rust_type.is_some();
        let inner_type_str = if let Some(domain) = &pi.domain_rust_type {
            domain.clone()
        } else if let Some(enum_type) = &pi.enum_rust_type {
            enum_type.clone()
        } else {
            pi.rust_type.clone()
        };

        let field_type: syn::Type = if is_nullable {
            parse_str(&format!("::std::option::Option<{inner_type_str}>"))?
        } else {
            parse_str(&inner_type_str)?
        };

        // Resolve the value expression for this parameter.
        let value_expr: TokenStream = resolve_param_value(&param.name, assignments)?;

        // For String fields (non-domain, non-enum), wrap with `.into()`.
        let is_string_field = pi.rust_type == "String" && !is_domain && !is_enum;
        let value_expr = if is_string_field {
            if is_nullable {
                quote! { (#value_expr).map(Into::<String>::into) }
            } else {
                quote! { Into::<String>::into(#value_expr) }
            }
        } else {
            value_expr
        };

        defs.extend(quote! {
            #field_name: #field_type,
        });

        let param_ident = format_ident!("__{}", param.name);
        inits.extend(quote! {
            #field_name: { let #param_ident: #field_type = #value_expr; #param_ident },
        });
    }

    Ok((defs, inits))
}

/// Produces the value expression that will be stored in the query struct field.
///
/// Returns the raw user expression — domain/enum serialization happens at push
/// time, not here.
fn resolve_param_value(
    param_name: &str,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let assignment = assignments.iter().find(|a| a.name == param_name);

    match assignment {
        Some(ParamAssignment { expr: Some(e), .. }) => Ok(quote! { #e }),
        Some(ParamAssignment { expr: None, .. }) | None => {
            let ident = format_ident!("{}", param_name);
            Ok(quote! { #ident })
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: cast domain/enum params in SQL
// ---------------------------------------------------------------------------

/// Rewrite the SQL to add type casts after each parameter placeholder.
///
/// Uses `ParamInfo::cast_type` (resolved from the base type after unwrapping
/// domains) and the byte offsets recorded by the lexer. Parameters without
/// a `cast_type` (unknown OIDs, custom types) are left uncast.
fn cast_params(lex_output: &LexOutput, query_info: &QueryInfo) -> String {
    let mut insertions: Vec<(usize, String)> = Vec::new();

    for (idx, param) in lex_output.params.iter().enumerate() {
        if let Some(pi) = query_info.params.get(idx)
            && let Some(pg_type) = &pi.cast_type
        {
            let cast_str = format!("::{pg_type}");
            for &offset in &param.sql_offsets {
                insertions.push((offset, cast_str.clone()));
            }
        }
    }

    if insertions.is_empty() {
        return lex_output.sql.clone();
    }

    insertions.sort_by_key(|(off, _)| *off);

    let sql = &lex_output.sql;
    let mut result = String::with_capacity(sql.len() + insertions.len() * 8);
    let mut last = 0;
    for (offset, cast_str) in &insertions {
        result.push_str(&sql[last..*offset]);
        result.push_str(cast_str);
        last = *offset;
    }
    result.push_str(&sql[last..]);

    result
}

// ---------------------------------------------------------------------------
// Helper: `&[&(dyn ToSql + Sync)]` slice
// ---------------------------------------------------------------------------

/// Builds the comma-separated list of `&self.pN as &(dyn ToSql + Sync)` used
/// inside the `&[...]` slice literal passed to `Executor::query` /
/// `Executor::execute`.
fn build_params_slice(
    lex_output: &LexOutput,
    query_info: &QueryInfo,
) -> Result<TokenStream, syn::Error> {
    let mut elems = TokenStream::new();

    for idx in 0..lex_output.params.len() {
        let field_name = format_ident!("p{}", idx);
        let pi = &query_info.params[idx];

        if pi.domain_rust_type.is_some() {
            if pi.nullable {
                // Option<DomainType> → Option<serde_json::Value> → &dyn ToSql
                elems.extend(quote! {
                    &match &self.#field_name {
                        Some(__v) => Some(::serde_json::to_value(__v)
                            .map_err(|e| cubos_sql::Error::Serialize(
                                format!("failed to serialize domain type to JSON: {e}")))?),
                        None => None,
                    } as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
                });
            } else {
                elems.extend(quote! {
                    &::serde_json::to_value(&self.#field_name)
                        .map_err(|e| cubos_sql::Error::Serialize(
                            format!("failed to serialize domain type to JSON: {e}")))?
                        as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
                });
            }
        } else if pi.enum_rust_type.is_some() {
            if pi.nullable {
                elems.extend(quote! {
                    &self.#field_name.as_ref().map(|__v| ::cubos_sql::__private::EnumString(__v.to_string()))
                        as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
                });
            } else {
                elems.extend(quote! {
                    &::cubos_sql::__private::EnumString(self.#field_name.to_string())
                        as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
                });
            }
        } else {
            elems.extend(quote! {
                &self.#field_name as &(dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync),
            });
        }
    }

    Ok(elems)
}

// ---------------------------------------------------------------------------
// Helper: row mapping inside `map(|__row| { ... })`
// ---------------------------------------------------------------------------

/// Produces the field assignments for `__sql_output { ... }` from a
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

/// Returns the Rust type token for a column field in `__sql_output`.
///
/// Domain types override the base type. Nullable columns are wrapped in
/// `Option<T>`.
fn column_rust_type(col: &ColumnInfo) -> Result<syn::Type, syn::Error> {
    // The concrete inner type: domain/enum type takes priority over the base type.
    // Domain/enum types are user-provided paths; base types need qualification.
    let inner_type_str = if let Some(domain) = col.domain_rust_type.as_deref() {
        domain.to_string()
    } else if let Some(enum_ty) = col.enum_rust_type.as_deref() {
        enum_ty.to_string()
    } else {
        col.rust_type.clone()
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
                    let __json_val = __row.get::<_, ::std::option::Option<::serde_json::Value>>(#idx_lit);
                    match __json_val {
                        Some(__v) => Some(::serde_json::from_value::<#domain_type>(__v)
                            .map_err(|e| cubos_sql::Error::Deserialize(
                                format!("failed to deserialize {}: {e}", stringify!(#domain_type))))?),
                        None => None,
                    }
                }
            })
        } else {
            Ok(quote! {
                ::serde_json::from_value::<#domain_type>(
                    __row.get::<_, ::serde_json::Value>(#idx_lit)
                ).map_err(|e| cubos_sql::Error::Deserialize(
                    format!("failed to deserialize {}: {e}", stringify!(#domain_type))))?
            })
        }
    } else if let Some(enum_type_str) = &col.enum_rust_type {
        // Enum column: read as EnumString (a FromSql wrapper that accepts
        // Kind::Enum), then parse the label into the mapped Rust type.
        let enum_type: syn::Type = parse_str(enum_type_str)?;

        if col.nullable {
            Ok(quote! {
                {
                    let __enum_val = __row.get::<_, ::std::option::Option<::cubos_sql::__private::EnumString>>(#idx_lit);
                    match __enum_val {
                        Some(__v) => Some(__v.0.parse::<#enum_type>()
                            .map_err(|e| cubos_sql::Error::Deserialize(
                                format!("failed to parse enum {}: {e}", stringify!(#enum_type))))?),
                        None => None,
                    }
                }
            })
        } else {
            Ok(quote! {
                {
                    let __enum_val = __row.get::<_, ::cubos_sql::__private::EnumString>(#idx_lit);
                    __enum_val.0.parse::<#enum_type>()
                        .map_err(|e| cubos_sql::Error::Deserialize(
                            format!("failed to parse enum {}: {e}", stringify!(#enum_type))))?
                }
            })
        }
    } else {
        // Plain column.
        let base_type: syn::Type = parse_str(&col.rust_type)?;

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
                    cast_type: None,
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
        assert!(code.contains("__sql_output"), "should define output struct");
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

    #[test]
    fn fetch_value_generated_for_single_column() {
        let lo = lex("SELECT count(*) FROM users").unwrap();
        let qi = make_query_info(&[], vec![make_column("count", "i64", false)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            code.contains("fetch_value"),
            "single-column query should have fetch_value"
        );
        assert!(
            code.contains("fetch_value_optional"),
            "single-column query should have fetch_value_optional"
        );
    }

    #[test]
    fn fetch_value_not_generated_for_multiple_columns() {
        let lo = lex("SELECT id, name FROM users").unwrap();
        let qi = make_query_info(
            &[],
            vec![
                make_column("id", "i64", false),
                make_column("name", "String", false),
            ],
        );
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            !code.contains("fetch_value"),
            "multi-column query should NOT have fetch_value"
        );
    }

    #[test]
    fn fetch_value_not_generated_for_zero_columns() {
        let lo = lex("DELETE FROM users WHERE id = $id").unwrap();
        let qi = make_query_info(&["i64"], vec![]);
        let executor = parse_executor_expr();
        let assignments = vec![ParamAssignment {
            name: "id".into(),
            expr: Some(syn::parse_str("1i64").unwrap()),
        }];

        let ts = generate(&lo, &qi, &executor, &assignments).unwrap();
        let code = ts.to_string();

        assert!(
            !code.contains("fetch_value"),
            "zero-column query should NOT have fetch_value"
        );
    }

    #[test]
    fn fetch_value_nullable_column_returns_option() {
        let lo = lex("SELECT max(age) FROM users").unwrap();
        let qi = make_query_info(&[], vec![make_column("max", "i32", true)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            code.contains("fetch_value"),
            "single nullable column should have fetch_value"
        );
        assert!(
            code.contains("Option"),
            "nullable column fetch_value should return Option"
        );
    }

    #[test]
    fn fetch_value_on_spread_single_column() {
        let lo = lex("INSERT INTO users (name) VALUES $..users { name } RETURNING id").unwrap();
        let qi = make_query_info(&["String"], vec![make_column("id", "i64", false)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            code.contains("fetch_value"),
            "spread with single RETURNING column should have fetch_value"
        );
    }

    #[test]
    fn fetch_as_methods_generated_for_regular_query() {
        let lo = lex("SELECT id, name FROM users").unwrap();
        let qi = make_query_info(
            &[],
            vec![
                make_column("id", "i64", false),
                make_column("name", "String", false),
            ],
        );
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(code.contains("fetch_all_as"), "should have fetch_all_as");
        assert!(code.contains("fetch_one_as"), "should have fetch_one_as");
        assert!(
            code.contains("fetch_optional_as"),
            "should have fetch_optional_as"
        );
        assert!(
            code.contains("cubos_sql :: FromRow"),
            "fetch_as methods should require FromRow bound"
        );
    }

    #[test]
    fn fetch_as_methods_generated_for_spread_query() {
        let lo = lex("INSERT INTO t (x) VALUES $..data { x } RETURNING id").unwrap();
        let qi = make_query_info(&["i32"], vec![make_column("id", "i64", false)]);
        let executor = parse_executor_expr();

        let ts = generate(&lo, &qi, &executor, &[]).unwrap();
        let code = ts.to_string();

        assert!(
            code.contains("fetch_all_as"),
            "spread should have fetch_all_as"
        );
        assert!(
            code.contains("fetch_one_as"),
            "spread should have fetch_one_as"
        );
        assert!(
            code.contains("fetch_optional_as"),
            "spread should have fetch_optional_as"
        );
    }
}

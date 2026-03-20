//! Code generation for the `query!` macro.
//!
//! Receives the lexer output ([`cubos_sql_core::param::LexOutput`]) and the
//! introspection result ([`crate::introspect::QueryInfo`]) and produces a
//! [`proc_macro2::TokenStream`] that implements the typed query builder.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse_str;

use crate::introspect::{ColumnInfo, QueryInfo};
use cubos_sql_core::param::LexOutput;

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
    // 5. The SQL string literal.
    // ------------------------------------------------------------------
    let sql_str = &lex_output.sql;

    // ------------------------------------------------------------------
    // 6. Assemble the full block.
    // ------------------------------------------------------------------
    let ts = quote! {
        {
            // ----- output type -----
            #[derive(Debug)]
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
                    ::std::result::Result::Ok(__rows.into_iter().map(|__row| {
                        __cubos_sql_output {
                            #row_mapping
                        }
                    }).collect())
                }

                async fn fetch_one(self) -> ::std::result::Result<__cubos_sql_output, cubos_sql::Error> {
                    let __rows = cubos_sql::Executor::query(
                        self.__executor,
                        #sql_str,
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
                        #sql_str,
                        &[#params_slice],
                    ).await?;
                    ::std::result::Result::Ok(__rows.into_iter().next().map(|__row| {
                        __cubos_sql_output {
                            #row_mapping
                        }
                    }))
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
        let field_name = format_ident!("{}", col.name);
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
        let field_type: syn::Type = if let Some(pi) = param_info {
            // Domain types are serialized to serde_json::Value before sending.
            if pi.domain_rust_type.is_some() {
                parse_str("::cubos_sql::__private::serde_json::Value")?
            } else {
                parse_str::<syn::Type>(&pi.rust_type)?
            }
        } else {
            // Fallback: use a generic ToSql-compatible type; this should not
            // happen in practice since introspection covers all params.
            parse_str("::cubos_sql::__private::serde_json::Value")?
        };

        // Resolve the value expression for this parameter.
        let value_expr: TokenStream = resolve_param_value(&param.name, param_info.and_then(|p| p.domain_rust_type.as_deref()), assignments)?;

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
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    // Find the matching assignment by name.
    let assignment = assignments.iter().find(|a| a.name == param_name);

    // Determine the raw value expression.
    let raw_expr: TokenStream = match assignment {
        Some(ParamAssignment { expr: Some(e), .. }) => {
            // Explicit expression provided by the caller.
            quote! { #e }
        }
        Some(ParamAssignment { expr: None, .. }) | None => {
            // Scope capture: use the parameter name as a variable reference.
            let ident = format_ident!("{}", param_name);
            quote! { #ident }
        }
    };

    // Wrap domain types so they are stored as JSON values.
    if domain_rust_type.is_some() {
        Ok(quote! { ::cubos_sql::__private::serde_json::to_value(&#raw_expr).unwrap() })
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
        let field_name = format_ident!("{}", col.name);
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
    let inner_type_str = col
        .domain_rust_type
        .as_deref()
        .unwrap_or(col.rust_type.as_str());

    let inner_type: syn::Type = parse_str(inner_type_str)?;

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
        // Domain column: Postgres stores it as JSONB; we read a
        // `serde_json::Value` and then deserialize into the domain type.
        let domain_type: syn::Type = parse_str(domain_type_str)?;

        if col.nullable {
            Ok(quote! {
                {
                    let __json_val = __row.get::<_, ::std::option::Option<::cubos_sql::__private::serde_json::Value>>(#idx_lit);
                    __json_val.map(|__v| ::cubos_sql::__private::serde_json::from_value::<#domain_type>(__v).unwrap())
                }
            })
        } else {
            Ok(quote! {
                ::cubos_sql::__private::serde_json::from_value::<#domain_type>(
                    __row.get::<_, ::cubos_sql::__private::serde_json::Value>(#idx_lit)
                ).unwrap()
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

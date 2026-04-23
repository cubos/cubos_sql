//! Code generation for the `sql!` macro.
//!
//! Receives an [`AnalyzedQuery`] from `cubos_sql_analyzer` and produces a
//! [`proc_macro2::TokenStream`] that implements the typed query builder.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse_str;

use cubos_sql_analyzer::{AnalyzedColumn, AnalyzedParam, AnalyzedQuery, AnalyzedSpreadField};

/// Rust keywords that cannot be used as identifiers without the `r#` prefix.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "try", "typeof", "unsized", "virtual",
];

/// Sanitize a SQL column name into a valid Rust identifier.
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

/// Small accessors so the same helpers work for regular params and spread fields.
trait TypedParam {
    fn rust_type(&self) -> &str;
    fn nullable(&self) -> bool;
    fn domain_rust_type(&self) -> Option<&str>;
    fn enum_rust_type(&self) -> Option<&str>;
}

impl TypedParam for AnalyzedParam {
    fn rust_type(&self) -> &str {
        &self.rust_type
    }
    fn nullable(&self) -> bool {
        self.nullable
    }
    fn domain_rust_type(&self) -> Option<&str> {
        self.domain_rust_type.as_deref()
    }
    fn enum_rust_type(&self) -> Option<&str> {
        self.enum_rust_type.as_deref()
    }
}

impl TypedParam for AnalyzedSpreadField {
    fn rust_type(&self) -> &str {
        &self.rust_type
    }
    fn nullable(&self) -> bool {
        self.nullable
    }
    fn domain_rust_type(&self) -> Option<&str> {
        self.domain_rust_type.as_deref()
    }
    fn enum_rust_type(&self) -> Option<&str> {
        self.enum_rust_type.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn generate(
    analyzed: &AnalyzedQuery,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    if analyzed.spreads.is_empty() {
        generate_regular(analyzed, executor_expr, assignments)
    } else {
        generate_spread(analyzed, executor_expr, assignments)
    }
}

/// Generate code for a regular query (no spreads).
fn generate_regular(
    analyzed: &AnalyzedQuery,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let output_struct = build_output_struct(&analyzed.columns)?;
    let (param_field_defs, param_field_inits) = build_param_fields(analyzed, assignments)?;
    let params_slice = build_params_slice(analyzed)?;
    let row_mapping = build_row_mapping(&analyzed.columns)?;

    let sql_str = cast_params(analyzed);
    let sql_limited = wrap_with_limit(&sql_str).unwrap_or_else(|| sql_str.clone());

    let fetch_value_method = build_fetch_value_method(&analyzed.columns)?;

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

fn generate_spread(
    analyzed: &AnalyzedQuery,
    executor_expr: &syn::Expr,
    assignments: &[ParamAssignment],
) -> Result<TokenStream, syn::Error> {
    let output_struct = build_output_struct(&analyzed.columns)?;
    let row_mapping = build_row_mapping(&analyzed.columns)?;
    let num_regular_params = analyzed.params.len();
    let num_spreads = analyzed.spreads.len();

    // ── Regular param fields ────────────────────────────────────────────
    let mut regular_param_fields = TokenStream::new();
    let mut regular_param_inits = TokenStream::new();
    let mut regular_param_pushes = TokenStream::new();
    for (idx, param) in analyzed.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);
        let (field_type, value_expr) =
            build_field_type_and_value(param, &resolve_param_value(&param.name, assignments)?)?;

        regular_param_fields.extend(quote! { #field_name: #field_type, });
        let param_ident = format_ident!("__{}", param.name);
        regular_param_inits.extend(quote! {
            #field_name: { let #param_ident: #field_type = #value_expr; #param_ident },
        });
        regular_param_pushes.extend(push_param(param, &quote! { self.#field_name }));
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

    for (si, spread) in analyzed.spreads.iter().enumerate() {
        let col_count = spread.fields.len();
        let type_ident = format_ident!("__S{}", si);
        let field_ident = format_ident!("__spread_{}", si);
        let size_ident = format_ident!("__size_{}", si);

        spread_generic_types.push(type_ident.clone());

        // SQL piece before this spread
        sql_pieces.push(&analyzed.sql[last_offset..spread.offset]);
        last_offset = spread.offset;
        fields_per_row_lits.push(proc_macro2::Literal::usize_unsuffixed(col_count));

        // Struct field + init (all spreads share the '__s lifetime)
        spread_struct_fields.extend(quote! {
            #field_ident: &'__s [#type_ident],
        });

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

        spread_empty_checks.extend(quote! {
            if self.#field_ident.is_empty() { __any_empty = true; }
        });

        spread_size_args.extend(quote! { self.#field_ident.len(), });
        spread_size_params.extend(quote! { #size_ident: usize, });

        // Param push expressions: iterate spread items and push field values
        let mut item_pushes = TokenStream::new();
        for field in &spread.fields {
            let accessor_ident = format_ident!("{}", field.name);
            let accessor: TokenStream = quote! { __item.#accessor_ident };
            item_pushes.extend(push_param(field, &accessor));
        }

        spread_param_pushes.extend(quote! {
            for __item in self.#field_ident.iter() {
                #item_pushes
            }
        });
    }

    // Final SQL piece (after last spread)
    sql_pieces.push(&analyzed.sql[last_offset..]);

    // ── Generate the __build_spread_sql function body ────────────────────
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

    let fetch_value_method = build_fetch_value_method(&analyzed.columns)?;

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

                async fn execute(self) -> ::std::result::Result<u64, cubos_sql::Error> {
                    #query_preamble
                    if __any_empty {
                        return ::std::result::Result::Ok(0);
                    }
                    cubos_sql::Executor::execute(&self.__executor, &__sql, &__params_ref).await
                }

                #fetch_value_method

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

fn build_fetch_value_method(columns: &[AnalyzedColumn]) -> Result<TokenStream, syn::Error> {
    if columns.len() != 1 {
        return Ok(TokenStream::new());
    }

    let col = &columns[0];
    let return_type = column_rust_type(col)?;
    let field_name = make_field_ident(&col.name);

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

    let optional_return_type = if col.nullable {
        quote! { #return_type }
    } else {
        quote! { ::std::option::Option<#return_type> }
    };

    Ok(quote! {
        async fn fetch_value(self) -> ::std::result::Result<#return_type, cubos_sql::Error> {
            let __v = self.fetch_one().await?;
            ::std::result::Result::Ok(__v.#field_name)
        }

        async fn fetch_value_optional(self) -> ::std::result::Result<#optional_return_type, cubos_sql::Error> {
            #optional_body
        }
    })
}

// ---------------------------------------------------------------------------
// Helper: output struct fields
// ---------------------------------------------------------------------------

fn build_output_struct(columns: &[AnalyzedColumn]) -> Result<TokenStream, syn::Error> {
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

fn build_param_fields(
    analyzed: &AnalyzedQuery,
    assignments: &[ParamAssignment],
) -> Result<(TokenStream, TokenStream), syn::Error> {
    let mut defs = TokenStream::new();
    let mut inits = TokenStream::new();

    for (idx, param) in analyzed.params.iter().enumerate() {
        let field_name = format_ident!("p{}", idx);
        let value_expr = resolve_param_value(&param.name, assignments)?;
        let (field_type, value_expr) = build_field_type_and_value(param, &value_expr)?;

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

/// Compute the Rust field type and (optionally wrapped) value expression for a
/// query parameter.
fn build_field_type_and_value<P: TypedParam>(
    param: &P,
    value_expr: &TokenStream,
) -> Result<(syn::Type, TokenStream), syn::Error> {
    let is_nullable = param.nullable();
    let is_domain = param.domain_rust_type().is_some();
    let is_enum = param.enum_rust_type().is_some();

    let inner_type_str = if let Some(domain) = param.domain_rust_type() {
        domain.to_string()
    } else if let Some(enum_ty) = param.enum_rust_type() {
        enum_ty.to_string()
    } else {
        param.rust_type().to_string()
    };

    let field_type: syn::Type = if is_nullable {
        parse_str(&format!("::std::option::Option<{inner_type_str}>"))?
    } else {
        parse_str(&inner_type_str)?
    };

    let is_string_field = param.rust_type() == "String" && !is_domain && !is_enum;
    let value_expr = if is_string_field {
        if is_nullable {
            quote! { (#value_expr).map(Into::<String>::into) }
        } else {
            quote! { Into::<String>::into(#value_expr) }
        }
    } else {
        value_expr.clone()
    };

    Ok((field_type, value_expr))
}

/// Build the `push` statement for a param/field in the spread execution path.
///
/// `accessor` is the expression that evaluates to the value (e.g. `self.p0`
/// for regular params or `__item.name` for spread fields).
fn push_param<P: TypedParam>(param: &P, accessor: &TokenStream) -> TokenStream {
    let is_nullable = param.nullable();
    let is_domain = param.domain_rust_type().is_some();
    let is_enum = param.enum_rust_type().is_some();

    if is_domain {
        if is_nullable {
            quote! {
                __params.push(Box::new(match &#accessor {
                    Some(__v) => Some(::serde_json::to_value(__v)
                        .map_err(|e| cubos_sql::Error::Serialize(
                            format!("failed to serialize domain type to JSON: {e}")))?),
                    None => None,
                }) as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
            }
        } else {
            quote! {
                __params.push(Box::new(::serde_json::to_value(&#accessor)
                    .map_err(|e| cubos_sql::Error::Serialize(
                        format!("failed to serialize domain type to JSON: {e}")))?)
                    as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
            }
        }
    } else if is_enum {
        if is_nullable {
            quote! {
                __params.push(Box::new(#accessor.as_ref().map(|__v| __v.to_string()))
                    as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
            }
        } else {
            quote! {
                __params.push(Box::new(#accessor.to_string())
                    as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
            }
        }
    } else {
        quote! {
            __params.push(Box::new(#accessor.clone())
                as Box<dyn ::cubos_sql::__private::tokio_postgres::types::ToSql + Sync>);
        }
    }
}

/// Produces the value expression that will be stored in the query struct field.
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

fn cast_params(analyzed: &AnalyzedQuery) -> String {
    let mut insertions: Vec<(usize, String)> = Vec::new();

    for param in &analyzed.params {
        if let Some(pg_type) = &param.cast_type {
            let cast_str = format!("::{pg_type}");
            for &offset in &param.sql_offsets {
                insertions.push((offset, cast_str.clone()));
            }
        }
    }

    if insertions.is_empty() {
        return analyzed.sql.clone();
    }

    insertions.sort_by_key(|(off, _)| *off);

    let sql = &analyzed.sql;
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

fn build_params_slice(analyzed: &AnalyzedQuery) -> Result<TokenStream, syn::Error> {
    let mut elems = TokenStream::new();

    for idx in 0..analyzed.params.len() {
        let field_name = format_ident!("p{}", idx);
        let pi = &analyzed.params[idx];

        if pi.domain_rust_type.is_some() {
            if pi.nullable {
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

fn build_row_mapping(columns: &[AnalyzedColumn]) -> Result<TokenStream, syn::Error> {
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

fn column_rust_type(col: &AnalyzedColumn) -> Result<syn::Type, syn::Error> {
    let inner_type_str = if let Some(domain) = col.domain_rust_type.as_deref() {
        domain.to_string()
    } else if let Some(enum_ty) = col.enum_rust_type.as_deref() {
        enum_ty.to_string()
    } else {
        col.rust_type.clone()
    };

    let inner_type: syn::Type = parse_str(&inner_type_str)?;

    if col.nullable {
        Ok(parse_str(&format!(
            "::std::option::Option<{}>",
            inner_type_str
        ))?)
    } else {
        Ok(inner_type)
    }
}

fn column_get_expr(col: &AnalyzedColumn, idx: usize) -> Result<TokenStream, syn::Error> {
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

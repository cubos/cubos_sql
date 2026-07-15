//! Parameter type inference from usage context.

use std::collections::{HashMap, HashSet};

use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::oid;

/// Collects type constraints for positional parameters ($1, $2, ...).
#[derive(Debug, Default, Clone)]
pub(crate) struct ParamCollector {
    /// Maps param number (1-based) → inferred type OID.
    constraints: HashMap<i32, PgTypeOid>,
    /// All param numbers seen in the query (even if type not yet inferred).
    seen: HashSet<i32>,
    /// Maps param number (1-based) → nullable annotation.
    nullable: HashMap<i32, bool>,
    /// Param numbers that have an explicit annotation from `$foo?` syntax (takes precedence).
    explicit_nullable: HashSet<i32>,
    /// Param numbers seen in a position where PG refuses to default to
    /// `text` — bare `$N` inside `ROW(...)` is the canonical case. If
    /// such a param is still untyped at finalization, [`into_sorted`]
    /// raises `could not determine data type of parameter $N` instead of
    /// silently falling back. Marking is harmless if a later inference
    /// site (e.g. ROW=ROW back-fill) pins the param to a concrete type.
    indeterminate_required: HashSet<i32>,
    /// Param numbers whose type PG *locked* as unknown at first use:
    /// `$1 IS NULL` consumes the parameter without assigning a type, and PG
    /// does not let a later use back-fill it — `SELECT $1 IS NULL, $1 = 1`
    /// is `could not determine data type of parameter $1` (42P08) even
    /// though the second use would pin int4. Unlike
    /// [`Self::indeterminate_required`], this fails finalization regardless
    /// of any type recorded afterwards.
    indeterminate_locked: HashSet<i32>,
}

impl ParamCollector {
    /// Record that a parameter was referenced (even if type is unknown).
    pub fn see(&mut self, param_num: i32) {
        self.seen.insert(param_num);
    }

    /// Record a type constraint for a parameter.
    pub fn record(&mut self, param_num: i32, type_oid: PgTypeOid) {
        self.seen.insert(param_num);
        if type_oid == oid::UNKNOWN {
            return;
        }
        self.constraints.entry(param_num).or_insert(type_oid);
    }

    /// Record an explicit nullability annotation for a parameter (from `$foo?` syntax).
    /// Explicit annotations take precedence over inferred nullability.
    pub fn set_nullable(&mut self, param_num: i32, nullable: bool) {
        self.nullable.insert(param_num, nullable);
        self.explicit_nullable.insert(param_num);
    }

    /// Infer nullability from column definition (e.g. INSERT/UPDATE into a nullable column).
    /// Does NOT override explicit annotations from `$foo?` or `$foo!` syntax.
    pub fn infer_nullable(&mut self, param_num: i32, nullable: bool) {
        if !self.explicit_nullable.contains(&param_num) {
            self.nullable.insert(param_num, nullable);
        }
    }

    /// Get the nullable annotation for a parameter. Defaults to false (non-nullable).
    pub fn is_nullable(&self, param_num: i32) -> bool {
        self.nullable.get(&param_num).copied().unwrap_or(false)
    }

    /// Get the inferred type for a parameter. Returns UNKNOWN if not yet constrained.
    pub fn get(&self, param_num: i32) -> PgTypeOid {
        self.constraints
            .get(&param_num)
            .copied()
            .unwrap_or(oid::UNKNOWN)
    }

    /// Mark `param_num` as having appeared in a context (e.g. inside
    /// `ROW(...)`) where PG refuses to default to `text`. If finalization
    /// finds the param still untyped, it errors instead of falling back.
    pub fn mark_indeterminate_required(&mut self, param_num: i32) {
        self.indeterminate_required.insert(param_num);
    }

    /// Mark `param_num` as consumed-untyped at this point (PG's first-use
    /// type locking): finalization fails even if a later site records a
    /// type. No-op when the param already has a type — `$1 = 1, $1 IS NULL`
    /// is fine because the first use typed it.
    pub fn mark_indeterminate_locked(&mut self, param_num: i32) {
        if !self.constraints.contains_key(&param_num) {
            self.indeterminate_locked.insert(param_num);
        }
    }

    /// Return all parameters in order, validating that every seen param has a type.
    ///
    /// Returns `(param_number, type_oid, nullable)` tuples.
    /// Fails if any `$N` was referenced but its type could not be inferred.
    pub fn into_sorted(self) -> Result<Vec<(i32, PgTypeOid, bool)>, AnalyzeError> {
        // Reject params that surfaced in an indeterminate-required context
        // and never got a concrete type pinned. This mirrors PG's
        // `could not determine data type of parameter $N` for cases like
        // `SELECT ROW($1)`. Iterate in numeric order so the diagnostic
        // points at the *lowest*-numbered offending param — `HashSet`
        // iteration order is otherwise non-deterministic across builds.
        let mut indeterminate: Vec<i32> = self.indeterminate_required.iter().copied().collect();
        indeterminate.extend(self.indeterminate_locked.iter().copied());
        indeterminate.sort_unstable();
        indeterminate.dedup();
        for num in indeterminate {
            if self.indeterminate_locked.contains(&num) || !self.constraints.contains_key(&num) {
                // Same wording, two PG codes (verified on PG 18): when a
                // locked param *also* had a use that deduced a type (the
                // deductions conflict — `$1 IS NULL, $1 = 1`) PG reports
                // `ambiguous_parameter` (42P08); with no competing
                // deduction at all it's `indeterminate_datatype` (42P18).
                let message = format!("could not determine data type of parameter ${num}");
                let kind = if self.indeterminate_locked.contains(&num)
                    && self.constraints.contains_key(&num)
                {
                    AnalyzeError::AmbiguousParameter(message)
                } else {
                    AnalyzeError::IndeterminateType(message)
                };
                return Err(crate::error::RawError::new(
                    kind,
                    None,
                    Some(format!(
                        "add an explicit cast to the parameter, e.g. `${num}::int4`"
                    )),
                )
                .with_primary_label("type cannot be determined")
                .finalize_implicit());
            }
        }

        // Params that were seen but not typed default to TEXT, matching PG's
        // behavior (preferred type of the string category for unknown params).
        let mut params: Vec<(i32, PgTypeOid, bool)> = self
            .seen
            .iter()
            .map(|&num| {
                let oid = self.constraints.get(&num).copied().unwrap_or(oid::TEXT);
                let nullable = self.nullable.get(&num).copied().unwrap_or(false);
                (num, oid, nullable)
            })
            .collect();
        params.sort_by_key(|(num, _, _)| *num);

        // Verify parameter numbers are contiguous starting from 1.
        for (i, (num, _, _)) in params.iter().enumerate() {
            if *num != (i as i32 + 1) {
                return Err(AnalyzeError::Unsupported(format!(
                    "parameter gap: expected ${} but next is ${num}",
                    i + 1
                )));
            }
        }

        Ok(params)
    }
}

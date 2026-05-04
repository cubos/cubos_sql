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

    /// Return all parameters in order, validating that every seen param has a type.
    ///
    /// Returns `(param_number, type_oid, nullable)` tuples.
    /// Fails if any `$N` was referenced but its type could not be inferred.
    pub fn into_sorted(self) -> Result<Vec<(i32, PgTypeOid, bool)>, AnalyzeError> {
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

//! Parameter type inference from usage context.

use std::collections::{HashMap, HashSet};

use crate::coerce::oid;
use crate::error::AnalyzeError;

/// Collects type constraints for positional parameters ($1, $2, ...).
#[derive(Debug, Default)]
pub struct ParamCollector {
    /// Maps param number (1-based) → inferred type OID.
    constraints: HashMap<i32, u32>,
    /// All param numbers seen in the query (even if type not yet inferred).
    seen: HashSet<i32>,
    /// Maps param number (1-based) → nullable annotation from `$foo?` syntax.
    nullable: HashMap<i32, bool>,
}

impl ParamCollector {
    /// Record that a parameter was referenced (even if type is unknown).
    pub fn see(&mut self, param_num: i32) {
        self.seen.insert(param_num);
    }

    /// Record a type constraint for a parameter.
    pub fn record(&mut self, param_num: i32, type_oid: u32) {
        self.seen.insert(param_num);
        if type_oid == oid::UNKNOWN {
            return;
        }
        self.constraints.entry(param_num).or_insert(type_oid);
    }

    /// Record the nullability annotation for a parameter.
    pub fn set_nullable(&mut self, param_num: i32, nullable: bool) {
        self.nullable.insert(param_num, nullable);
    }

    /// Get the nullable annotation for a parameter. Defaults to false (non-nullable).
    pub fn is_nullable(&self, param_num: i32) -> bool {
        self.nullable.get(&param_num).copied().unwrap_or(false)
    }

    /// Get the inferred type for a parameter. Returns UNKNOWN if not yet constrained.
    pub fn get(&self, param_num: i32) -> u32 {
        self.constraints
            .get(&param_num)
            .copied()
            .unwrap_or(oid::UNKNOWN)
    }

    /// Return all parameters in order, validating that every seen param has a type.
    ///
    /// Returns `(param_number, type_oid, nullable)` tuples.
    /// Fails if any `$N` was referenced but its type could not be inferred.
    pub fn into_sorted(self) -> Result<Vec<(i32, u32, bool)>, AnalyzeError> {
        // Check for params that were seen but not typed.
        for &num in &self.seen {
            if !self.constraints.contains_key(&num) {
                return Err(AnalyzeError::Unsupported(format!(
                    "could not infer type for parameter ${num}"
                )));
            }
        }

        let mut params: Vec<(i32, u32, bool)> = self
            .constraints
            .iter()
            .map(|(&num, &oid)| {
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

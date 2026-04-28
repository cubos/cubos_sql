//! Expand `GROUP BY` clauses with `GROUPING SETS`/`ROLLUP`/`CUBE` into the
//! flat list of grouping sets that PG would generate, and derive which
//! columns are *omitted* by at least one set.
//!
//! The result drives the nullability promotion in
//! [`crate::nullability::NullabilityContext::grouping_omitted`]: a column
//! that appears in every set keeps its base nullability, while one that is
//! present in some sets but absent from others must be reported as nullable
//! in the projection (PG fills those rows with NULL).
//!
//! For ordinary `GROUP BY a, b` (no `GroupingSet` nodes), the function
//! returns an empty set — every column is in the single grouping set, so no
//! promotion is needed.

use std::collections::HashSet;

use pg_query::protobuf::{self, GroupingSetKind, node};

use crate::scope::Scope;

/// Result of expanding a `GROUP BY` clause that contains
/// `GROUPING SETS`/`ROLLUP`/`CUBE`.
#[derive(Debug, Default)]
pub(crate) struct GroupingExpansion {
    /// Columns present in some grouping sets but absent from others — must
    /// be reported as nullable in the projection.
    pub omitted: HashSet<(String, String)>,
    /// Whether any grouping set is empty (i.e. aggregates the whole input).
    /// Non-COUNT aggregates can return NULL for that row when the input is
    /// empty, so they must be promoted to nullable.
    pub has_empty_set: bool,
}

/// Resolve `group_clause` into the columns that some grouping set omits and
/// whether any of those sets is empty.
///
/// Only plain `ColumnRef`s are tracked; non-column expressions in the
/// `GROUP BY` (e.g. `date_trunc('day', ts)`) contribute nothing — the
/// projection references them by the inner column anyway, and that column
/// might or might not be in scope.
pub(crate) fn expand_grouping_sets(
    group_clause: &[protobuf::Node],
    scope: &Scope,
) -> GroupingExpansion {
    if group_clause.is_empty() {
        return GroupingExpansion::default();
    }

    // Per top-level entry, the alternatives it contributes (each alternative
    // is one grouping set fragment).
    let mut per_entry: Vec<Vec<HashSet<(String, String)>>> = Vec::new();
    let mut saw_grouping_set = false;
    for node in group_clause {
        let alts = alternatives_for(node, scope, &mut saw_grouping_set);
        per_entry.push(alts);
    }

    if !saw_grouping_set {
        // Plain `GROUP BY a, b, …` — single grouping set, no omissions, and
        // (assuming `group_clause` is non-empty) not the empty grouping set.
        return GroupingExpansion::default();
    }

    // Cartesian product of alternatives — each combination yields one final
    // grouping set (the union of one alternative from each entry).
    let mut sets: Vec<HashSet<(String, String)>> = vec![HashSet::new()];
    for alts in &per_entry {
        if alts.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(sets.len() * alts.len());
        for prefix in &sets {
            for alt in alts {
                let mut combined = prefix.clone();
                combined.extend(alt.iter().cloned());
                next.push(combined);
            }
        }
        sets = next;
    }

    if sets.is_empty() {
        return GroupingExpansion::default();
    }

    let union: HashSet<(String, String)> = sets.iter().flatten().cloned().collect();
    let mut intersection = sets[0].clone();
    for s in &sets[1..] {
        intersection.retain(|x| s.contains(x));
    }
    let omitted = union.difference(&intersection).cloned().collect();
    let has_empty_set = sets.iter().any(|s| s.is_empty());
    GroupingExpansion {
        omitted,
        has_empty_set,
    }
}

/// Alternatives contributed by one node in the top-level `group_clause`.
fn alternatives_for(
    node: &protobuf::Node,
    scope: &Scope,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    match node.node.as_ref() {
        Some(node::Node::GroupingSet(gs)) => {
            *saw_grouping_set = true;
            alternatives_for_grouping_set(gs, scope, saw_grouping_set)
        }
        _ => vec![singleton_set(node, scope)],
    }
}

fn alternatives_for_grouping_set(
    gs: &protobuf::GroupingSet,
    scope: &Scope,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    let kind = GroupingSetKind::try_from(gs.kind).unwrap_or(GroupingSetKind::Undefined);
    match kind {
        GroupingSetKind::GroupingSetEmpty => vec![HashSet::new()],
        GroupingSetKind::GroupingSetSimple => {
            // `(a, b)` — a single set with the union of its members.
            let mut set = HashSet::new();
            for item in &gs.content {
                set.extend(singleton_set(item, scope));
            }
            vec![set]
        }
        GroupingSetKind::GroupingSetRollup => {
            // ROLLUP(a, b, c) → [{a,b,c}, {a,b}, {a}, {}]
            let items: Vec<HashSet<(String, String)>> =
                gs.content.iter().map(|n| singleton_set(n, scope)).collect();
            let mut alts = Vec::with_capacity(items.len() + 1);
            for cut in (0..=items.len()).rev() {
                let mut s = HashSet::new();
                for item in &items[..cut] {
                    s.extend(item.iter().cloned());
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetCube => {
            // CUBE(a, b) → powerset — 2^n sets.
            let items: Vec<HashSet<(String, String)>> =
                gs.content.iter().map(|n| singleton_set(n, scope)).collect();
            let n = items.len();
            let mut alts = Vec::with_capacity(1usize << n.min(16));
            for mask in 0..(1u32 << n) {
                let mut s = HashSet::new();
                for (i, item) in items.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        s.extend(item.iter().cloned());
                    }
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetSets => {
            // GROUPING SETS (s1, s2, …) — concatenate the alternatives of
            // each child. Each child is itself either an expression (one
            // alt = singleton) or a nested `GroupingSet`.
            let mut alts = Vec::new();
            for child in &gs.content {
                alts.extend(alternatives_for(child, scope, saw_grouping_set));
            }
            if alts.is_empty() {
                vec![HashSet::new()]
            } else {
                alts
            }
        }
        GroupingSetKind::Undefined => vec![HashSet::new()],
    }
}

/// Resolve a node as a single column reference. Returns a singleton set
/// `{(table_alias, column_name)}` if the node is a `ColumnRef` that scope
/// can resolve, or an empty set otherwise (opaque expression — does not
/// drive nullability promotion).
fn singleton_set(node: &protobuf::Node, scope: &Scope) -> HashSet<(String, String)> {
    let mut out = HashSet::new();
    let Some(node::Node::ColumnRef(cr)) = node.node.as_ref() else {
        return out;
    };
    let parts: Vec<&str> = cr
        .fields
        .iter()
        .filter_map(|f| match f.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let (table, column) = match parts.as_slice() {
        [col] => (None, *col),
        [tbl, col] => (Some(*tbl), *col),
        [_schema, tbl, col] => (Some(*tbl), *col),
        _ => return out,
    };
    if let Ok(col) = scope.resolve_column(table, column) {
        out.insert((col.table_alias.clone(), col.name.clone()));
    }
    out
}

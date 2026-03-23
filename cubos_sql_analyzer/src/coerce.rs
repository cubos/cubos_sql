//! Type coercion and common-type resolution.

use crate::schema::SchemaSnapshot;

/// Well-known OIDs for builtin types.
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const INT8: u32 = 20;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const NUMERIC: u32 = 1700;
    pub const TEXT: u32 = 25;
    pub const VARCHAR: u32 = 1043;
    pub const BPCHAR: u32 = 1042;
    pub const NAME: u32 = 19;
    pub const BYTEA: u32 = 17;
    pub const OID_TYPE: u32 = 26;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const UUID: u32 = 2950;
    pub const JSON: u32 = 114;
    pub const JSONB: u32 = 3802;
    pub const UNKNOWN: u32 = 705;
}

/// Numeric type promotion order (lower index = less preferred).
const NUMERIC_PROMOTION: &[(u32, u8)] = &[
    (oid::INT2, 0),
    (oid::INT4, 1),
    (oid::INT8, 2),
    (oid::NUMERIC, 3),
    (oid::FLOAT4, 4),
    (oid::FLOAT8, 5),
];

fn numeric_rank(type_oid: u32) -> Option<u8> {
    NUMERIC_PROMOTION
        .iter()
        .find(|(oid, _)| *oid == type_oid)
        .map(|(_, rank)| *rank)
}

/// Check if a type is a string-like type.
pub fn is_string_type(type_oid: u32) -> bool {
    matches!(type_oid, oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME)
}

/// Find the common supertype for a list of types.
///
/// Used for CASE, COALESCE, UNION column reconciliation.
pub fn find_common_type(types: &[u32], snapshot: &SchemaSnapshot) -> Option<u32> {
    if types.is_empty() {
        return None;
    }

    // Filter out UNKNOWN (untyped literals).
    let concrete: Vec<u32> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    if concrete.is_empty() {
        return Some(oid::TEXT); // All unknown → text
    }

    // If all the same, return that.
    if concrete.iter().all(|&t| t == concrete[0]) {
        return Some(concrete[0]);
    }

    // Try numeric promotion.
    if concrete.iter().all(|t| numeric_rank(*t).is_some()) {
        return concrete.iter().max_by_key(|t| numeric_rank(**t)).copied();
    }

    // Try string types → text.
    if concrete.iter().all(|t| is_string_type(*t)) {
        return Some(oid::TEXT);
    }

    // Try implicit casts: find a type that all others can cast to.
    for &candidate in &concrete {
        if concrete
            .iter()
            .all(|&t| t == candidate || snapshot.has_implicit_cast(t, candidate))
        {
            return Some(candidate);
        }
    }

    // Fallback: try text (many types can cast to text).
    if concrete
        .iter()
        .all(|&t| t == oid::TEXT || snapshot.has_implicit_cast(t, oid::TEXT))
    {
        return Some(oid::TEXT);
    }

    None
}

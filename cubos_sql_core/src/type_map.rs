//! Maps PostgreSQL type OIDs to Rust type information.
//!
//! Used by the proc macro to generate correctly-typed output and parameter code
//! without needing a runtime HashMap allocation.

/// Static type information for a single PostgreSQL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgTypeInfo {
    /// The PostgreSQL OID for this type.
    pub oid: u32,
    /// The canonical PostgreSQL type name (lowercase).
    pub pg_name: &'static str,
    /// The Rust type used when reading this column from a query result.
    pub rust_output_type: &'static str,
    /// The Rust type used when passing this value as a query parameter (`ToSql`).
    pub rust_param_type: &'static str,
}

/// Static table of all supported PostgreSQL types, ordered by OID for readability.
/// Lookups are O(n) linear scans; the table is small enough that this is fine.
static PG_TYPES: &[PgTypeInfo] = &[
    PgTypeInfo {
        oid: 16,
        pg_name: "bool",
        rust_output_type: "bool",
        rust_param_type: "bool",
    },
    PgTypeInfo {
        oid: 17,
        pg_name: "bytea",
        rust_output_type: "Vec<u8>",
        rust_param_type: "Vec<u8>",
    },
    PgTypeInfo {
        oid: 19,
        pg_name: "name",
        rust_output_type: "String",
        rust_param_type: "String",
    },
    PgTypeInfo {
        oid: 20,
        pg_name: "int8",
        rust_output_type: "i64",
        rust_param_type: "i64",
    },
    PgTypeInfo {
        oid: 21,
        pg_name: "int2",
        rust_output_type: "i16",
        rust_param_type: "i16",
    },
    PgTypeInfo {
        oid: 23,
        pg_name: "int4",
        rust_output_type: "i32",
        rust_param_type: "i32",
    },
    PgTypeInfo {
        oid: 25,
        pg_name: "text",
        rust_output_type: "String",
        rust_param_type: "String",
    },
    PgTypeInfo {
        oid: 26,
        pg_name: "oid",
        rust_output_type: "u32",
        rust_param_type: "u32",
    },
    PgTypeInfo {
        oid: 114,
        pg_name: "json",
        rust_output_type: "serde_json::Value",
        rust_param_type: "serde_json::Value",
    },
    PgTypeInfo {
        oid: 700,
        pg_name: "float4",
        rust_output_type: "f32",
        rust_param_type: "f32",
    },
    PgTypeInfo {
        oid: 701,
        pg_name: "float8",
        rust_output_type: "f64",
        rust_param_type: "f64",
    },
    PgTypeInfo {
        oid: 1007,
        pg_name: "int4array",
        rust_output_type: "Vec<i32>",
        rust_param_type: "Vec<i32>",
    },
    PgTypeInfo {
        oid: 1009,
        pg_name: "textarray",
        rust_output_type: "Vec<String>",
        rust_param_type: "Vec<String>",
    },
    PgTypeInfo {
        oid: 1016,
        pg_name: "int8array",
        rust_output_type: "Vec<i64>",
        rust_param_type: "Vec<i64>",
    },
    PgTypeInfo {
        oid: 1042,
        pg_name: "bpchar",
        rust_output_type: "String",
        rust_param_type: "String",
    },
    PgTypeInfo {
        oid: 1043,
        pg_name: "varchar",
        rust_output_type: "String",
        rust_param_type: "String",
    },
    PgTypeInfo {
        oid: 1082,
        pg_name: "date",
        rust_output_type: "chrono::NaiveDate",
        rust_param_type: "chrono::NaiveDate",
    },
    PgTypeInfo {
        oid: 1114,
        pg_name: "timestamp",
        rust_output_type: "chrono::NaiveDateTime",
        rust_param_type: "chrono::NaiveDateTime",
    },
    PgTypeInfo {
        oid: 1184,
        pg_name: "timestamptz",
        rust_output_type: "chrono::DateTime<chrono::Utc>",
        rust_param_type: "chrono::DateTime<chrono::Utc>",
    },
    PgTypeInfo {
        oid: 2950,
        pg_name: "uuid",
        rust_output_type: "uuid::Uuid",
        rust_param_type: "uuid::Uuid",
    },
    PgTypeInfo {
        oid: 3802,
        pg_name: "jsonb",
        rust_output_type: "serde_json::Value",
        rust_param_type: "serde_json::Value",
    },
];

/// Returns type information for the given PostgreSQL OID, or `None` if the
/// OID is not in the supported set.
pub fn from_oid(oid: u32) -> Option<&'static PgTypeInfo> {
    PG_TYPES.iter().find(|t| t.oid == oid)
}

/// Returns type information for the given PostgreSQL type name (case-insensitive),
/// or `None` if the name is not in the supported set.
pub fn from_name(name: &str) -> Option<&'static PgTypeInfo> {
    let lower = name.to_ascii_lowercase();
    PG_TYPES.iter().find(|t| t.pg_name == lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_oid_bool() {
        let info = from_oid(16).expect("bool OID 16 must be present");
        assert_eq!(info.pg_name, "bool");
        assert_eq!(info.rust_output_type, "bool");
        assert_eq!(info.rust_param_type, "bool");
    }

    #[test]
    fn from_oid_int4() {
        let info = from_oid(23).expect("int4 OID 23 must be present");
        assert_eq!(info.pg_name, "int4");
        assert_eq!(info.rust_output_type, "i32");
    }

    #[test]
    fn from_oid_int8() {
        let info = from_oid(20).expect("int8 OID 20 must be present");
        assert_eq!(info.rust_output_type, "i64");
    }

    #[test]
    fn from_oid_int2() {
        let info = from_oid(21).expect("int2 OID 21 must be present");
        assert_eq!(info.rust_output_type, "i16");
    }

    #[test]
    fn from_oid_float4() {
        let info = from_oid(700).expect("float4 OID 700 must be present");
        assert_eq!(info.rust_output_type, "f32");
    }

    #[test]
    fn from_oid_float8() {
        let info = from_oid(701).expect("float8 OID 701 must be present");
        assert_eq!(info.rust_output_type, "f64");
    }

    #[test]
    fn from_oid_text() {
        let info = from_oid(25).expect("text OID 25 must be present");
        assert_eq!(info.rust_output_type, "String");
    }

    #[test]
    fn from_oid_varchar() {
        let info = from_oid(1043).expect("varchar OID 1043 must be present");
        assert_eq!(info.pg_name, "varchar");
        assert_eq!(info.rust_output_type, "String");
    }

    #[test]
    fn from_oid_bpchar() {
        let info = from_oid(1042).expect("bpchar OID 1042 must be present");
        assert_eq!(info.pg_name, "bpchar");
        assert_eq!(info.rust_output_type, "String");
    }

    #[test]
    fn from_oid_name() {
        let info = from_oid(19).expect("name OID 19 must be present");
        assert_eq!(info.rust_output_type, "String");
    }

    #[test]
    fn from_oid_bytea() {
        let info = from_oid(17).expect("bytea OID 17 must be present");
        assert_eq!(info.rust_output_type, "Vec<u8>");
    }

    #[test]
    fn from_oid_timestamptz() {
        let info = from_oid(1184).expect("timestamptz OID 1184 must be present");
        assert_eq!(info.rust_output_type, "chrono::DateTime<chrono::Utc>");
    }

    #[test]
    fn from_oid_timestamp() {
        let info = from_oid(1114).expect("timestamp OID 1114 must be present");
        assert_eq!(info.rust_output_type, "chrono::NaiveDateTime");
    }

    #[test]
    fn from_oid_date() {
        let info = from_oid(1082).expect("date OID 1082 must be present");
        assert_eq!(info.rust_output_type, "chrono::NaiveDate");
    }

    #[test]
    fn from_oid_uuid() {
        let info = from_oid(2950).expect("uuid OID 2950 must be present");
        assert_eq!(info.rust_output_type, "uuid::Uuid");
    }

    #[test]
    fn from_oid_jsonb() {
        let info = from_oid(3802).expect("jsonb OID 3802 must be present");
        assert_eq!(info.rust_output_type, "serde_json::Value");
    }

    #[test]
    fn from_oid_json() {
        let info = from_oid(114).expect("json OID 114 must be present");
        assert_eq!(info.rust_output_type, "serde_json::Value");
    }

    #[test]
    fn from_oid_oid_type() {
        let info = from_oid(26).expect("oid OID 26 must be present");
        assert_eq!(info.rust_output_type, "u32");
    }

    #[test]
    fn from_oid_int4array() {
        let info = from_oid(1007).expect("int4array OID 1007 must be present");
        assert_eq!(info.rust_output_type, "Vec<i32>");
    }

    #[test]
    fn from_oid_int8array() {
        let info = from_oid(1016).expect("int8array OID 1016 must be present");
        assert_eq!(info.rust_output_type, "Vec<i64>");
    }

    #[test]
    fn from_oid_textarray() {
        let info = from_oid(1009).expect("textarray OID 1009 must be present");
        assert_eq!(info.rust_output_type, "Vec<String>");
    }

    #[test]
    fn from_oid_unknown_returns_none() {
        assert!(from_oid(99999).is_none());
        assert!(from_oid(0).is_none());
    }

    #[test]
    fn from_name_case_insensitive() {
        let lower = from_name("bool").expect("'bool' must be found");
        let upper = from_name("BOOL").expect("'BOOL' must be found");
        let mixed = from_name("Bool").expect("'Bool' must be found");
        assert_eq!(lower.oid, upper.oid);
        assert_eq!(lower.oid, mixed.oid);
        assert_eq!(lower.oid, 16);
    }

    #[test]
    fn from_name_text() {
        let info = from_name("text").expect("'text' must be found");
        assert_eq!(info.oid, 25);
    }

    #[test]
    fn from_name_timestamptz() {
        let info = from_name("timestamptz").expect("'timestamptz' must be found");
        assert_eq!(info.oid, 1184);
    }

    #[test]
    fn from_name_unknown_returns_none() {
        assert!(from_name("notareal_type").is_none());
        assert!(from_name("").is_none());
    }
}

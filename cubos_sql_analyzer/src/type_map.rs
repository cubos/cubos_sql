//! Maps PostgreSQL type OIDs and names to Rust types.
//!
//! This module is used by the proc macro at compile time to determine the Rust
//! types for query parameters and output columns. Users do not interact with
//! this module directly, but the mapping table below shows which PostgreSQL
//! types are supported and what Rust types they map to.
//!
//! # Supported type mappings
//!
//! | PostgreSQL type | OID   | Rust type                       |
//! |-----------------|-------|---------------------------------|
//! | `bool`          | 16    | `bool`                          |
//! | `bytea`         | 17    | `Vec<u8>`                       |
//! | `name`          | 19    | `String`                        |
//! | `int8`          | 20    | `i64`                           |
//! | `int2`          | 21    | `i16`                           |
//! | `int4`          | 23    | `i32`                           |
//! | `text`          | 25    | `String`                        |
//! | `oid`           | 26    | `u32`                           |
//! | `xid`           | 28    | `u32`                           |
//! | `json`          | 114   | `serde_json::Value`             |
//! | `cidr`          | 650   | `String`                        |
//! | `float4`        | 700   | `f32`                           |
//! | `float8`        | 701   | `f64`                           |
//! | `macaddr`       | 829   | `String`                        |
//! | `inet`          | 869   | `String`                        |
//! | `char` (bpchar) | 1042  | `String`                        |
//! | `varchar`       | 1043  | `String`                        |
//! | `date`          | 1082  | `chrono::NaiveDate`             |
//! | `time`          | 1083  | `chrono::NaiveTime`             |
//! | `timestamp`     | 1114  | `chrono::NaiveDateTime`         |
//! | `timestamptz`   | 1184  | `chrono::DateTime<chrono::Utc>` |
//! | `interval`      | 1186  | `String`                        |
//! | `timetz`        | 1266  | `String`                        |
//! | `anyelement`    | 2276  | `String`                        |
//! | `anyarray`      | 2277  | `String`                        |
//! | `regproc`       | 2202  | `u32`                           |
//! | `regprocedure`  | 2203  | `u32`                           |
//! | `regoper`       | 2204  | `u32`                           |
//! | `regoperator`   | 2205  | `u32`                           |
//! | `regclass`      | 2206  | `u32`                           |
//! | `regtype`       | 2207  | `u32`                           |
//! | `uuid`          | 2950  | `uuid::Uuid`                    |
//! | `pg_lsn`        | 3220  | `String`                        |
//! | `pg_ndistinct`  | 3361  | `String`                        |
//! | `pg_dependencies` | 3402 | `String`                       |
//! | `pg_mcv_list`   | 5017  | `String`                        |
//! | `jsonb`         | 3802  | `serde_json::Value`             |
//! | `xid8`          | 5069  | `u64`                           |
//! | `regnamespace`  | 4089  | `u32`                           |
//! | `regrole`       | 4096  | `u32`                           |
//! | `regcollation`  | 4191  | `u32`                           |
//! | `regconfig`     | 4194  | `u32`                           |
//! | `regdictionary` | 4195  | `u32`                           |
//!
//! Array types (e.g. `int4[]`, `text[]`, `uuid[]`) are resolved generically
//! by the proc macro via `Kind::Array` — each element is mapped to the
//! corresponding scalar type above and wrapped in `Vec<T>`.
//!
//! Columns with `CREATE DOMAIN ... AS JSONB` are handled separately via the
//! [domain mapping configuration](crate::config) and are deserialized into
//! user-defined Rust structs instead of `serde_json::Value`.
//!
//! Custom types can be mapped via `[package.metadata.cubos_sql.types]` in
//! `Cargo.toml` — see the [configuration docs](crate::config).

/// Well-known OIDs for builtin PostgreSQL types, keyed by pg_catalog name.
pub(crate) mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const NUMERIC: u32 = 1700;
}

/// Static type information for a single PostgreSQL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgTypeInfo {
    /// The PostgreSQL OID for this type.
    pub oid: u32,
    /// The canonical PostgreSQL type name (lowercase).
    pub pg_name: &'static str,
    /// The Rust type string used for both output (FromSql) and parameters (ToSql).
    pub rust_type: &'static str,
}

/// Static table of all supported PostgreSQL types, ordered by OID for readability.
/// Lookups are O(n) linear scans; the table is small enough that this is fine.
static PG_TYPES: &[PgTypeInfo] = &[
    PgTypeInfo {
        oid: 16,
        pg_name: "bool",
        rust_type: "bool",
    },
    PgTypeInfo {
        oid: 17,
        pg_name: "bytea",
        rust_type: "Vec<u8>",
    },
    // Internal single-byte `"char"` — distinct from SQL's `char(n)` (bpchar).
    // Wire representation is a single i8; we expose it as `String` so
    // downstream users don't need a custom newtype.
    PgTypeInfo {
        oid: 18,
        pg_name: "char",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 19,
        pg_name: "name",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 20,
        pg_name: "int8",
        rust_type: "i64",
    },
    // Transaction IDs — 32-bit unsigned counters; tokio-postgres exposes them
    // as `u32` (same wire format as `oid`).
    PgTypeInfo {
        oid: 28,
        pg_name: "xid",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 5069,
        pg_name: "xid8",
        rust_type: "u64",
    },
    PgTypeInfo {
        oid: 21,
        pg_name: "int2",
        rust_type: "i16",
    },
    PgTypeInfo {
        oid: 23,
        pg_name: "int4",
        rust_type: "i32",
    },
    PgTypeInfo {
        oid: 25,
        pg_name: "text",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 26,
        pg_name: "oid",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 114,
        pg_name: "json",
        rust_type: "::serde_json::Value",
    },
    // IP address / CIDR — we store the textual form (`inet_ntoa` equivalent).
    // Clients that need structured access can re-parse into `std::net::IpAddr`
    // themselves; mapping to `String` keeps the default path zero-dependency.
    PgTypeInfo {
        oid: 650,
        pg_name: "cidr",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 700,
        pg_name: "float4",
        rust_type: "f32",
    },
    PgTypeInfo {
        oid: 701,
        pg_name: "float8",
        rust_type: "f64",
    },
    PgTypeInfo {
        oid: 829,
        pg_name: "macaddr",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 869,
        pg_name: "inet",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 1042,
        pg_name: "bpchar",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 1043,
        pg_name: "varchar",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 1082,
        pg_name: "date",
        rust_type: "::chrono::NaiveDate",
    },
    PgTypeInfo {
        oid: 1083,
        pg_name: "time",
        rust_type: "::chrono::NaiveTime",
    },
    PgTypeInfo {
        oid: 1114,
        pg_name: "timestamp",
        rust_type: "::chrono::NaiveDateTime",
    },
    PgTypeInfo {
        oid: 1184,
        pg_name: "timestamptz",
        rust_type: "::chrono::DateTime<::chrono::Utc>",
    },
    // PG `interval` doesn't cleanly map to `chrono::Duration` (PG separates
    // months, days and microseconds). Expose the textual form — downstream
    // code can parse it via crates like `postgres-interval` if needed.
    PgTypeInfo {
        oid: 1186,
        pg_name: "interval",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 1266,
        pg_name: "timetz",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 1700,
        pg_name: "numeric",
        rust_type: "::rust_decimal::Decimal",
    },
    // Object identifier type aliases (`reg*`). Each is a `u32` on the wire —
    // PG formats it as a human-readable name at display time, but the raw OID
    // is what gets read/written by clients. Mapping them all to `u32` matches
    // `tokio-postgres`/`postgres` behavior (they expose `regclass` etc. as
    // `Oid = u32`).
    PgTypeInfo {
        oid: 2202,
        pg_name: "regproc",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 2203,
        pg_name: "regprocedure",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 2204,
        pg_name: "regoper",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 2205,
        pg_name: "regoperator",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 2206,
        pg_name: "regclass",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 2207,
        pg_name: "regtype",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 4089,
        pg_name: "regnamespace",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 4096,
        pg_name: "regrole",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 4191,
        pg_name: "regcollation",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 4194,
        pg_name: "regconfig",
        rust_type: "u32",
    },
    PgTypeInfo {
        oid: 4195,
        pg_name: "regdictionary",
        rust_type: "u32",
    },
    // `anyarray` / `anyelement` are pseudo-types used in polymorphic function
    // signatures. They occasionally leak into `pg_catalog.pg_stats` columns
    // like `most_common_vals`. We can't know the element type statically, so
    // surface the raw textual representation PG would otherwise print.
    PgTypeInfo {
        oid: 2276,
        pg_name: "anyelement",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 2277,
        pg_name: "anyarray",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 2950,
        pg_name: "uuid",
        rust_type: "::uuid::Uuid",
    },
    PgTypeInfo {
        oid: 3220,
        pg_name: "pg_lsn",
        rust_type: "String",
    },
    // Extended-statistics payloads — opaque internal types that PG renders
    // as text when asked. Surfaced as `String` so pg_statistic_ext views
    // (pg_stats_ext, pg_stats_ext_exprs) can project them verbatim.
    PgTypeInfo {
        oid: 3361,
        pg_name: "pg_ndistinct",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 3402,
        pg_name: "pg_dependencies",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 5017,
        pg_name: "pg_mcv_list",
        rust_type: "String",
    },
    PgTypeInfo {
        oid: 3802,
        pg_name: "jsonb",
        rust_type: "::serde_json::Value",
    },
    // Array types are resolved generically by the proc macro via Kind::Array.
];

/// Returns type information for the given PostgreSQL OID.
///
/// Returns `None` if the OID does not correspond to a supported type.
/// See the [module-level documentation](self) for the full list of supported
/// OIDs.
pub fn from_oid(oid: u32) -> Option<&'static PgTypeInfo> {
    PG_TYPES.iter().find(|t| t.oid == oid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_oid_bool() {
        let info = from_oid(16).expect("bool OID 16 must be present");
        assert_eq!(info.pg_name, "bool");
        assert_eq!(info.rust_type, "bool");
    }

    #[test]
    fn from_oid_int4() {
        let info = from_oid(23).expect("int4 OID 23 must be present");
        assert_eq!(info.pg_name, "int4");
        assert_eq!(info.rust_type, "i32");
    }

    #[test]
    fn from_oid_int8() {
        let info = from_oid(20).expect("int8 OID 20 must be present");
        assert_eq!(info.rust_type, "i64");
    }

    #[test]
    fn from_oid_int2() {
        let info = from_oid(21).expect("int2 OID 21 must be present");
        assert_eq!(info.rust_type, "i16");
    }

    #[test]
    fn from_oid_float4() {
        let info = from_oid(700).expect("float4 OID 700 must be present");
        assert_eq!(info.rust_type, "f32");
    }

    #[test]
    fn from_oid_float8() {
        let info = from_oid(701).expect("float8 OID 701 must be present");
        assert_eq!(info.rust_type, "f64");
    }

    #[test]
    fn from_oid_text() {
        let info = from_oid(25).expect("text OID 25 must be present");
        assert_eq!(info.rust_type, "String");
    }

    #[test]
    fn from_oid_varchar() {
        let info = from_oid(1043).expect("varchar OID 1043 must be present");
        assert_eq!(info.pg_name, "varchar");
        assert_eq!(info.rust_type, "String");
    }

    #[test]
    fn from_oid_bpchar() {
        let info = from_oid(1042).expect("bpchar OID 1042 must be present");
        assert_eq!(info.pg_name, "bpchar");
        assert_eq!(info.rust_type, "String");
    }

    #[test]
    fn from_oid_name() {
        let info = from_oid(19).expect("name OID 19 must be present");
        assert_eq!(info.rust_type, "String");
    }

    #[test]
    fn from_oid_bytea() {
        let info = from_oid(17).expect("bytea OID 17 must be present");
        assert_eq!(info.rust_type, "Vec<u8>");
    }

    #[test]
    fn from_oid_timestamptz() {
        let info = from_oid(1184).expect("timestamptz OID 1184 must be present");
        assert_eq!(info.rust_type, "::chrono::DateTime<::chrono::Utc>");
    }

    #[test]
    fn from_oid_timestamp() {
        let info = from_oid(1114).expect("timestamp OID 1114 must be present");
        assert_eq!(info.rust_type, "::chrono::NaiveDateTime");
    }

    #[test]
    fn from_oid_date() {
        let info = from_oid(1082).expect("date OID 1082 must be present");
        assert_eq!(info.rust_type, "::chrono::NaiveDate");
    }

    #[test]
    fn from_oid_uuid() {
        let info = from_oid(2950).expect("uuid OID 2950 must be present");
        assert_eq!(info.rust_type, "::uuid::Uuid");
    }

    #[test]
    fn from_oid_jsonb() {
        let info = from_oid(3802).expect("jsonb OID 3802 must be present");
        assert_eq!(info.rust_type, "::serde_json::Value");
    }

    #[test]
    fn from_oid_json() {
        let info = from_oid(114).expect("json OID 114 must be present");
        assert_eq!(info.rust_type, "::serde_json::Value");
    }

    #[test]
    fn from_oid_oid_type() {
        let info = from_oid(26).expect("oid OID 26 must be present");
        assert_eq!(info.rust_type, "u32");
    }

    #[test]
    fn from_oid_time() {
        let info = from_oid(1083).expect("time OID 1083 must be present");
        assert_eq!(info.rust_type, "::chrono::NaiveTime");
    }

    #[test]
    fn array_oids_not_in_static_table() {
        // Array types are resolved generically by Kind::Array in the proc macro,
        // so they should NOT be in the static type_map.
        assert!(
            from_oid(1000).is_none(),
            "_bool array should not be in static table"
        );
        assert!(
            from_oid(1007).is_none(),
            "_int4 array should not be in static table"
        );
        assert!(
            from_oid(1009).is_none(),
            "_text array should not be in static table"
        );
        assert!(
            from_oid(2951).is_none(),
            "_uuid array should not be in static table"
        );
        assert!(
            from_oid(3807).is_none(),
            "_jsonb array should not be in static table"
        );
    }

    #[test]
    fn from_oid_unknown_returns_none() {
        assert!(from_oid(99999).is_none());
        assert!(from_oid(0).is_none());
    }

    #[test]
    fn from_oid_xid_and_xid8() {
        assert_eq!(from_oid(28).unwrap().rust_type, "u32");
        assert_eq!(from_oid(5069).unwrap().rust_type, "u64");
    }

    #[test]
    fn from_oid_network_types_are_strings() {
        for (oid, name) in [(650, "cidr"), (829, "macaddr"), (869, "inet")] {
            let info = from_oid(oid).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(info.pg_name, name);
            assert_eq!(info.rust_type, "String");
        }
    }

    #[test]
    fn from_oid_interval_and_timetz_are_strings() {
        assert_eq!(from_oid(1186).unwrap().rust_type, "String");
        assert_eq!(from_oid(1266).unwrap().rust_type, "String");
    }

    #[test]
    fn from_oid_pg_lsn_is_string() {
        let info = from_oid(3220).unwrap();
        assert_eq!(info.pg_name, "pg_lsn");
        assert_eq!(info.rust_type, "String");
    }

    #[test]
    fn from_oid_anyelement_and_anyarray_are_strings() {
        // Pseudo-types — leak into catalogs like pg_stats; surface as text.
        assert_eq!(from_oid(2276).unwrap().rust_type, "String");
        assert_eq!(from_oid(2277).unwrap().rust_type, "String");
    }

    #[test]
    fn from_oid_reg_types_are_u32() {
        // All `reg*` object-identifier aliases share the OID u32 representation.
        for (oid, name) in [
            (2202, "regproc"),
            (2203, "regprocedure"),
            (2204, "regoper"),
            (2205, "regoperator"),
            (2206, "regclass"),
            (2207, "regtype"),
            (4089, "regnamespace"),
            (4096, "regrole"),
            (4191, "regcollation"),
            (4194, "regconfig"),
            (4195, "regdictionary"),
        ] {
            let info = from_oid(oid).unwrap_or_else(|| panic!("{name} (oid {oid}) missing"));
            assert_eq!(info.pg_name, name);
            assert_eq!(info.rust_type, "u32");
        }
    }
}

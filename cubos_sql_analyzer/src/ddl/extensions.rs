//! CREATE EXTENSION / ALTER EXTENSION handlers with version tracking.
//!
//! Each extension is declared as a registry entry with a default version,
//! a base install script, and optional upgrade scripts. The interpreter
//! tracks which extensions are installed at which version, enabling
//! ALTER EXTENSION UPDATE to apply the correct upgrade chain.
//!
//! To add support for a new extension:
//! 1. Add `.sql` files to `cubos_sql_analyzer/src/extensions/`
//! 2. Add an `ExtensionDef` entry to the `REGISTRY` array below
//! 3. Run `./update_extensions.sh` to fetch from PG upstream

use pg_query::protobuf::{AlterExtensionStmt, CreateExtensionStmt};

use super::DdlError;
use super::util::ensure_namespace;
use crate::oid::{PgCastOid, PgClassOid, PgExtensionOid, PgGenericOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    DepType, PG_CAST_RELID, PG_EXTENSION_RELID, PG_PROC_RELID, PG_TYPE_RELID, PgCatalog, PgDepend,
    PgExtension,
};

// ─── Extension version graph ────────────────────────────────────────────────

/// A single version of an extension: either a base install or an upgrade.
struct ExtensionVersion {
    /// The version this script installs (e.g. "1.4").
    version: &'static str,
    /// The version this upgrades FROM. `None` = base install.
    from: Option<&'static str>,
    /// The SQL to execute for this version.
    sql: &'static str,
}

/// An extension definition in the registry.
struct ExtensionDef {
    name: &'static str,
    /// The default version installed by `CREATE EXTENSION` with no VERSION clause.
    default_version: &'static str,
    /// All known versions (base installs + upgrades).
    versions: &'static [ExtensionVersion],
}

// ─── Registry ───────────────────────────────────────────────────────────────
//
// All PostgreSQL contrib extensions, alphabetical order.
// SQL files are in cubos_sql_analyzer/src/extensions/ and fetched via
// update_extensions.sh from the PostgreSQL repository.

static REGISTRY: &[ExtensionDef] = &[
    // ── amcheck ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "amcheck",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: include_str!("../extensions/amcheck--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../extensions/amcheck--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/amcheck--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/amcheck--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/amcheck--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/amcheck--1.4--1.5.sql"),
            },
        ],
    },
    // ── autoinc (SPI) ──────────────────────────────────────────────────
    ExtensionDef {
        name: "autoinc",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/autoinc--1.0.sql"),
        }],
    },
    // ── bloom ──────────────────────────────────────────────────────────
    ExtensionDef {
        name: "bloom",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/bloom--1.0.sql"),
        }],
    },
    // ── bool_plperl (requires plperl) ──────────────────────────────────
    ExtensionDef {
        name: "bool_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/bool_plperl--1.0.sql"),
        }],
    },
    // ── bool_plperlu (requires plperlu) ────────────────────────────────
    ExtensionDef {
        name: "bool_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/bool_plperlu--1.0.sql"),
        }],
    },
    // ── btree_gin ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "btree_gin",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: include_str!("../extensions/btree_gin--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../extensions/btree_gin--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/btree_gin--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/btree_gin--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/btree_gin--1.3--1.4.sql"),
            },
        ],
    },
    // ── btree_gist ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "btree_gist",
        default_version: "1.9",
        versions: &[ExtensionVersion {
            version: "1.9",
            from: None,
            sql: include_str!("../extensions/btree_gist--1.9.sql"),
        }],
    },
    // ── citext ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "citext",
        default_version: "1.8",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: include_str!("../extensions/citext--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/citext--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/citext--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../extensions/citext--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../extensions/citext--1.7--1.8.sql"),
            },
        ],
    },
    // ── cube ───────────────────────────────────────────────────────────
    ExtensionDef {
        name: "cube",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: include_str!("../extensions/cube--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/cube--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/cube--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/cube--1.4--1.5.sql"),
            },
        ],
    },
    // ── dblink ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "dblink",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: include_str!("../extensions/dblink--1.2.sql"),
        }],
    },
    // ── dict_int ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_int",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/dict_int--1.0.sql"),
        }],
    },
    // ── dict_xsyn ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_xsyn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/dict_xsyn--1.0.sql"),
        }],
    },
    // ── earthdistance (depends on cube) ────────────────────────────────
    ExtensionDef {
        name: "earthdistance",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/earthdistance--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/earthdistance--1.1--1.2.sql"),
            },
        ],
    },
    // ── file_fdw ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "file_fdw",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/file_fdw--1.0.sql"),
        }],
    },
    // ── fuzzystrmatch ──────────────────────────────────────────────────
    ExtensionDef {
        name: "fuzzystrmatch",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/fuzzystrmatch--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/fuzzystrmatch--1.1--1.2.sql"),
            },
        ],
    },
    // ── hstore ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "hstore",
        default_version: "1.8",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: include_str!("../extensions/hstore--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/hstore--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/hstore--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../extensions/hstore--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../extensions/hstore--1.7--1.8.sql"),
            },
        ],
    },
    // ── hstore_plperl (requires hstore + plperl) ───────────────────────
    ExtensionDef {
        name: "hstore_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/hstore_plperl--1.0.sql"),
        }],
    },
    // ── hstore_plperlu (requires hstore + plperlu) ─────────────────────
    ExtensionDef {
        name: "hstore_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/hstore_plperlu--1.0.sql"),
        }],
    },
    // ── hstore_plpython3u (requires hstore + plpython3u) ───────────────
    ExtensionDef {
        name: "hstore_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/hstore_plpython3u--1.0.sql"),
        }],
    },
    // ── intagg ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "intagg",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../extensions/intagg--1.1.sql"),
        }],
    },
    // ── insert_username (SPI) ──────────────────────────────────────────
    ExtensionDef {
        name: "insert_username",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/insert_username--1.0.sql"),
        }],
    },
    // ── intarray ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "intarray",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: include_str!("../extensions/intarray--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/intarray--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/intarray--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/intarray--1.4--1.5.sql"),
            },
        ],
    },
    // ── isn ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "isn",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/isn--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/isn--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/isn--1.2--1.3.sql"),
            },
        ],
    },
    // ── jsonb_plperl (requires plperl) ─────────────────────────────────
    ExtensionDef {
        name: "jsonb_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/jsonb_plperl--1.0.sql"),
        }],
    },
    // ── jsonb_plperlu (requires plperlu) ───────────────────────────────
    ExtensionDef {
        name: "jsonb_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/jsonb_plperlu--1.0.sql"),
        }],
    },
    // ── jsonb_plpython3u (requires plpython3u) ─────────────────────────
    ExtensionDef {
        name: "jsonb_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/jsonb_plpython3u--1.0.sql"),
        }],
    },
    // ── lo ─────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "lo",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/lo--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/lo--1.1--1.2.sql"),
            },
        ],
    },
    // ── ltree ──────────────────────────────────────────────────────────
    ExtensionDef {
        name: "ltree",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/ltree--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/ltree--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/ltree--1.2--1.3.sql"),
            },
        ],
    },
    // ── ltree_plpython3u (requires ltree + plpython3u) ─────────────────
    ExtensionDef {
        name: "ltree_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/ltree_plpython3u--1.0.sql"),
        }],
    },
    // ── moddatetime (SPI) ──────────────────────────────────────────────
    ExtensionDef {
        name: "moddatetime",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/moddatetime--1.0.sql"),
        }],
    },
    // ── pageinspect ────────────────────────────────────────────────────
    ExtensionDef {
        name: "pageinspect",
        default_version: "1.13",
        versions: &[
            ExtensionVersion {
                version: "1.5",
                from: None,
                sql: include_str!("../extensions/pageinspect--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/pageinspect--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../extensions/pageinspect--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../extensions/pageinspect--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: include_str!("../extensions/pageinspect--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: include_str!("../extensions/pageinspect--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: include_str!("../extensions/pageinspect--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: include_str!("../extensions/pageinspect--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: include_str!("../extensions/pageinspect--1.12--1.13.sql"),
            },
        ],
    },
    // ── pg_buffercache ─────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_buffercache",
        default_version: "1.7",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: include_str!("../extensions/pg_buffercache--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/pg_buffercache--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/pg_buffercache--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/pg_buffercache--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/pg_buffercache--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../extensions/pg_buffercache--1.6--1.7.sql"),
            },
        ],
    },
    // ── pg_freespacemap ────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_freespacemap",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/pg_freespacemap--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/pg_freespacemap--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/pg_freespacemap--1.2--1.3.sql"),
            },
        ],
    },
    // ── pg_logicalinspect ──────────────────────────────────────────────
    ExtensionDef {
        name: "pg_logicalinspect",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/pg_logicalinspect--1.0.sql"),
        }],
    },
    // ── pg_prewarm ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_prewarm",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/pg_prewarm--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/pg_prewarm--1.1--1.2.sql"),
            },
        ],
    },
    // ── pg_stash_advice ────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_stash_advice",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/pg_stash_advice--1.0.sql"),
        }],
    },
    // ── pg_stat_statements ─────────────────────────────────────────────
    ExtensionDef {
        name: "pg_stat_statements",
        default_version: "1.13",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: include_str!("../extensions/pg_stat_statements--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/pg_stat_statements--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/pg_stat_statements--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../extensions/pg_stat_statements--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../extensions/pg_stat_statements--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: include_str!("../extensions/pg_stat_statements--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: include_str!("../extensions/pg_stat_statements--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: include_str!("../extensions/pg_stat_statements--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: include_str!("../extensions/pg_stat_statements--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: include_str!("../extensions/pg_stat_statements--1.12--1.13.sql"),
            },
        ],
    },
    // ── pg_surgery ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_surgery",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/pg_surgery--1.0.sql"),
        }],
    },
    // ── pg_trgm ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_trgm",
        default_version: "1.6",
        versions: &[
            ExtensionVersion {
                version: "1.3",
                from: None,
                sql: include_str!("../extensions/pg_trgm--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/pg_trgm--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/pg_trgm--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../extensions/pg_trgm--1.5--1.6.sql"),
            },
        ],
    },
    // ── pg_visibility ──────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_visibility",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/pg_visibility--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/pg_visibility--1.1--1.2.sql"),
            },
        ],
    },
    // ── pg_walinspect ──────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_walinspect",
        default_version: "1.1",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: include_str!("../extensions/pg_walinspect--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../extensions/pg_walinspect--1.0--1.1.sql"),
            },
        ],
    },
    // ── pgcrypto ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgcrypto",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.3",
                from: None,
                sql: include_str!("../extensions/pgcrypto--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/pgcrypto--1.3--1.4.sql"),
            },
        ],
    },
    // ── pgrowlocks ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgrowlocks",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: include_str!("../extensions/pgrowlocks--1.2.sql"),
        }],
    },
    // ── pgstattuple ────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgstattuple",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: include_str!("../extensions/pgstattuple--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../extensions/pgstattuple--1.4--1.5.sql"),
            },
        ],
    },
    // ── postgres_fdw ───────────────────────────────────────────────────
    ExtensionDef {
        name: "postgres_fdw",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: include_str!("../extensions/postgres_fdw--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../extensions/postgres_fdw--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/postgres_fdw--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/postgres_fdw--1.2--1.3.sql"),
            },
        ],
    },
    // ── refint (SPI) ───────────────────────────────────────────────────
    ExtensionDef {
        name: "refint",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/refint--1.0.sql"),
        }],
    },
    // ── seg ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "seg",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/seg--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/seg--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../extensions/seg--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../extensions/seg--1.3--1.4.sql"),
            },
        ],
    },
    // ── sslinfo ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "sslinfo",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: include_str!("../extensions/sslinfo--1.2.sql"),
        }],
    },
    // ── tablefunc ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "tablefunc",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/tablefunc--1.0.sql"),
        }],
    },
    // ── tcn ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "tcn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/tcn--1.0.sql"),
        }],
    },
    // ── tsm_system_rows ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_rows",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/tsm_system_rows--1.0.sql"),
        }],
    },
    // ── tsm_system_time ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_time",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../extensions/tsm_system_time--1.0.sql"),
        }],
    },
    // ── unaccent ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "unaccent",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../extensions/unaccent--1.1.sql"),
        }],
    },
    // ── uuid-ossp ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "uuid-ossp",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../extensions/uuid-ossp--1.1.sql"),
        }],
    },
    // ── xml2 ───────────────────────────────────────────────────────────
    ExtensionDef {
        name: "xml2",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: include_str!("../extensions/xml2--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../extensions/xml2--1.1--1.2.sql"),
            },
        ],
    },
    // ═══════════════════════════════════════════════════════════════════════
    // Third-party extensions
    // ═══════════════════════════════════════════════════════════════════════
    // ── vector (pgvector) ──────────────────────────────────────────────
    ExtensionDef {
        name: "vector",
        default_version: "0.8.2",
        versions: &[ExtensionVersion {
            version: "0.8.2",
            from: None,
            sql: include_str!("../extensions/vector--0.8.2.sql"),
        }],
    },
];

// ─── CREATE EXTENSION ───────────────────────────────────────────────────────

pub fn create_extension(
    interp: &mut PgCatalog,
    stmt: &CreateExtensionStmt,
) -> Result<(), DdlError> {
    let name = &stmt.extname;

    // Check if already installed.
    if interp.extension_by_name.contains_key(name.as_str()) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "extension \"{name}\" already exists"
        )));
    }

    let ext = REGISTRY
        .iter()
        .find(|e| e.name == name.as_str())
        .ok_or_else(|| {
            DdlError::ExtensionError(format!(
                "unknown extension '{name}': add a SQL file to cubos_sql_analyzer/src/extensions/ \
                 to register it for static analysis"
            ))
        })?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());
    let target_schema =
        extract_option(&stmt.options, "schema").unwrap_or_else(|| "public".to_owned());

    // Find the install path: base version, then upgrades to target.
    let path = find_install_path(ext, &target_version)?;

    // Allocate the pg_extension row up front so we can reference its OID
    // when tagging objects created during installation.
    let target_nsoid = ensure_namespace(interp, &target_schema)?;
    let ext_oid = PgExtensionOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_extension(PgExtension {
        oid: ext_oid,
        extname: name.clone(),
        extnamespace: target_nsoid,
        extversion: target_version,
    });

    // Snapshot OIDs before install so the `pg_depend` tagging step can
    // identify the objects the extension created.
    let types_before: std::collections::HashSet<PgTypeOid> =
        interp.pg_type.keys().copied().collect();
    let procs_before: std::collections::HashSet<PgProcOid> =
        interp.pg_proc.keys().copied().collect();
    let casts_before: std::collections::HashSet<PgCastOid> =
        interp.pg_cast.keys().copied().collect();

    apply_with_schema(interp, &target_schema, &path)?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);

    Ok(())
}

// ─── ALTER EXTENSION UPDATE ─────────────────────────────────────────────────

pub fn alter_extension(interp: &mut PgCatalog, stmt: &AlterExtensionStmt) -> Result<(), DdlError> {
    let name = &stmt.extname;

    let ext_oid = *interp
        .extension_by_name
        .get(name.as_str())
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' is not installed")))?;
    let installed_version = interp
        .pg_extension
        .get(&ext_oid)
        .map(|e| e.extversion.clone())
        .unwrap_or_default();
    let installed_nsname = interp
        .pg_extension
        .get(&ext_oid)
        .and_then(|e| interp.namespace_name(e.extnamespace).map(str::to_owned))
        .unwrap_or_else(|| "public".to_owned());

    let ext = REGISTRY
        .iter()
        .find(|e| e.name == name.as_str())
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' not in registry")))?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());

    if installed_version == target_version {
        return Ok(()); // Already at target version.
    }

    let path = find_upgrade_path(ext, &installed_version, &target_version)?;

    // Track new objects created by upgrade scripts via pg_depend tagging.
    let types_before: std::collections::HashSet<PgTypeOid> =
        interp.pg_type.keys().copied().collect();
    let procs_before: std::collections::HashSet<PgProcOid> =
        interp.pg_proc.keys().copied().collect();
    let casts_before: std::collections::HashSet<PgCastOid> =
        interp.pg_cast.keys().copied().collect();

    apply_with_schema(interp, &installed_nsname, &path)?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);

    if let Some(entry) = interp.pg_extension.get_mut(&ext_oid) {
        entry.extversion = target_version;
    }

    Ok(())
}

/// Diff `pg_type`/`pg_proc`/`pg_cast` against the snapshot taken before the
/// extension scripts ran, and add `pg_depend` rows for every newly-created
/// object so that `DROP EXTENSION` can find them.
fn record_extension_membership(
    interp: &mut PgCatalog,
    ext_oid: PgExtensionOid,
    types_before: &std::collections::HashSet<PgTypeOid>,
    procs_before: &std::collections::HashSet<PgProcOid>,
    casts_before: &std::collections::HashSet<PgCastOid>,
) {
    let new_types: Vec<PgTypeOid> = interp
        .pg_type
        .keys()
        .filter(|k| !types_before.contains(k))
        .copied()
        .collect();
    let new_procs: Vec<PgProcOid> = interp
        .pg_proc
        .keys()
        .filter(|k| !procs_before.contains(k))
        .copied()
        .collect();
    let new_casts: Vec<PgCastOid> = interp
        .pg_cast
        .keys()
        .filter(|k| !casts_before.contains(k))
        .copied()
        .collect();

    let ref_oid = PgGenericOid::from_nonzero(ext_oid.into_nonzero());
    let ext_dep = |classid: PgClassOid, objid: PgGenericOid| PgDepend {
        classid,
        objid,
        objsubid: 0,
        refclassid: PG_EXTENSION_RELID,
        refobjid: ref_oid,
        refobjsubid: 0,
        deptype: DepType::Extension,
    };
    for type_oid in new_types {
        let g = PgGenericOid::from_nonzero(type_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_TYPE_RELID, g));
    }
    for proc_oid in new_procs {
        let g = PgGenericOid::from_nonzero(proc_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_PROC_RELID, g));
    }
    for cast_oid in new_casts {
        let g = PgGenericOid::from_nonzero(cast_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_CAST_RELID, g));
    }
}

// ─── Path resolution ────────────────────────────────────────────────────────

/// Find the script chain to install an extension at a target version.
/// Returns: base install script + any upgrades needed to reach target.
fn find_install_path<'a>(ext: &'a ExtensionDef, target: &str) -> Result<Vec<&'a str>, DdlError> {
    // Find the base version (from == None).
    let base = ext
        .versions
        .iter()
        .find(|v| v.from.is_none())
        .ok_or_else(|| {
            DdlError::ExtensionError(format!(
                "extension '{}' has no base install version",
                ext.name
            ))
        })?;

    let mut path = vec![base.sql];
    let mut current = base.version;

    if current == target {
        return Ok(path);
    }

    // Walk upgrade chain.
    for _ in 0..100 {
        if let Some(upgrade) = ext.versions.iter().find(|v| v.from == Some(current)) {
            path.push(upgrade.sql);
            current = upgrade.version;
            if current == target {
                return Ok(path);
            }
        } else {
            break;
        }
    }

    Err(DdlError::ExtensionError(format!(
        "no install path for extension '{}' to version '{target}' (reached '{current}')",
        ext.name,
    )))
}

/// Find the upgrade path from one version to another.
fn find_upgrade_path<'a>(
    ext: &'a ExtensionDef,
    from: &str,
    target: &str,
) -> Result<Vec<&'a str>, DdlError> {
    let mut path = Vec::new();
    let mut current = from;

    for _ in 0..100 {
        if let Some(upgrade) = ext.versions.iter().find(|v| v.from == Some(current)) {
            path.push(upgrade.sql);
            current = upgrade.version;
            if current == target {
                return Ok(path);
            }
        } else {
            break;
        }
    }

    Err(DdlError::ExtensionError(format!(
        "no upgrade path for extension '{}' from '{from}' to '{target}' (reached '{current}')",
        ext.name,
    )))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Apply a list of SQL scripts with a temporary `search_path` prepend so
/// unqualified names in the extension SQL resolve into the target schema.
fn apply_with_schema(
    interp: &mut PgCatalog,
    schema: &str,
    scripts: &[&str],
) -> Result<(), DdlError> {
    let original = interp.search_path.clone();
    let target_oid = ensure_namespace(interp, schema)?;
    if interp.search_path.first().copied() != Some(target_oid) {
        interp.search_path.insert(0, target_oid);
    }

    let mut result = Ok(());
    for sql in scripts {
        if !sql.is_empty() {
            // Bypass the public `apply_sql` so we don't double-mirror to
            // PGlite under `pglite_sanity` — the user-facing `CREATE
            // EXTENSION` already went there once and PGlite handles its
            // own internal scripts. Our embedded scripts also use
            // `MODULE_PATHNAME` placeholders that PGlite would reject.
            result = super::apply_sql_to(interp, sql);
            if result.is_err() {
                break;
            }
        }
    }

    interp.search_path = original;
    result
}

/// Extract a string option from CREATE/ALTER EXTENSION options.
fn extract_option(options: &[pg_query::protobuf::Node], name: &str) -> Option<String> {
    for opt in options {
        if let Some(pg_query::protobuf::node::Node::DefElem(de)) = opt.node.as_ref()
            && de.defname == name
            && let Some(arg) = de.arg.as_deref()
            && let Some(pg_query::protobuf::node::Node::String(s)) = arg.node.as_ref()
        {
            return Some(s.sval.clone());
        }
    }
    None
}

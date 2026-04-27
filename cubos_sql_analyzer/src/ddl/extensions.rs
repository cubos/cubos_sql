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

use super::{DdlError, InstalledExtension};
use crate::pg_catalog::PgCatalog;

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
    if interp.installed_extensions.contains_key(name.as_str()) {
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

    // Snapshot state before install to track what the extension creates.
    let types_before: std::collections::HashSet<u32> = interp.types.keys().copied().collect();
    let funcs_before: std::collections::HashSet<crate::qualified_name::QualifiedName> =
        interp.functions_by_name.keys().cloned().collect();
    let casts_before: std::collections::HashSet<String> = interp.casts.keys().cloned().collect();

    // Apply scripts with schema override.
    apply_with_schema(interp, &target_schema, &path)?;

    // Compute diff: what was created by the extension.
    let type_oids: Vec<u32> = interp
        .types
        .keys()
        .filter(|k| !types_before.contains(k))
        .copied()
        .collect();

    // Tag each newly-created type with the extension name. This lets the
    // Rust type resolver route extension types (e.g. pgvector's `vector`)
    // to crate-specific Rust types without needing a config entry.
    for oid in &type_oids {
        if let Some(te) = interp.types.get_mut(oid) {
            te.extension = Some(name.clone());
        }
    }
    let function_names: Vec<crate::qualified_name::QualifiedName> = interp
        .functions_by_name
        .keys()
        .filter(|k| !funcs_before.contains(*k))
        .cloned()
        .collect();
    let cast_keys: Vec<String> = interp
        .casts
        .keys()
        .filter(|k| !casts_before.contains(k.as_str()))
        .cloned()
        .collect();

    // Record as installed.
    interp.installed_extensions.insert(
        name.clone(),
        InstalledExtension {
            version: target_version,
            schema: target_schema,
            type_oids,
            function_names,
            cast_keys,
        },
    );

    Ok(())
}

// ─── ALTER EXTENSION UPDATE ─────────────────────────────────────────────────

pub fn alter_extension(interp: &mut PgCatalog, stmt: &AlterExtensionStmt) -> Result<(), DdlError> {
    let name = &stmt.extname;

    let installed = interp
        .installed_extensions
        .get(name.as_str())
        .cloned()
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' is not installed")))?;

    let ext = REGISTRY
        .iter()
        .find(|e| e.name == name.as_str())
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' not in registry")))?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());

    if installed.version == target_version {
        return Ok(()); // Already at target version.
    }

    // Find upgrade path from current version to target.
    let path = find_upgrade_path(ext, &installed.version, &target_version)?;

    // Track new objects created by upgrade scripts.
    let funcs_before: std::collections::HashSet<crate::qualified_name::QualifiedName> =
        interp.functions_by_name.keys().cloned().collect();

    apply_with_schema(interp, &installed.schema, &path)?;

    let new_funcs: Vec<crate::qualified_name::QualifiedName> = interp
        .functions_by_name
        .keys()
        .filter(|k| !funcs_before.contains(*k))
        .cloned()
        .collect();

    // Update installed version and track new objects.
    let entry = interp.installed_extensions.get_mut(name.as_str()).unwrap();
    entry.version = target_version;
    entry.function_names.extend(new_funcs);

    Ok(())
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

/// Apply a list of SQL scripts with a temporary search_path override.
fn apply_with_schema(
    interp: &mut PgCatalog,
    schema: &str,
    scripts: &[&str],
) -> Result<(), DdlError> {
    let original = interp.search_path.clone();
    if interp.search_path.first().is_none_or(|s| s != schema) {
        interp.search_path.insert(0, schema.to_owned());
    }

    let mut result = Ok(());
    for sql in scripts {
        if !sql.is_empty() {
            result = interp.apply_sql(sql);
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

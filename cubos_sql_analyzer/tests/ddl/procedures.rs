//! CREATE / DROP PROCEDURE: procedures are stored distinct from functions,
//! cannot be called inside expressions, accept OUT parameters.

use crate::common::*;

// ── CREATE PROCEDURE ────────────────────────────────────────────────────────

#[test]
fn create_procedure_registers_with_is_procedure_flag() {
    let snap = build(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    let public_oid = snap.namespace_oid("public").unwrap();
    let procs: Vec<&PgProc> = snap
        .pg_proc()
        .values()
        .filter(|p| p.pronamespace == public_oid && p.proname == "do_thing")
        .collect();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].prokind, ProKind::Procedure);
}

#[test]
fn procedure_is_not_callable_in_expressions() {
    let db = build_db(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    assert_analyze_err!(
        db.analyze("SELECT do_thing(1)"),
        AnalyzeError::UndefinedFunction(_),
        "do_thing",
    );
}

// ── DROP PROCEDURE vs function of same name ────────────────────────────────

#[test]
fn drop_procedure_does_not_touch_function_of_same_name() {
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION f(x int) RETURNS int AS 'SELECT $1' LANGUAGE SQL;
         CREATE PROCEDURE f(x int) LANGUAGE SQL AS $$ SELECT $1 $$;
         DROP PROCEDURE f(int);",
    )]);

    let public_oid = snap.namespace_oid("public").unwrap();
    let procs: Vec<&PgProc> = snap
        .pg_proc()
        .values()
        .filter(|p| p.pronamespace == public_oid && p.proname == "f")
        .collect();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].prokind, ProKind::Function);
}

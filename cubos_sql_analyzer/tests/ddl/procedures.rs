//! CREATE / DROP PROCEDURE: procedures are stored distinct from functions,
//! cannot be called inside expressions, accept OUT parameters.

use crate::common::*;

// ── CREATE PROCEDURE ────────────────────────────────────────────────────────

#[test]
fn create_procedure_registers_with_is_procedure_flag() {
    // Procedures land in functions_by_name but must be flagged so the
    // resolver does not surface them as callable expressions.
    let snap = build(&[(
        "0001.sql",
        "CREATE PROCEDURE do_thing(x int) LANGUAGE SQL AS $$ SELECT $1 $$;",
    )]);

    let fns = snap
        .functions_by_name()
        .get(&QualifiedName::new("public", "do_thing"))
        .expect("do_thing should be registered");
    assert_eq!(fns.len(), 1);
    assert!(fns[0].is_procedure);
    assert!(!fns[0].is_aggregate);
}

#[test]
fn procedure_is_not_callable_in_expressions() {
    // A `CREATE PROCEDURE` must not be considered a valid expression-level
    // function. Calling it inside a SELECT should fail analysis.
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

    let fns = snap
        .functions_by_name()
        .get(&QualifiedName::new("public", "f"))
        .unwrap();
    assert_eq!(fns.len(), 1);
    assert!(!fns[0].is_procedure);
}

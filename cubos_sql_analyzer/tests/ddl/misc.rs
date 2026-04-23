//! DDL integration miscellany: snapshot JSON roundtrip, multi-statement
//! real-world migrations, DML statements appearing in migration files, and
//! other behaviours that don't fit a single DDL feature.

use crate::common::*;

fn setup() -> Database {
    let mut db = Database::new();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();
    db
}

// ── Snapshot JSON roundtrip ──────────────────────────────────────────────────

#[test]
fn snapshot_roundtrip() {
    let db = setup();
    let snapshot = db.snapshot();

    let json = serde_json::to_string(snapshot).unwrap();
    let restored: SchemaSnapshot = serde_json::from_str(&json).unwrap();

    assert_eq!(snapshot.types.len(), restored.types.len());
    assert_eq!(snapshot.tables.len(), restored.tables.len());
    assert_eq!(
        snapshot.functions_by_name.len(),
        restored.functions_by_name.len()
    );
    assert_eq!(
        snapshot.operators_by_name.len(),
        restored.operators_by_name.len()
    );
    assert_eq!(snapshot.casts.len(), restored.casts.len());

    // Analyze against both databases — results must match exactly.
    let restored_db = Database::from_snapshot(restored);
    let sql = "SELECT id, name FROM users";
    let info1 = db.analyze(sql).unwrap();
    let info2 = restored_db.analyze(sql).unwrap();
    assert_identical(&info1, &info2, "snapshot roundtrip");
}

// ── DML mixed into migration files is silently ignored ────────────────────

#[test]
fn dml_statements_in_migration_are_ignored() {
    // Real-world migrations often intersperse INSERT/UPDATE/DELETE between
    // DDL statements. The analyzer is DDL-only and must skip DML without
    // aborting the migration.
    let snap = build(&[(
        "0001.sql",
        "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO t (name) VALUES ('seed');
         UPDATE t SET name = 'updated' WHERE id = 1;
         DELETE FROM t WHERE id = 999;",
    )]);

    let table = snap.resolve_table(None, "t").unwrap();
    assert_eq!(table.columns.len(), 2);
}

// ── Multi-file real-world migration chain ─────────────────────────────────

#[test]
fn complex_real_world_migration_chain() {
    let snap = build(&[
        (
            "0001.sql",
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";

             CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

             CREATE TABLE organizations (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 name TEXT NOT NULL,
                 slug TEXT NOT NULL UNIQUE,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE users (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
                 email TEXT NOT NULL,
                 name TEXT NOT NULL,
                 role user_role NOT NULL DEFAULT 'viewer',
                 active BOOLEAN NOT NULL DEFAULT true,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 UNIQUE (org_id, email)
             );

             CREATE INDEX idx_users_org_id ON users (org_id);
             CREATE INDEX idx_users_email ON users (email);",
        ),
        (
            "0002.sql",
            "CREATE TABLE projects (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 org_id UUID NOT NULL REFERENCES organizations(id),
                 name TEXT NOT NULL,
                 description TEXT,
                 archived BOOLEAN NOT NULL DEFAULT false,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE TABLE tasks (
                 id UUID NOT NULL DEFAULT uuid_generate_v4() PRIMARY KEY,
                 project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 assigned_to UUID REFERENCES users(id),
                 title TEXT NOT NULL,
                 body TEXT,
                 priority INT NOT NULL DEFAULT 0 CHECK (priority >= 0),
                 completed_at TIMESTAMPTZ,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );

             CREATE VIEW active_tasks AS
                 SELECT t.id, t.title, t.priority, p.name AS project_name,
                        u.name AS assignee_name
                 FROM tasks t
                 JOIN projects p ON p.id = t.project_id
                 LEFT JOIN users u ON u.id = t.assigned_to
                 WHERE t.completed_at IS NULL AND NOT p.archived;",
        ),
        (
            "0003.sql",
            "ALTER TYPE user_role ADD VALUE 'owner' BEFORE 'admin';
             ALTER TABLE users ADD COLUMN last_login_at TIMESTAMPTZ;
             ALTER TABLE projects ADD COLUMN owner_id UUID REFERENCES users(id);",
        ),
    ]);

    // Verify organizations.
    let orgs = snap.resolve_table(None, "organizations").unwrap();
    assert_eq!(orgs.columns.len(), 4);
    let org_id = orgs.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(org_id.not_null);
    assert!(org_id.has_default);

    // Verify users (8 columns after ALTER ADD COLUMN).
    let users = snap.resolve_table(None, "users").unwrap();
    assert_eq!(users.columns.len(), 8);
    let role_col = users.columns.iter().find(|c| c.name == "role").unwrap();
    let role_type = snap.get_type(role_col.type_oid).unwrap();
    assert!(matches!(role_type.kind, TypeKind::Enum { .. }));

    // user_role gained a new label at the top.
    if let TypeKind::Enum { labels } = &role_type.kind {
        assert_eq!(labels, &["owner", "admin", "editor", "viewer"]);
    }

    // Verify the active_tasks view has the right shape.
    let view = snap.resolve_table(None, "active_tasks").unwrap();
    assert_eq!(view.columns.len(), 5);
    assert_eq!(view.columns[0].name, "id");
    assert_eq!(view.columns[1].name, "title");
    assert_eq!(view.columns[3].name, "project_name");
    assert_eq!(view.columns[4].name, "assignee_name");

    // Tasks table.
    let tasks = snap.resolve_table(None, "tasks").unwrap();
    assert_eq!(tasks.columns.len(), 8);
    let priority = tasks.columns.iter().find(|c| c.name == "priority").unwrap();
    assert!(priority.not_null);
    assert!(priority.has_default);

    // Projects with added column.
    let projects = snap.resolve_table(None, "projects").unwrap();
    assert_eq!(projects.columns.len(), 7);
}

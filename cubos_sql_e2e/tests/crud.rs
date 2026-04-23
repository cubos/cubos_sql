mod common;

use cubos_sql::sql;

#[tokio::test]
async fn insert_returning_and_select() {
    let pool = common::setup().await;

    let name = "Alice";
    let email = "alice-crud@example.com";
    let age = 30;
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, age) VALUES ($name, $email, $age) RETURNING id, created_at"
    )
    .fetch_one()
    .await
    .expect("insert");

    assert!(inserted.id > 0);

    let id = inserted.id;
    let found = sql!(
        &pool,
        "SELECT id, name, email, age FROM users WHERE id = $id"
    )
    .fetch_optional()
    .await
    .expect("select")
    .expect("row present");

    assert_eq!(found.name, "Alice");
    assert_eq!(found.email, "alice-crud@example.com");
    assert_eq!(found.age, Some(30));
}

#[tokio::test]
async fn update_and_delete_return_affected_rows() {
    let pool = common::setup().await;

    let email = "bob-crud@example.com";
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('Bob', $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let new_age = 42;
    let updated = sql!(&pool, "UPDATE users SET age = $new_age WHERE id = $id")
        .execute()
        .await
        .expect("update");
    assert_eq!(updated, 1);

    let after = sql!(&pool, "SELECT age FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select after update");
    assert_eq!(after.age, Some(42));

    let deleted = sql!(&pool, "DELETE FROM users WHERE id = $id")
        .execute()
        .await
        .expect("delete");
    assert_eq!(deleted, 1);

    let gone = sql!(&pool, "SELECT id FROM users WHERE id = $id")
        .fetch_optional()
        .await
        .expect("select after delete");
    assert!(gone.is_none());
}

#[tokio::test]
async fn fetch_all_returns_multiple_rows() {
    let pool = common::setup().await;

    for (name, email) in [
        ("Carol", "carol-crud@example.com"),
        ("Dave", "dave-crud@example.com"),
    ] {
        sql!(
            &pool,
            "INSERT INTO users (name, email) VALUES ($name, $email)"
        )
        .execute()
        .await
        .expect("insert");
    }

    let pattern = "%-crud@example.com";
    let rows = sql!(
        &pool,
        "SELECT id, name, email FROM users WHERE email LIKE $pattern ORDER BY email"
    )
    .fetch_all()
    .await
    .expect("select");

    assert!(rows.len() >= 2);
    let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"Carol"));
    assert!(names.contains(&"Dave"));
}

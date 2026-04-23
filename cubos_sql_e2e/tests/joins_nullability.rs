mod common;

use cubos_sql::sql;

#[tokio::test]
async fn inner_join_produces_non_nullable_columns() {
    let pool = common::setup().await;

    let name = "Joiner";
    let email = "joiner-joins@example.com";
    let user = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    let user_id = user.id;
    let title = "First Post";
    sql!(
        &pool,
        "INSERT INTO posts (user_id, title) VALUES ($user_id, $title)"
    )
    .execute()
    .await
    .expect("insert post");

    let rows = sql!(
        &pool,
        "SELECT p.id, p.title, u.name AS author_name
         FROM posts p
         INNER JOIN users u ON u.id = p.user_id
         WHERE p.user_id = $user_id"
    )
    .fetch_all()
    .await
    .expect("join");

    assert_eq!(rows.len(), 1);
    // author_name is NOT NULL in users and INNER JOIN preserves that — must be String, not Option<String>.
    let _non_optional_check: &String = &rows[0].author_name;
    assert_eq!(rows[0].author_name, "Joiner");
    assert_eq!(rows[0].title, "First Post");
}

#[tokio::test]
async fn left_join_with_count_and_zero_posts() {
    let pool = common::setup().await;

    // User without posts.
    let name = "Lonely";
    let email = "lonely-joins@example.com";
    let lonely = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert lonely");

    // User with 3 posts.
    let name = "Prolific";
    let email = "prolific-joins@example.com";
    let prolific = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert prolific");

    for title in ["p1", "p2", "p3"] {
        let user_id = prolific.id;
        let title = title.to_string();
        sql!(
            &pool,
            "INSERT INTO posts (user_id, title) VALUES ($user_id, $title)"
        )
        .execute()
        .await
        .expect("insert post");
    }

    let ids = [lonely.id, prolific.id];
    let rows = sql!(
        &pool,
        "SELECT u.id, u.name, COUNT(p.id) AS post_count
         FROM users u
         LEFT JOIN posts p ON p.user_id = u.id
         WHERE u.id = ANY($ids)
         GROUP BY u.id, u.name
         ORDER BY u.id"
    )
    .fetch_all()
    .await
    .expect("left join");

    assert_eq!(rows.len(), 2);
    // post_count is COUNT(...) which PG declares as non-null bigint — i64 (not Option).
    let _i64_check: i64 = rows[0].post_count;
    let lonely_row = rows.iter().find(|r| r.id == lonely.id).unwrap();
    let prolific_row = rows.iter().find(|r| r.id == prolific.id).unwrap();
    assert_eq!(lonely_row.post_count, 0);
    assert_eq!(prolific_row.post_count, 3);
}

#[tokio::test]
async fn left_join_nullable_columns_become_option() {
    let pool = common::setup().await;

    let name = "Solo";
    let email = "solo-joins@example.com";
    let solo = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert solo");

    // LEFT JOIN where there are no matching posts → p.title must be Option<String>.
    let user_id = solo.id;
    let rows = sql!(
        &pool,
        "SELECT u.id, p.title
         FROM users u
         LEFT JOIN posts p ON p.user_id = u.id
         WHERE u.id = $user_id"
    )
    .fetch_all()
    .await
    .expect("left join");

    assert_eq!(rows.len(), 1);
    let _option_check: &Option<String> = &rows[0].title;
    assert!(rows[0].title.is_none());
}

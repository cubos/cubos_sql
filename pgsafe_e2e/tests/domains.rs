mod common;

use pgsafe::sql;
use pgsafe_e2e::UserPreferences;

#[tokio::test]
async fn domain_jsonb_roundtrip_on_insert_and_select() {
    let pool = common::setup().await;

    let name = "Domain Owner";
    let email = "domain-owner@example.com";
    let preferences = UserPreferences {
        theme: "dark".into(),
        newsletter: true,
        daily_digest_limit: 5,
    };

    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, preferences) VALUES ($name, $email, $preferences) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let row = sql!(&pool, "SELECT id, preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");

    let got: UserPreferences = row.preferences.expect("preferences set");
    assert_eq!(got.theme, "dark");
    assert!(got.newsletter);
    assert_eq!(got.daily_digest_limit, 5);
}

#[tokio::test]
async fn domain_jsonb_is_nullable_when_not_set() {
    let pool = common::setup().await;

    let name = "No Prefs";
    let email = "no-prefs@example.com";
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let row = sql!(&pool, "SELECT preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");
    assert!(row.preferences.is_none());
}

#[tokio::test]
async fn domain_jsonb_update_replaces_value() {
    let pool = common::setup().await;

    let name = "Pref Updater";
    let email = "pref-updater@example.com";
    let preferences = UserPreferences {
        theme: "light".into(),
        newsletter: false,
        daily_digest_limit: 1,
    };
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, preferences) VALUES ($name, $email, $preferences) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let preferences = UserPreferences {
        theme: "solarized".into(),
        newsletter: true,
        daily_digest_limit: 20,
    };
    let updated = sql!(
        &pool,
        "UPDATE users SET preferences = $preferences WHERE id = $id"
    )
    .execute()
    .await
    .expect("update");
    assert_eq!(updated, 1);

    let after = sql!(&pool, "SELECT preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");
    let got = after.preferences.expect("preferences set");
    assert_eq!(got.theme, "solarized");
    assert_eq!(got.daily_digest_limit, 20);
}

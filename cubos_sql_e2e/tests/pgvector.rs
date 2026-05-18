//! End-to-end coverage for the `pgvector` extension against a real Postgres.
//!
//! The shared `common` container runs on the `pgvector/pgvector` image, so
//! migration `0006_vectors.sql` (`CREATE EXTENSION vector` + a `vector(3)`
//! column) applies cleanly. These tests exercise:
//!
//! - inserting and reading back a `vector` column, decoded into
//!   `pgvector::Vector`;
//! - a distance operator (`<->`) whose `$param` the analyzer infers as
//!   `vector`, again mapped to `pgvector::Vector`.

mod common;

use cubos_sql::sql;
use pgvector::Vector;

#[tokio::test]
async fn vector_column_roundtrip() {
    let pool = common::setup().await;

    let label = "origin";
    let embedding = Vector::from(vec![1.0_f32, 2.0, 3.0]);
    sql!(
        &pool,
        "INSERT INTO embeddings (label, embedding) VALUES ($label, $embedding)"
    )
    .execute()
    .await
    .expect("insert embedding");

    let row = sql!(
        &pool,
        "SELECT label, embedding FROM embeddings WHERE label = $label"
    )
    .fetch_one()
    .await
    .expect("select embedding");

    // `embedding` is a NOT NULL `vector` column — it surfaces as a plain
    // `pgvector::Vector`, not an `Option`.
    let _embedding_check: &Vector = &row.embedding;
    assert_eq!(row.label, "origin");
    assert_eq!(row.embedding.as_slice(), &[1.0_f32, 2.0, 3.0]);
}

#[tokio::test]
async fn vector_distance_operator_param_is_vector() {
    let pool = common::setup().await;

    for (label, coords) in [
        ("a", vec![0.0_f32, 0.0, 0.0]),
        ("b", vec![1.0_f32, 0.0, 0.0]),
        ("c", vec![5.0_f32, 5.0, 5.0]),
    ] {
        let embedding = Vector::from(coords);
        sql!(
            &pool,
            "INSERT INTO embeddings (label, embedding) VALUES ($label, $embedding)"
        )
        .execute()
        .await
        .expect("insert embedding");
    }

    // `embedding <-> $query` — the L2-distance operator forces the analyzer
    // to infer `$query` as `vector` (mapped to `pgvector::Vector`); the
    // operator result is `float8` → `f64`. The nearest row to `[1,0,0]` is
    // `b`, at distance 0.
    let query = Vector::from(vec![1.0_f32, 0.0, 0.0]);
    let row = sql!(
        &pool,
        "SELECT label, embedding <-> $query AS distance
         FROM embeddings
         ORDER BY distance
         LIMIT 1"
    )
    .fetch_one()
    .await
    .expect("distance query");

    let _distance_check: f64 = row.distance;
    assert_eq!(row.label, "b");
    assert!(
        row.distance.abs() < 1e-6,
        "expected nearest distance ~0, got {}",
        row.distance
    );
}

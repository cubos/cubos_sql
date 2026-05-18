-- pgvector: the `vector` extension and a table with a fixed-dimension
-- `vector(N)` column. Exercised end-to-end by `tests/pgvector.rs`.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE embeddings (
    id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label     TEXT NOT NULL,
    embedding vector(3) NOT NULL
);

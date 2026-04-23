CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');

ALTER TABLE posts ADD COLUMN status post_status NOT NULL DEFAULT 'draft';

CREATE INDEX idx_posts_status ON posts (status);

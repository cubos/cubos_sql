CREATE DOMAIN user_preferences AS JSONB;

ALTER TABLE users ADD COLUMN preferences user_preferences;

-- Composite (record) types and the unified `[types]` resolution.
--
-- Covers composites directly, composites nested in composites, a composite
-- pointed at a user struct via `[types]`, composite fields whose type is
-- itself customised (enum / JSONB domain), and every base type reached
-- *through* a domain — including a domain over a domain.

CREATE TYPE address AS (
    street TEXT,
    city   TEXT,
    zip    TEXT
);

CREATE TYPE company AS (
    name TEXT,
    hq   address
);

CREATE TYPE geo_point AS (
    x FLOAT8,
    y FLOAT8
);

-- Composite whose fields use customised types: an enum and a JSONB domain.
CREATE TYPE tagged AS (
    label  TEXT,
    status post_status,
    prefs  user_preferences
);

-- Domains layered over the composite / enum / JSONB base types. `address_dom2`
-- and `prefs_dom` are domains over domains, exercising the recursive walk.
CREATE DOMAIN address_dom  AS address;
CREATE DOMAIN address_dom2 AS address_dom;
CREATE DOMAIN status_dom   AS post_status;
CREATE DOMAIN prefs_dom    AS user_preferences;

CREATE TABLE offices (
    id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label TEXT NOT NULL,
    addr  address NOT NULL,
    org   company
);

CREATE TABLE landmarks (
    id       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name     TEXT NOT NULL,
    location geo_point NOT NULL
);

CREATE TABLE tagged_rows (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    t  tagged NOT NULL
);

-- Columns whose declared type is a domain (and a domain-over-domain) over
-- each customised base type.
CREATE TABLE domained (
    id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    addr1  address_dom  NOT NULL,
    addr2  address_dom2 NOT NULL,
    status status_dom,
    prefs  prefs_dom
);

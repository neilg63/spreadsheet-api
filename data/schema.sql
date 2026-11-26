-- =====================================================================
-- Spreadsheet API service: SQLite schema
--
-- Applied once, to a new database. Later changes go in data/migrations
-- as .sql files, applied in name order, each once.
--
-- Callers are *principals*, referenced as "<issuer>/<external id>":
--   guest/<hash>     anonymous demo visitors (IP + user agent, salted)
--   github/<id>, google/<sub>   free accounts from a social login (demo)
--   service/<ref>    tenants forwarded by the primary API (service mode)
--   <app>/<id>       users of an application that signs its own access
--                    tokens (ISSUERS_FILE registers the application)
-- Each principal is on a plan with limits. Credits and costs are kept in
-- the model but unused: there are no paid accounts.
--
--   issuers ─< principals ─┬─ social_users, auth_sessions
--      └─< issuer_keys     ├─< usage_events, credit_ledger
--                          ├─< uploads (source files, purged on schedule)
--                          ├─< api_schemas ─< api_schema_versions ─< mappings
--                          └─< collections ─< datasets ─< dataset_rows
--                                   │  (current_id = live dataset)
--                                   ├── links_to ──> collections   (reserved)
--                                   └─< endpoints                  (reserved)
--
-- Conventions: ids are text (UUIDs, or the names of plans and issuers);
-- times are UTC text, 2026-11-26T21:30:43.310Z, which sorts as time;
-- durations are whole seconds; lists and objects are JSON text; booleans
-- are 0 and 1. Rules that need the caller's plan (quotas, row caps,
-- features) are enforced by the service in src/store.rs, not here.
-- =====================================================================


-- =====================================================================
-- PLANS
-- =====================================================================
CREATE TABLE plans (
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    rank                  INTEGER NOT NULL,          -- 0 anonymous, 1 basic, 2+ paid
    imports_per_hour      INTEGER NOT NULL,
    max_rows_per_import   INTEGER NOT NULL,
    max_collections       INTEGER,                   -- NULL = unlimited
    max_endpoints         INTEGER,
    mapping_window_secs   INTEGER,                   -- time to finish mapping after upload
    source_retention_secs INTEGER NOT NULL,          -- uploaded files are purged after this
    dataset_ttl_secs      INTEGER,                   -- NULL = kept until deleted
    endpoint_ttl_secs     INTEGER,
    keep_revisions        INTEGER NOT NULL DEFAULT 1,
    -- filter, sort, search, custom_endpoints, custom_slug, custom_schema, links,
    -- auto_refresh, charts_maps, private_endpoints
    features              TEXT NOT NULL DEFAULT '[]',
    monthly_credits       INTEGER NOT NULL DEFAULT 0,
    -- credits charged per 1,000 units of each usage metric (0 = free)
    costs                 TEXT NOT NULL DEFAULT '{}'
);


-- =====================================================================
-- ISSUERS  (the part before the slash in a user reference)
--   An issuer vouches for its principals. The built-in ones are the
--   service's own; an application registered in ISSUERS_FILE is one too,
--   and proves itself by signing access tokens with a key listed in
--   issuer_keys.
-- =====================================================================
CREATE TABLE issuers (
    id                 TEXT PRIMARY KEY,
    name               TEXT NOT NULL,
    trust              TEXT NOT NULL CHECK (trust IN ('anonymous', 'social', 'managed', 'system')),
    default_plan       TEXT NOT NULL REFERENCES plans (id),
    principal_ttl_secs INTEGER,                      -- inactive principals are removed after this
    max_token_secs     INTEGER,                      -- longest access token lifetime; NULL = the service default
    enabled            INTEGER NOT NULL DEFAULT 1
);

-- Ed25519 public keys an application signs its access tokens with.
CREATE TABLE issuer_keys (
    issuer     TEXT NOT NULL REFERENCES issuers (id) ON DELETE CASCADE,
    kid        TEXT NOT NULL,                        -- the token header's key id
    public_key TEXT NOT NULL,                        -- PEM, or the 32 raw bytes in base64
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    revoked_at TEXT,
    PRIMARY KEY (issuer, kid)
);

-- Tokens withdrawn before they expire, by their jti claim. Swept once expired.
CREATE TABLE revoked_tokens (
    issuer     TEXT NOT NULL REFERENCES issuers (id) ON DELETE CASCADE,
    jti        TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (issuer, jti)
);


-- =====================================================================
-- PRINCIPALS  (an external identity; the polymorphic "user")
--   id is the whole reference, "google/1098...", so a lookup needs no join.
-- =====================================================================
CREATE TABLE principals (
    id           TEXT PRIMARY KEY,
    issuer       TEXT NOT NULL REFERENCES issuers (id),
    external_id  TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'blocked')),
    plan         TEXT REFERENCES plans (id),         -- NULL = the issuer's default plan
    plan_until   TEXT,                               -- when the override ends
    credits      INTEGER NOT NULL DEFAULT 0,
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at   TEXT,                               -- issuers with a principal_ttl_secs only
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (issuer, external_id)
);
CREATE INDEX principals_expiry ON principals (expires_at) WHERE expires_at IS NOT NULL;

-- The limited profile kept for social logins.
CREATE TABLE social_users (
    principal     TEXT PRIMARY KEY REFERENCES principals (id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    email         TEXT,
    name          TEXT,
    avatar_uri    TEXT,
    locale        TEXT,
    last_login_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX social_users_email ON social_users (email);


-- =====================================================================
-- USAGE & CREDITS
-- =====================================================================

-- Append-only usage: one row per import, and one per finished import with qty = rows.
CREATE TABLE usage_events (
    id        INTEGER PRIMARY KEY,
    principal TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    metric    TEXT NOT NULL CHECK (metric IN ('import', 'rows_imported', 'api_request', 'storage_mb_day')),
    qty       INTEGER NOT NULL DEFAULT 1,
    ref       TEXT,                                  -- the dataset
    ip        TEXT,                                  -- for anonymous rate limits
    at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX usage_events_principal ON usage_events (principal, metric, at);
CREATE INDEX usage_events_ip ON usage_events (ip, metric, at);

-- Append-only credit ledger (unused: there are no paid accounts).
CREATE TABLE credit_ledger (
    id        INTEGER PRIMARY KEY,
    principal TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    delta     INTEGER NOT NULL,                      -- + grant/purchase, - usage
    reason    TEXT NOT NULL CHECK (reason IN ('monthly_grant', 'purchase', 'usage', 'refund', 'adjustment')),
    metric    TEXT,
    ref       TEXT,
    note      TEXT,
    at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX credit_ledger_principal ON credit_ledger (principal, at);


-- =====================================================================
-- SOURCE FILES
-- =====================================================================
CREATE TABLE uploads (
    id           TEXT PRIMARY KEY,
    owner        TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    source_kind  TEXT NOT NULL DEFAULT 'file' CHECK (source_kind IN ('file', 'url', 'transfer')),
    filename     TEXT NOT NULL,
    storage_key  TEXT NOT NULL,                      -- file name in UPLOAD_DIR
    content_type TEXT,
    size_bytes   INTEGER NOT NULL,
    sheets       TEXT NOT NULL DEFAULT '[]',
    purge_at     TEXT,                               -- from the owner's plan, on create
    purged_at    TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX uploads_purge ON uploads (purge_at) WHERE purged_at IS NULL;
CREATE INDEX uploads_owner ON uploads (owner);


-- =====================================================================
-- SCHEMAS & MAPPINGS
-- =====================================================================
CREATE TABLE api_schemas (
    id          TEXT PRIMARY KEY,
    owner       TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    description TEXT,
    inferred    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (owner, name)
);

-- A version stays a draft until its first import publishes it; after that its
-- definition and field_spec are not changed.
CREATE TABLE api_schema_versions (
    id         TEXT PRIMARY KEY,
    schema_id  TEXT NOT NULL REFERENCES api_schemas (id) ON DELETE CASCADE,
    version    INTEGER NOT NULL CHECK (version >= 1),
    status     TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published', 'deprecated')),
    definition TEXT NOT NULL,                        -- JSON Schema of one row
    field_spec TEXT NOT NULL DEFAULT '[]',           -- output paths with type and query flags
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (schema_id, version)
);

CREATE TABLE mappings (
    id             TEXT PRIMARY KEY,
    schema_version TEXT NOT NULL REFERENCES api_schema_versions (id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    sheet          TEXT,
    header_row     INTEGER NOT NULL DEFAULT 1,
    columns        TEXT NOT NULL DEFAULT '[]',
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (schema_version, name)
);


-- =====================================================================
-- COLLECTIONS  (a stable, named data source; each import is a dataset)
--   visibility decides who may read the rows:
--     unlisted  anyone who has a dataset's id (the default)
--     public    the same, and free to be listed
--     private   the owner, or an access token that grants read
-- =====================================================================
CREATE TABLE collections (
    id           TEXT PRIMARY KEY,
    owner        TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    schema_id    TEXT REFERENCES api_schemas (id) ON DELETE SET NULL,
    current_id   TEXT REFERENCES datasets (id) ON DELETE SET NULL,
    primary_key  TEXT,                               -- JSON list of paths, e.g. ["country_code"]
    auto_publish INTEGER NOT NULL DEFAULT 1,         -- a finished import becomes current
    visibility   TEXT NOT NULL DEFAULT 'unlisted' CHECK (visibility IN ('unlisted', 'public', 'private')),
    expires_at   TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (owner, name)
);
CREATE INDEX collections_expiry ON collections (expires_at) WHERE expires_at IS NOT NULL;

-- PK/FK between collections (reserved; not served yet).
CREATE TABLE links_to (
    id              TEXT PRIMARY KEY,
    from_collection TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    to_collection   TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    name            TEXT NOT NULL,                   -- key under which linked data appears
    from_path       TEXT NOT NULL,                   -- JSON list of FK paths, in the target's key order
    cardinality     TEXT NOT NULL DEFAULT 'many_to_one' CHECK (cardinality IN ('many_to_one', 'one_to_one')),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (from_collection, name)
);


-- =====================================================================
-- DATASETS  (one import of one worksheet into a collection)
--   status: mapping -> importing -> imported -> superseded, or failed / expired
-- =====================================================================
CREATE TABLE datasets (
    id               TEXT PRIMARY KEY,
    owner            TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    collection_id    TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    upload_id        TEXT REFERENCES uploads (id) ON DELETE SET NULL,
    sheet            TEXT,
    schema_version   TEXT NOT NULL REFERENCES api_schema_versions (id),
    mapping_id       TEXT REFERENCES mappings (id) ON DELETE SET NULL,
    series           INTEGER NOT NULL DEFAULT 1,     -- import number within the collection
    status           TEXT NOT NULL DEFAULT 'mapping'
        CHECK (status IN ('mapping', 'importing', 'imported', 'superseded', 'failed', 'expired')),
    error            TEXT,
    num_rows         INTEGER NOT NULL DEFAULT 0,
    num_invalid      INTEGER NOT NULL DEFAULT 0,
    client_ip        TEXT,                           -- for anonymous rate limits
    mapping_deadline TEXT,
    imported_at      TEXT,
    expires_at       TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (collection_id, series)
);
CREATE INDEX datasets_owner ON datasets (owner, created_at);
CREATE INDEX datasets_expiry ON datasets (expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX datasets_upload ON datasets (upload_id);
CREATE INDEX datasets_version ON datasets (schema_version);


-- ---------------------------------------------------------------------
-- dataset_rows
--   data: the row, as JSONB. Read it with json(data); filter and sort with
--         data ->> '$.price.amount'.
--   pk:   the row's primary-key value(s) joined with '|', when the mapping
--         has a primary_key.
-- ---------------------------------------------------------------------
CREATE TABLE dataset_rows (
    id          INTEGER PRIMARY KEY,
    dataset_id  TEXT NOT NULL REFERENCES datasets (id) ON DELETE CASCADE,
    row_num     INTEGER NOT NULL,
    pk          TEXT,
    valid       INTEGER NOT NULL DEFAULT 1,
    errors      TEXT,                                -- JSON list of {path, message}; NULL when valid
    search_text TEXT,                                -- the searchable fields, for full-text search
    data        BLOB NOT NULL
);
CREATE UNIQUE INDEX dataset_rows_position ON dataset_rows (dataset_id, row_num);
CREATE INDEX dataset_rows_pk ON dataset_rows (dataset_id, pk) WHERE pk IS NOT NULL;

-- Full-text index over search_text. It holds no copy of the text: the rows are its content.
CREATE VIRTUAL TABLE dataset_rows_fts USING fts5 (
    search_text,
    content = 'dataset_rows',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER dataset_rows_fts_insert AFTER INSERT ON dataset_rows WHEN new.search_text IS NOT NULL BEGIN
    INSERT INTO dataset_rows_fts (rowid, search_text) VALUES (new.id, new.search_text);
END;

CREATE TRIGGER dataset_rows_fts_delete AFTER DELETE ON dataset_rows WHEN old.search_text IS NOT NULL BEGIN
    INSERT INTO dataset_rows_fts (dataset_rows_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;


-- =====================================================================
-- ENDPOINTS  (reserved; rows are served at /datasets/{id} for now)
-- =====================================================================
CREATE TABLE endpoints (
    id            TEXT PRIMARY KEY,
    owner         TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    collection_id TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    slug          TEXT NOT NULL UNIQUE,
    kind          TEXT NOT NULL DEFAULT 'temporary' CHECK (kind IN ('temporary', 'custom')),
    status        TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'expired', 'disabled')),
    filterable    TEXT NOT NULL DEFAULT '[]',
    sortable      TEXT NOT NULL DEFAULT '[]',
    output        TEXT NOT NULL DEFAULT 'json' CHECK (output IN ('json', 'csv', 'geojson', 'series')),
    config        TEXT NOT NULL DEFAULT '{}',
    max_page_size INTEGER NOT NULL DEFAULT 100 CHECK (max_page_size BETWEEN 1 AND 1000),
    expires_at    TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);


-- =====================================================================
-- LOGIN SESSIONS  (demo mode)
-- =====================================================================

-- A browser session after a social login. The cookie holds a random token;
-- only its SHA-256 is stored, so the table can't be replayed if it leaks.
CREATE TABLE auth_sessions (
    id           INTEGER PRIMARY KEY,
    principal    TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,
    user_agent   TEXT,
    ip           TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at   TEXT NOT NULL
);
CREATE INDEX auth_sessions_principal ON auth_sessions (principal);

-- A login in progress: OAuth state, PKCE verifier and where to return.
-- Single use; the guest it started from is carried over into the account.
CREATE TABLE auth_states (
    id            TEXT PRIMARY KEY,
    provider      TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    return_to     TEXT NOT NULL,
    guest         TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+10 minutes'))
);

-- What this database has had applied to it.
CREATE TABLE service_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);


-- =====================================================================
-- SEED DATA
--   Two account types in the demo service, plus the service-mode tenant:
--     guest       anonymous visitors, keyed by a salted hash of IP + user agent
--     registered  free accounts from a GitHub or Google login
--     service     tenants forwarded by the primary API, and the default plan
--                 of applications registered in ISSUERS_FILE
--   Limits are starting points; tune them to the service's capacity.
-- =====================================================================
INSERT INTO plans (id, name, rank, imports_per_hour, max_rows_per_import, max_collections, max_endpoints,
                   mapping_window_secs, source_retention_secs, dataset_ttl_secs, endpoint_ttl_secs, keep_revisions, features)
VALUES
    ('guest', 'Guest', 0, 5, 1000, 5, 3, 1800, 1800, 86400, 86400, 1,
     '["filter", "sort"]'),
    ('registered', 'Registered', 1, 30, 20000, 50, 20, 604800, 86400, 7776000, 7776000, 2,
     '["filter", "sort", "search", "custom_endpoints", "custom_slug", "custom_schema", "links"]'),
    ('service', 'Service', 2, 100000, 5000000, NULL, NULL, NULL, 604800, NULL, NULL, 5,
     '["filter", "sort", "search", "custom_endpoints", "custom_slug", "custom_schema", "links", "auto_refresh", "charts_maps", "private_endpoints"]');

INSERT INTO issuers (id, name, trust, default_plan, principal_ttl_secs) VALUES
    ('guest', 'Guest', 'anonymous', 'guest', 86400),
    ('github', 'GitHub', 'social', 'registered', NULL),
    ('google', 'Google', 'social', 'registered', NULL),
    ('service', 'Primary API', 'system', 'service', NULL);

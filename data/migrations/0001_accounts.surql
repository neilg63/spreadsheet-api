-- Guest / registered / service accounts and login sessions (2026-10-15).
-- Applied automatically to databases created from an earlier schema.surql;
-- new databases get all of this from schema.surql and skip it.

UPSERT plans:guest CONTENT {
    name: 'Guest', rank: 0,
    limits: {
        imports_per_hour: 5, max_rows_per_import: 1000,
        max_collections: 5, max_endpoints: 3,
        mapping_window: 30m, source_retention: 30m,
        dataset_ttl: 1d, endpoint_ttl: 1d, keep_revisions: 1
    },
    features: ['filter', 'sort'],
    monthly_credits: 0, costs: {}
};
UPSERT plans:registered CONTENT {
    name: 'Registered', rank: 1,
    limits: {
        imports_per_hour: 30, max_rows_per_import: 20000,
        max_collections: 50, max_endpoints: 20,
        mapping_window: 7d, source_retention: 1d,
        dataset_ttl: 90d, endpoint_ttl: 90d, keep_revisions: 2
    },
    features: ['filter', 'sort', 'search', 'custom_endpoints', 'custom_slug', 'custom_schema', 'links'],
    monthly_credits: 0, costs: {}
};
UPSERT plans:service CONTENT {
    name: 'Service', rank: 2,
    limits: {
        imports_per_hour: 100000, max_rows_per_import: 5000000,
        mapping_window: NONE, source_retention: 7d,
        dataset_ttl: NONE, endpoint_ttl: NONE, keep_revisions: 5
    },
    features: ['filter', 'sort', 'search', 'custom_endpoints', 'custom_slug', 'custom_schema',
               'links', 'auto_refresh', 'charts_maps', 'private_endpoints'],
    monthly_credits: 0, costs: {}
};
UPSERT issuers:guest   CONTENT { name: 'Guest',       trust: 'anonymous', default_plan: plans:guest, principal_ttl: 1d, enabled: true };
UPSERT issuers:github  CONTENT { name: 'GitHub',      trust: 'social',    default_plan: plans:registered, enabled: true };
UPSERT issuers:google  CONTENT { name: 'Google',      trust: 'social',    default_plan: plans:registered, enabled: true };
UPSERT issuers:service CONTENT { name: 'Primary API', trust: 'system',    default_plan: plans:service, enabled: true };

-- The old issuers stay so existing principals keep working, but sign-ins stop.
UPDATE issuers SET enabled = false WHERE id INSIDE [issuers:device, issuers:microsoft, issuers:mainusers];
UPDATE issuers SET default_plan = plans:registered WHERE id INSIDE [issuers:system, issuers:mainusers, issuers:microsoft];
UPDATE issuers:device SET default_plan = plans:guest;
UPDATE principals SET plan = NONE WHERE plan INSIDE [plans:anon, plans:basic, plans:pro];
DELETE plans WHERE id INSIDE [plans:anon, plans:basic, plans:pro];

DEFINE TABLE auth_sessions SCHEMAFULL;
DEFINE FIELD principal    ON auth_sessions TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD token_hash   ON auth_sessions TYPE string;
DEFINE FIELD user_agent   ON auth_sessions TYPE option<string>;
DEFINE FIELD ip           ON auth_sessions TYPE option<string>;
DEFINE FIELD created_at   ON auth_sessions TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD last_seen_at ON auth_sessions TYPE datetime DEFAULT time::now();
DEFINE FIELD expires_at   ON auth_sessions TYPE datetime;
DEFINE INDEX auth_sessions_token ON auth_sessions FIELDS token_hash UNIQUE;
DEFINE INDEX auth_sessions_principal ON auth_sessions FIELDS principal;

DEFINE TABLE auth_states SCHEMAFULL;
DEFINE FIELD provider      ON auth_states TYPE string;
DEFINE FIELD pkce_verifier ON auth_states TYPE string;
DEFINE FIELD return_to     ON auth_states TYPE string;
DEFINE FIELD guest         ON auth_states TYPE option<record<principals>>;
DEFINE FIELD created_at    ON auth_states TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD expires_at    ON auth_states TYPE datetime DEFAULT time::now() + 10m;

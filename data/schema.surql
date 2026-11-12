-- =====================================================================
-- Spreadsheet API service — SurrealDB schema (tested on SurrealDB 3.2.4)
-- For 2.x: `TYPE object FLEXIBLE` -> `FLEXIBLE TYPE object`,
--          `type::record(...)` -> `type::thing(...)`,
--          `FULLTEXT ANALYZER` -> `SEARCH ANALYZER`.
--
-- Callers are *principals*, referenced as "<issuer>/<external id>":
--   guest/<hash>     anonymous demo visitors (IP + user agent, salted)
--   github/<id>, google/<sub>   free accounts from a social login (demo)
--   service/<ref>    tenants forwarded by the primary API (microservice)
-- Each principal is on a plan with limits. Credits and costs are kept in
-- the model but unused: there are no paid accounts.
-- The service connects as a system user and enforces ownership itself.
--
--   issuers ─< principals ─┬─ social_users, auth_sessions
--                          ├─< credit_ledger, usage_events
--                          ├─< uploads (source files, purged on schedule)
--                          ├─< api_schemas ─< api_schema_versions ─< mappings
--                          └─< collections ─< datasets ─< dataset_rows
--                                   │  (current = live dataset)
--                                   ├── links_to ──> collections   (PK/FK)
--                                   └─< endpoints (temporary or custom, TTL by plan)
-- =====================================================================


-- =====================================================================
-- PLANS
-- =====================================================================
DEFINE TABLE plans SCHEMAFULL;

DEFINE FIELD name     ON plans TYPE string;
DEFINE FIELD rank     ON plans TYPE int;   -- 0 anonymous, 1 basic, 2+ paid
DEFINE FIELD limits   ON plans TYPE object;
DEFINE FIELD limits.imports_per_hour     ON plans TYPE int;
DEFINE FIELD limits.max_rows_per_import  ON plans TYPE int;
DEFINE FIELD limits.max_collections      ON plans TYPE option<int>;       -- NONE = unlimited
DEFINE FIELD limits.max_endpoints        ON plans TYPE option<int>;
DEFINE FIELD limits.mapping_window       ON plans TYPE option<duration>;  -- time to finish mapping after upload
DEFINE FIELD limits.source_retention     ON plans TYPE duration;          -- uploaded files are purged after this
DEFINE FIELD limits.dataset_ttl          ON plans TYPE option<duration>;  -- NONE = kept until deleted
DEFINE FIELD limits.endpoint_ttl         ON plans TYPE option<duration>;
DEFINE FIELD limits.keep_revisions       ON plans TYPE int DEFAULT 1;     -- superseded imports kept per collection
DEFINE FIELD features ON plans TYPE array<
    'filter' | 'sort' | 'search' | 'custom_endpoints' | 'custom_slug' | 'custom_schema'
    | 'links' | 'auto_refresh' | 'charts_maps' | 'private_endpoints'
> DEFAULT [];
DEFINE FIELD monthly_credits ON plans TYPE int DEFAULT 0;
-- Credits charged per 1,000 units of each usage metric (0 = free)
DEFINE FIELD costs ON plans TYPE object DEFAULT {};
DEFINE FIELD costs.import         ON plans TYPE int DEFAULT 0;
DEFINE FIELD costs.rows_imported  ON plans TYPE int DEFAULT 0;
DEFINE FIELD costs.api_request    ON plans TYPE int DEFAULT 0;
DEFINE FIELD costs.storage_mb_day ON plans TYPE int DEFAULT 0;


-- =====================================================================
-- ISSUERS  (the part before the slash in a user_ref)
--   issuers:device, issuers:google, issuers:mainusers, issuers:system ...
-- =====================================================================
DEFINE TABLE issuers SCHEMAFULL;

DEFINE FIELD name         ON issuers TYPE string;
DEFINE FIELD trust        ON issuers TYPE 'anonymous' | 'social' | 'managed' | 'system';
DEFINE FIELD default_plan ON issuers TYPE record<plans>;
DEFINE FIELD principal_ttl ON issuers TYPE option<duration>;  -- inactive principals are removed after this
DEFINE FIELD enabled      ON issuers TYPE bool DEFAULT true;


-- =====================================================================
-- PRINCIPALS  (an external identity; the polymorphic "user")
--   Record ID is [issuer, external_id], so "google/1098..." maps to
--   principals:['google', '1098...'] — a key lookup, no join table.
--   First contact: UPSERT principals:['google', $sub] MERGE { ... };
-- =====================================================================
DEFINE TABLE principals SCHEMAFULL;

DEFINE FIELD issuer       ON principals TYPE record<issuers> REFERENCE ON DELETE REJECT;
DEFINE FIELD external_id  ON principals TYPE string;
DEFINE FIELD status       ON principals TYPE 'active' | 'blocked' DEFAULT 'active';
DEFINE FIELD plan         ON principals TYPE option<record<plans>>;   -- NONE = issuer's default plan
DEFINE FIELD plan_until   ON principals TYPE option<datetime>;        -- paid period end, set from UAC/billing
DEFINE FIELD credits      ON principals TYPE int DEFAULT 0;           -- maintained by credit_ledger
DEFINE FIELD last_seen_at ON principals TYPE datetime DEFAULT time::now();
DEFINE FIELD expires_at   ON principals TYPE option<datetime>;        -- anonymous principals only
DEFINE FIELD created_at   ON principals TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at   ON principals TYPE datetime VALUE time::now();

DEFINE INDEX principals_ref ON principals FIELDS issuer, external_id UNIQUE;

-- Anonymous principals expire after a period of inactivity
DEFINE EVENT principals_ttl ON principals
    WHEN $event IN ['CREATE', 'UPDATE'] AND $after.issuer.principal_ttl != NONE
     AND ($before = NONE OR $before.last_seen_at != $after.last_seen_at) THEN {
        UPDATE $after.id SET expires_at = $after.last_seen_at + $after.issuer.principal_ttl;
    };


-- ---------------------------------------------------------------------
-- social_users  (the limited profile kept for social logins)
-- ---------------------------------------------------------------------
DEFINE TABLE social_users SCHEMAFULL;

DEFINE FIELD principal     ON social_users TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD provider      ON social_users TYPE string;             -- 'google', 'microsoft', 'github' ...
DEFINE FIELD email         ON social_users TYPE option<string>;
DEFINE FIELD name          ON social_users TYPE option<string>;
DEFINE FIELD avatar_uri    ON social_users TYPE option<string>;
DEFINE FIELD locale        ON social_users TYPE option<string>;
DEFINE FIELD last_login_at ON social_users TYPE datetime DEFAULT time::now();

DEFINE INDEX social_users_principal ON social_users FIELDS principal UNIQUE;
DEFINE INDEX social_users_email     ON social_users FIELDS email;


-- =====================================================================
-- USAGE & CREDITS
-- =====================================================================

-- Append-only usage. API requests are aggregated by the service
-- (e.g. per endpoint per minute) and written as one row with qty = n.
DEFINE TABLE usage_events SCHEMAFULL;

DEFINE FIELD principal ON usage_events TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD metric    ON usage_events TYPE 'import' | 'rows_imported' | 'api_request' | 'storage_mb_day';
DEFINE FIELD qty       ON usage_events TYPE int DEFAULT 1;
DEFINE FIELD ref       ON usage_events TYPE option<record>;   -- dataset, endpoint ...
DEFINE FIELD ip        ON usage_events TYPE option<string>;   -- for anonymous rate limits
DEFINE FIELD at        ON usage_events TYPE datetime DEFAULT time::now() READONLY;

DEFINE INDEX usage_events_principal ON usage_events FIELDS principal, metric, at;
DEFINE INDEX usage_events_ip        ON usage_events FIELDS ip, metric, at;

-- Price usage in one place: every usage row on a charging plan debits credits
DEFINE EVENT usage_events_charge ON usage_events WHEN $event = 'CREATE' THEN {
    LET $rate = fn::effective_plan($after.principal).costs[$after.metric] ?? 0;
    IF $rate > 0 {
        CREATE credit_ledger SET
            principal = $after.principal,
            delta     = -math::ceil($after.qty * $rate / 1000),
            reason    = 'usage',
            metric    = $after.metric,
            ref       = $after.id;
    };
};

-- Append-only credit ledger; principals.credits is its running sum
DEFINE TABLE credit_ledger SCHEMAFULL;

DEFINE FIELD principal ON credit_ledger TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD delta     ON credit_ledger TYPE int;    -- + grant/purchase, - usage
DEFINE FIELD reason    ON credit_ledger TYPE 'monthly_grant' | 'purchase' | 'usage' | 'refund' | 'adjustment';
DEFINE FIELD metric    ON credit_ledger TYPE option<string>;
DEFINE FIELD ref       ON credit_ledger TYPE option<record>;
DEFINE FIELD note      ON credit_ledger TYPE option<string>;
DEFINE FIELD at        ON credit_ledger TYPE datetime DEFAULT time::now() READONLY;

DEFINE INDEX credit_ledger_principal ON credit_ledger FIELDS principal, at;

-- Deletes are allowed only so entries cascade away with their principal
DEFINE EVENT credit_ledger_apply ON credit_ledger WHEN $event IN ['CREATE', 'UPDATE'] THEN {
    IF $event = 'UPDATE' { THROW 'The credit ledger is append-only; add a correcting entry'; };
    UPDATE $after.principal SET credits += $after.delta;
};


-- =====================================================================
-- SOURCE FILES
-- =====================================================================
DEFINE TABLE uploads SCHEMAFULL;

DEFINE FIELD owner        ON uploads TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD source_kind  ON uploads TYPE 'file' | 'url' | 'transfer' DEFAULT 'file';
DEFINE FIELD filename     ON uploads TYPE string;          -- file name or URI
DEFINE FIELD storage_key  ON uploads TYPE string;          -- object-storage key
DEFINE FIELD content_type ON uploads TYPE option<string>;
DEFINE FIELD size_bytes   ON uploads TYPE int;
DEFINE FIELD sheets       ON uploads TYPE array<string> DEFAULT [];
DEFINE FIELD purge_at     ON uploads TYPE option<datetime>;  -- set from plan on create
DEFINE FIELD purged_at    ON uploads TYPE option<datetime>;
DEFINE FIELD created_at   ON uploads TYPE datetime DEFAULT time::now() READONLY;

DEFINE INDEX uploads_purge ON uploads FIELDS purged_at, purge_at;

DEFINE EVENT uploads_retention ON uploads WHEN $event = 'CREATE' THEN {
    UPDATE $after.id SET purge_at =
        time::now() + fn::effective_plan($after.owner).limits.source_retention;
};


-- =====================================================================
-- SCHEMAS & MAPPINGS
--   Anonymous and basic users get a version inferred from the sheet;
--   plans with 'custom_schema' can supply JSON Schema from API docs.
-- =====================================================================
DEFINE TABLE api_schemas SCHEMAFULL;

DEFINE FIELD owner       ON api_schemas TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD name        ON api_schemas TYPE string;
DEFINE FIELD description ON api_schemas TYPE option<string>;
DEFINE FIELD inferred    ON api_schemas TYPE bool DEFAULT false;
DEFINE FIELD created_at  ON api_schemas TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at  ON api_schemas TYPE datetime VALUE time::now();

DEFINE INDEX api_schemas_owner_name ON api_schemas FIELDS owner, name UNIQUE;


DEFINE TABLE api_schema_versions SCHEMAFULL;

DEFINE FIELD schema       ON api_schema_versions TYPE record<api_schemas> REFERENCE ON DELETE CASCADE;
DEFINE FIELD version      ON api_schema_versions TYPE int ASSERT $value >= 1;
DEFINE FIELD status       ON api_schema_versions TYPE 'draft' | 'published' | 'deprecated' DEFAULT 'draft';
DEFINE FIELD definition   ON api_schema_versions TYPE object FLEXIBLE;   -- JSON Schema
DEFINE FIELD field_spec   ON api_schema_versions TYPE array<object> DEFAULT [];
DEFINE FIELD field_spec.*.path       ON api_schema_versions TYPE string;
DEFINE FIELD field_spec.*.type       ON api_schema_versions
    TYPE 'string' | 'number' | 'integer' | 'boolean' | 'date' | 'datetime' | 'object' | 'array' | 'geo';
DEFINE FIELD field_spec.*.required   ON api_schema_versions TYPE bool DEFAULT false;
DEFINE FIELD field_spec.*.searchable ON api_schema_versions TYPE bool DEFAULT false;
DEFINE FIELD field_spec.*.filterable ON api_schema_versions TYPE bool DEFAULT true;
DEFINE FIELD field_spec.*.sortable   ON api_schema_versions TYPE bool DEFAULT true;
DEFINE FIELD created_at   ON api_schema_versions TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at   ON api_schema_versions TYPE datetime VALUE time::now();

DEFINE INDEX api_schema_versions_unique ON api_schema_versions FIELDS schema, version UNIQUE;

DEFINE EVENT api_schema_versions_frozen ON api_schema_versions
    WHEN $event = 'UPDATE' AND $before.status != 'draft' THEN {
        IF $before.definition != $after.definition OR $before.field_spec != $after.field_spec {
            THROW 'Published schema versions are immutable; create a new version';
        };
    };


DEFINE TABLE mappings SCHEMAFULL;

DEFINE FIELD schema_version ON mappings TYPE record<api_schema_versions> REFERENCE ON DELETE CASCADE;
DEFINE FIELD name           ON mappings TYPE string;
DEFINE FIELD sheet          ON mappings TYPE option<string>;
DEFINE FIELD header_row     ON mappings TYPE int DEFAULT 1;
DEFINE FIELD columns        ON mappings TYPE array<object> DEFAULT [];
DEFINE FIELD columns.*.column    ON mappings TYPE string;
DEFINE FIELD columns.*.path      ON mappings TYPE string;
DEFINE FIELD columns.*.transform ON mappings TYPE option<string>;
DEFINE FIELD columns.*.default   ON mappings TYPE any;
DEFINE FIELD created_at     ON mappings TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at     ON mappings TYPE datetime VALUE time::now();

DEFINE INDEX mappings_version_name ON mappings FIELDS schema_version, name UNIQUE;


-- =====================================================================
-- COLLECTIONS  (a stable, named data source; each import is a dataset)
--   Endpoints and links point at the collection, so a new upload of
--   the same sheet replaces the data behind every endpoint, chart and
--   map automatically.
-- =====================================================================
DEFINE TABLE collections SCHEMAFULL;

DEFINE FIELD owner        ON collections TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD name         ON collections TYPE string;
DEFINE FIELD schema       ON collections TYPE option<record<api_schemas>>;
DEFINE FIELD current      ON collections TYPE option<record<datasets>> REFERENCE ON DELETE UNSET;
DEFINE FIELD primary_key  ON collections TYPE option<array<string>>;  -- JSON paths, e.g. ['country_code']
DEFINE FIELD auto_publish ON collections TYPE bool DEFAULT true;       -- a finished import becomes current
DEFINE FIELD expires_at   ON collections TYPE option<datetime>;
DEFINE FIELD created_at   ON collections TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at   ON collections TYPE datetime VALUE time::now();

DEFINE INDEX collections_owner_name ON collections FIELDS owner, name UNIQUE;

DEFINE EVENT collections_on_create ON collections WHEN $event = 'CREATE' THEN {
    LET $plan = fn::effective_plan($after.owner);
    IF $plan.limits.max_collections != NONE
       AND count(SELECT id FROM collections WHERE owner = $after.owner) > $plan.limits.max_collections {
        THROW 'Collection limit reached for your plan';
    };
    IF $after.primary_key != NONE AND 'links' NOT IN $plan.features {
        THROW 'Keys and links are not included in your plan';
    };
    UPDATE $after.id SET expires_at =
        IF $plan.limits.dataset_ttl != NONE { time::now() + $plan.limits.dataset_ttl };
};


-- ---------------------------------------------------------------------
-- links_to  (PK/FK between collections; plans with 'links')
--   RELATE collections:sales_gb->links_to->collections:countries
--       SET name = 'country', from_path = 'country_code';
--   Resolving a row's parent (uses the dataset_rows_pk index):
--     (SELECT VALUE data FROM ONLY dataset_rows
--        WHERE dataset = $parent.current AND pk = <string>$row.data.country_code LIMIT 1)
-- ---------------------------------------------------------------------
DEFINE TABLE links_to TYPE RELATION IN collections OUT collections ENFORCED SCHEMAFULL;

DEFINE FIELD name        ON links_to TYPE string;        -- key under which linked data appears in output
DEFINE FIELD from_path   ON links_to TYPE array<string>; -- FK paths in the child, same order as parent PK
DEFINE FIELD cardinality ON links_to TYPE 'many_to_one' | 'one_to_one' DEFAULT 'many_to_one';
DEFINE FIELD created_at  ON links_to TYPE datetime DEFAULT time::now() READONLY;

DEFINE INDEX links_to_unique ON links_to FIELDS in, name UNIQUE;

DEFINE EVENT links_to_rules ON links_to WHEN $event IN ['CREATE', 'UPDATE'] THEN {
    IF $after.in.owner != $after.out.owner { THROW 'Both collections must have the same owner'; };
    IF $after.out.primary_key = NONE { THROW 'The target collection has no primary key'; };
    IF array::len($after.from_path) != array::len($after.out.primary_key) {
        THROW 'Foreign key must have as many columns as the target primary key';
    };
    IF 'links' NOT IN fn::effective_plan($after.in.owner).features {
        THROW 'Links are not included in your plan';
    };
};


-- =====================================================================
-- DATASETS  (one import of one worksheet into a collection)
-- =====================================================================
DEFINE TABLE datasets SCHEMAFULL;

DEFINE FIELD owner            ON datasets TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD collection       ON datasets TYPE record<collections> REFERENCE ON DELETE CASCADE;
DEFINE FIELD upload           ON datasets TYPE option<record<uploads>> REFERENCE ON DELETE UNSET;
DEFINE FIELD sheet            ON datasets TYPE option<string>;
DEFINE FIELD schema_version   ON datasets TYPE record<api_schema_versions> REFERENCE ON DELETE REJECT;
DEFINE FIELD mapping          ON datasets TYPE option<record<mappings>> REFERENCE ON DELETE UNSET;
DEFINE FIELD series           ON datasets TYPE int DEFAULT 1;   -- import number within the collection
DEFINE FIELD status           ON datasets
    TYPE 'mapping' | 'importing' | 'imported' | 'superseded' | 'failed' | 'expired' DEFAULT 'mapping';
DEFINE FIELD error            ON datasets TYPE option<string>;
DEFINE FIELD num_rows         ON datasets TYPE int DEFAULT 0;
DEFINE FIELD num_invalid      ON datasets TYPE int DEFAULT 0;
DEFINE FIELD client_ip        ON datasets TYPE option<string>;  -- for anonymous rate limits
DEFINE FIELD mapping_deadline ON datasets TYPE option<datetime>;
DEFINE FIELD imported_at      ON datasets TYPE option<datetime>;
DEFINE FIELD expires_at       ON datasets TYPE option<datetime>;
DEFINE FIELD created_at       ON datasets TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at       ON datasets TYPE datetime VALUE time::now();

DEFINE INDEX datasets_owner      ON datasets FIELDS owner, created_at;
DEFINE INDEX datasets_collection ON datasets FIELDS collection, series UNIQUE;
DEFINE INDEX datasets_expiry     ON datasets FIELDS expires_at;

-- Hourly import quota, plus per-IP quota for anonymous users; set deadlines
DEFINE EVENT datasets_on_create ON datasets WHEN $event = 'CREATE' THEN {
    LET $plan = fn::effective_plan($after.owner);
    IF fn::usage($after.owner, 'import', 1h) >= $plan.limits.imports_per_hour {
        THROW 'Hourly import limit reached for your plan';
    };
    IF $after.owner.issuer.trust = 'anonymous' AND $after.client_ip != NONE
       AND fn::usage_by_ip($after.client_ip, 'import', 1h) >= $plan.limits.imports_per_hour {
        THROW 'Hourly import limit reached';
    };
    CREATE usage_events SET principal = $after.owner, metric = 'import',
        ref = $after.id, ip = $after.client_ip;
    UPDATE $after.id SET
        mapping_deadline = IF $plan.limits.mapping_window != NONE { time::now() + $plan.limits.mapping_window },
        expires_at       = IF $plan.limits.dataset_ttl    != NONE { time::now() + $plan.limits.dataset_ttl };
};

-- Mapping must finish inside the plan's window
DEFINE EVENT datasets_mapping_window ON datasets
    WHEN $event = 'UPDATE' AND $before.status = 'mapping' AND $after.status = 'importing' THEN {
        IF $after.mapping_deadline != NONE AND time::now() > $after.mapping_deadline {
            THROW 'The mapping window for this import has closed';
        };
    };

-- On completion: enforce row cap, meter rows, swap the collection's live data
DEFINE EVENT datasets_on_imported ON datasets
    WHEN $event = 'UPDATE' AND $before.status != 'imported' AND $after.status = 'imported' THEN {
        IF $after.num_rows > fn::effective_plan($after.owner).limits.max_rows_per_import {
            THROW 'Row limit exceeded for your plan';
        };
        CREATE usage_events SET principal = $after.owner, metric = 'rows_imported',
            qty = $after.num_rows, ref = $after.id;
        IF $after.collection.auto_publish {
            LET $old = $after.collection.current;
            UPDATE $after.collection SET current = $after.id;
            IF $old != NONE AND $old != $after.id { UPDATE $old SET status = 'superseded'; };
        };
    };


-- ---------------------------------------------------------------------
-- dataset_rows
--   pk: the row's primary-key value(s) joined with '|', set by the
--       importer when the collection has a primary_key.
-- ---------------------------------------------------------------------
DEFINE TABLE dataset_rows SCHEMAFULL;

DEFINE FIELD dataset     ON dataset_rows TYPE record<datasets> REFERENCE ON DELETE CASCADE;
DEFINE FIELD row_num     ON dataset_rows TYPE int;
DEFINE FIELD pk          ON dataset_rows TYPE option<string>;
DEFINE FIELD data        ON dataset_rows TYPE object FLEXIBLE;
DEFINE FIELD source      ON dataset_rows TYPE object FLEXIBLE DEFAULT {};
DEFINE FIELD valid       ON dataset_rows TYPE bool DEFAULT true;
DEFINE FIELD errors      ON dataset_rows TYPE array<object> DEFAULT [];
DEFINE FIELD errors.*.path    ON dataset_rows TYPE string;
DEFINE FIELD errors.*.message ON dataset_rows TYPE string;
DEFINE FIELD search_text ON dataset_rows TYPE option<string>;

DEFINE INDEX dataset_rows_position ON dataset_rows FIELDS dataset, row_num UNIQUE;
DEFINE INDEX dataset_rows_valid    ON dataset_rows FIELDS dataset, valid;
DEFINE INDEX dataset_rows_pk       ON dataset_rows FIELDS dataset, pk;   -- importer rejects duplicate keys

DEFINE ANALYZER row_text TOKENIZERS blank, class, punct FILTERS lowercase, ascii;
DEFINE INDEX dataset_rows_search ON dataset_rows FIELDS search_text FULLTEXT ANALYZER row_text BM25;


-- =====================================================================
-- ENDPOINTS
--   Queried with fn::find (see QUERYING): filtering, sorting and paging
--   run inside SurrealDB, as they did in MongoDB.
-- =====================================================================
DEFINE TABLE endpoints SCHEMAFULL;

DEFINE FIELD owner         ON endpoints TYPE record<principals> REFERENCE ON DELETE CASCADE;
DEFINE FIELD collection    ON endpoints TYPE record<collections> REFERENCE ON DELETE CASCADE;
DEFINE FIELD slug          ON endpoints TYPE string DEFAULT string::lowercase(rand::string(12));
DEFINE FIELD kind          ON endpoints TYPE 'temporary' | 'custom' DEFAULT 'temporary';
DEFINE FIELD status        ON endpoints TYPE 'active' | 'expired' | 'disabled' DEFAULT 'active';
DEFINE FIELD visibility    ON endpoints TYPE 'unlisted' | 'public' | 'private' DEFAULT 'unlisted';
DEFINE FIELD filterable    ON endpoints TYPE array<string> DEFAULT [];   -- field paths exposed as filters
DEFINE FIELD sortable      ON endpoints TYPE array<string> DEFAULT [];
DEFINE FIELD include_links ON endpoints TYPE array<record<links_to>> DEFAULT [];
DEFINE FIELD output        ON endpoints TYPE 'json' | 'csv' | 'geojson' | 'series' DEFAULT 'json';
DEFINE FIELD config        ON endpoints TYPE object FLEXIBLE DEFAULT {};  -- e.g. lat/lng paths, chart axes
DEFINE FIELD max_page_size ON endpoints TYPE int DEFAULT 100 ASSERT $value >= 1 AND $value <= 1000;
DEFINE FIELD expires_at    ON endpoints TYPE option<datetime>;
DEFINE FIELD created_at    ON endpoints TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD updated_at    ON endpoints TYPE datetime VALUE time::now();

DEFINE INDEX endpoints_slug   ON endpoints FIELDS slug UNIQUE;
DEFINE INDEX endpoints_expiry ON endpoints FIELDS status, expires_at;

DEFINE EVENT endpoints_on_create ON endpoints WHEN $event = 'CREATE' THEN {
    LET $plan = fn::effective_plan($after.owner);
    IF $after.kind = 'custom' AND 'custom_endpoints' NOT IN $plan.features {
        THROW 'Custom endpoints are not included in your plan';
    };
    IF $after.visibility = 'private' AND 'private_endpoints' NOT IN $plan.features {
        THROW 'Private endpoints are not included in your plan';
    };
    IF array::len($after.include_links) > 0 AND 'links' NOT IN $plan.features {
        THROW 'Linked data is not included in your plan';
    };
    IF $after.output IN ['geojson', 'series'] AND 'charts_maps' NOT IN $plan.features {
        THROW 'Chart and map outputs are not included in your plan';
    };
    IF $plan.limits.max_endpoints != NONE
       AND count(SELECT id FROM endpoints WHERE owner = $after.owner AND status = 'active') > $plan.limits.max_endpoints {
        THROW 'Endpoint limit reached for your plan';
    };
    UPDATE $after.id SET expires_at =
        IF $plan.limits.endpoint_ttl != NONE { time::now() + $plan.limits.endpoint_ttl };
};


-- =====================================================================
-- LOGIN SESSIONS  (demo mode; the microservice relies on nginx + the primary API)
-- =====================================================================

-- A browser session after a social login. The cookie holds a random token;
-- only its SHA-256 is stored, so the table can't be replayed if it leaks.
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

-- A login in progress: OAuth state, PKCE verifier and where to return.
-- Single use; the guest it started from is carried over into the account.
DEFINE TABLE auth_states SCHEMAFULL;

DEFINE FIELD provider      ON auth_states TYPE string;
DEFINE FIELD pkce_verifier ON auth_states TYPE string;
DEFINE FIELD return_to     ON auth_states TYPE string;
DEFINE FIELD guest         ON auth_states TYPE option<record<principals>>;
DEFINE FIELD created_at    ON auth_states TYPE datetime DEFAULT time::now() READONLY;
DEFINE FIELD expires_at    ON auth_states TYPE datetime DEFAULT time::now() + 10m;


-- =====================================================================
-- FUNCTIONS
-- =====================================================================

-- "google/1098..." -> principals:['google', '1098...']
DEFINE FUNCTION fn::principal($user_ref: string) {
    LET $parts = string::split($user_ref, '/');
    RETURN type::record('principals', [$parts[0], array::join(array::slice($parts, 1), '/')]);
};

-- Paid plan while it is current, otherwise the issuer's default
DEFINE FUNCTION fn::effective_plan($p: record<principals>) {
    RETURN IF $p.plan != NONE AND ($p.plan_until = NONE OR $p.plan_until > time::now()) {
        $p.plan
    } ELSE {
        $p.issuer.default_plan
    };
};

DEFINE FUNCTION fn::usage($p: record<principals>, $metric: string, $window: duration) {
    RETURN math::sum(SELECT VALUE qty FROM usage_events
        WHERE principal = $p AND metric = $metric AND at > time::now() - $window);
};

DEFINE FUNCTION fn::usage_by_ip($ip: string, $metric: string, $window: duration) {
    RETURN math::sum(SELECT VALUE qty FROM usage_events
        WHERE ip = $ip AND metric = $metric AND at > time::now() - $window);
};

-- What the UI shows: plan, limits, what is left this hour, credit balance
DEFINE FUNCTION fn::allowance($p: record<principals>) {
    LET $plan = fn::effective_plan($p);
    RETURN {
        plan:              $plan.name,
        imports_left_hour: math::max([0, $plan.limits.imports_per_hour - fn::usage($p, 'import', 1h)]),
        max_rows:          $plan.limits.max_rows_per_import,
        credits:           $p.credits,
        features:          $plan.features
    };
};

-- Run every minute from the service. Returns storage keys to delete.
DEFINE FUNCTION fn::sweep() {
    LET $now = time::now();
    UPDATE endpoints SET status = 'expired'
        WHERE status = 'active' AND expires_at != NONE AND expires_at < $now;
    UPDATE datasets SET status = 'expired'
        WHERE status = 'mapping' AND mapping_deadline != NONE AND mapping_deadline < $now;
    DELETE datasets    WHERE expires_at != NONE AND expires_at < $now;   -- rows cascade
    DELETE collections WHERE expires_at != NONE AND expires_at < $now;
    DELETE principals  WHERE expires_at != NONE AND expires_at < $now;   -- anonymous only
    LET $purged = (UPDATE uploads SET purged_at = $now
        WHERE purged_at = NONE AND purge_at != NONE AND purge_at < $now RETURN storage_key);
    RETURN $purged.storage_key;
};


-- =====================================================================
-- QUERYING  (MongoDB-style find, executed inside SurrealDB)
--   The service passes the caller's JSON straight through as a bound
--   parameter; nothing is concatenated into query text.
--
--   fn::find(endpoints:abc, {
--       filter: { country: { '$in': ['GB', 'IE'] }, population: { '$gte': 5 } },
--       sort:   { population: -1 },
--       limit:  50,
--       skip:   0
--   });
--
--   Operators: $eq $ne $gt $gte $lt $lte $in $nin $contains $startsWith $exists
--   A bare value means $eq: { country: 'GB' }. Nested fields use dots:
--   'address.city' (so the importer must keep dots out of key names).
-- =====================================================================

-- Value at a dotted path, e.g. fn::value_at(data, 'address.city')
DEFINE FUNCTION fn::value_at($obj: any, $path: string) {
    RETURN string::split($path, '.').fold($obj, |$acc, $key| $acc[$key]);
};

-- Evaluate one operator
DEFINE FUNCTION fn::test($v: any, $op: string, $arg: any) -> bool {
    IF $op = '$eq'  { RETURN $v = $arg; };
    IF $op = '$ne'  { RETURN $v != $arg; };
    IF $op = '$in'  { RETURN $v INSIDE $arg; };
    IF $op = '$nin' { RETURN $v NOTINSIDE $arg; };
    IF $op = '$exists' { RETURN ($v != NONE) = $arg; };
    IF $v = NONE { RETURN false; };
    IF $op = '$gt'  { RETURN $v > $arg; };
    IF $op = '$gte' { RETURN $v >= $arg; };
    IF $op = '$lt'  { RETURN $v < $arg; };
    IF $op = '$lte' { RETURN $v <= $arg; };
    IF $op = '$contains' {
        RETURN string::lowercase(<string>$v) CONTAINS string::lowercase(<string>$arg);
    };
    IF $op = '$startsWith' {
        RETURN string::starts_with(string::lowercase(<string>$v), string::lowercase(<string>$arg));
    };
    THROW 'Unsupported operator: ' + $op;
};

-- { a: 1, b: { '$gt': 2 } }  ->  [{path:'a', op:'$eq', value:1}, {path:'b', op:'$gt', value:2}]
DEFINE FUNCTION fn::conditions($filter: object) -> array<object> {
    RETURN object::entries($filter).map(|$e|
        IF $e[1].is_object() AND object::keys($e[1]).all(|$k| string::starts_with($k, '$')) {
            object::entries($e[1]).map(|$o| { path: $e[0], op: $o[0], value: $o[1] })
        } ELSE {
            [{ path: $e[0], op: '$eq', value: $e[1] }]
        }
    ).flatten();
};

DEFINE FUNCTION fn::find($endpoint: record<endpoints>, $q: object) {
    IF $endpoint.status != 'active' OR ($endpoint.expires_at != NONE AND $endpoint.expires_at < time::now()) {
        THROW 'Endpoint not found or expired';
    };
    LET $ds   = $endpoint.collection.current;
    IF $ds = NONE { THROW 'No data has been published to this endpoint yet'; };
    LET $spec = $ds.schema_version.field_spec;

    -- Only fields the endpoint (or, by default, the schema) allows
    LET $can_filter = IF array::len($endpoint.filterable) > 0 { $endpoint.filterable }
                      ELSE { $spec.filter(|$f| $f.filterable).map(|$f| $f.path) };
    LET $can_sort   = IF array::len($endpoint.sortable) > 0 { $endpoint.sortable }
                      ELSE { $spec.filter(|$f| $f.sortable).map(|$f| $f.path) };

    LET $conds = fn::conditions($q.filter ?? {});
    FOR $c IN $conds {
        IF $c.path NOTINSIDE $can_filter { THROW 'Cannot filter on field: ' + $c.path; };
    };

    LET $sort = object::entries($q.sort ?? {});
    IF array::len($sort) > 1 { THROW 'Sort on one field at a time'; };
    LET $sort_by  = $sort[0][0];
    LET $sort_dir = $sort[0][1] ?? 1;
    IF $sort_by != NONE AND $sort_by NOTINSIDE $can_sort { THROW 'Cannot sort on field: ' + $sort_by; };

    LET $limit = math::max([1, math::min([$q.limit ?? 50, $endpoint.max_page_size])]);
    LET $skip  = math::max([0, $q.skip ?? 0]);

    -- One pass over the dataset applies the filter and extracts the sort key
    LET $matched = SELECT data, row_num, fn::value_at(data, $sort_by ?? '') AS _key
        FROM dataset_rows
        WHERE dataset = $ds AND valid = true
          AND $conds.all(|$c| fn::test(fn::value_at(data, $c.path), $c.op, $c.value));

    -- Sort and page the matches (ORDER BY direction cannot be a parameter)
    LET $page = IF $sort_by = NONE {
        SELECT data, row_num FROM $matched ORDER BY row_num LIMIT $limit START $skip
    } ELSE IF $sort_dir < 0 {
        SELECT data, row_num, _key FROM $matched ORDER BY _key DESC, row_num LIMIT $limit START $skip
    } ELSE {
        SELECT data, row_num, _key FROM $matched ORDER BY _key ASC, row_num LIMIT $limit START $skip
    };

    RETURN { data: $page.map(|$r| $r.data), total: array::len($matched), limit: $limit, skip: $skip };
};


-- =====================================================================
-- SEED DATA
--   Two account types in the demo service, plus the microservice tenant:
--     guest       anonymous visitors, keyed by a salted hash of IP + user agent
--     registered  free accounts from a GitHub or Google login
--     service     tenants forwarded by the primary API (AUTH_MODE=service)
--   Limits are starting points; tune them to the demo's capacity.
-- =====================================================================
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

UPSERT issuers:guest   CONTENT { name: 'Guest',           trust: 'anonymous', default_plan: plans:guest, principal_ttl: 1d, enabled: true };
UPSERT issuers:github  CONTENT { name: 'GitHub',          trust: 'social',    default_plan: plans:registered, enabled: true };
UPSERT issuers:google  CONTENT { name: 'Google',          trust: 'social',    default_plan: plans:registered, enabled: true };
UPSERT issuers:service CONTENT { name: 'Primary API',     trust: 'system',    default_plan: plans:service, enabled: true };

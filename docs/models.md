# Models

Every table in [`data/schema.surql`](../data/schema.surql) has a typed model in
[`src/models/`](../src/models/), one module per table. This page documents the same models in
the same order. `cargo test` checks that each model reads its table's records back without
losing a field (`tests::models_match_the_schema`), so a schema change that isn't reflected in
the model fails `cargo test`.

```
issuers ─< principals ─┬─ social_users, auth_sessions
                       ├─< usage_events, credit_ledger
                       ├─< uploads
                       ├─< api_schemas ─< api_schema_versions ─< mappings
                       └─< collections ─< datasets ─< dataset_rows
                                │  (current = live dataset)
                                ├── links_to ──> collections
                                └─< endpoints
plans: referenced by issuers (default plan) and principals (plan override)
auth_states: logins in progress (demo mode)
```

**Conventions.** In the database and in the Rust records, ids and links are `table:key`
strings, datetimes are RFC 3339 strings, and durations are SurrealQL durations such as `15m`,
`1h` or `7d`. Optional fields are absent when unset. **HTTP responses use bare keys**
(`"0199a…"`, not `"datasets:0199a…"`) and the response shapes described under
[API views](#api-views).

| Table | Model | Module | In the API |
|---|---|---|---|
| `plans` | `Plan` | [plan.rs](../src/models/plan.rs) | `allowance` on upload and `/auth/me` |
| `issuers` | `Issuer` | [issuer.rs](../src/models/issuer.rs) | the prefix of `user_ref` |
| `principals` | `Principal` | [principal.rs](../src/models/principal.rs) | the caller (`user_ref` in `/auth/me`) |
| `social_users` | `SocialUser` | [social_user.rs](../src/models/social_user.rs) | `profile` in `/auth/me` |
| `auth_sessions` | `AuthSession` | [auth_session.rs](../src/models/auth_session.rs) | – (the session cookie) |
| `auth_states` | `AuthState` | [auth_state.rs](../src/models/auth_state.rs) | – (the OAuth `state`) |
| `usage_events` | `UsageEvent` | [usage_event.rs](../src/models/usage_event.rs) | – (written by schema events) |
| `credit_ledger` | `CreditEntry` | [credit_ledger.rs](../src/models/credit_ledger.rs) | – |
| `uploads` | `Upload` | [upload.rs](../src/models/upload.rs) | `upload` on upload |
| `api_schemas` | `ApiSchema` | [api_schema.rs](../src/models/api_schema.rs) | – |
| `api_schema_versions` | `ApiSchemaVersion` | [api_schema_version.rs](../src/models/api_schema_version.rs) | `schema_version`, `definition`, `field_spec` |
| `mappings` | `Mapping` | [mapping.rs](../src/models/mapping.rs) | – (the mapping document is in `definition`) |
| `collections` | `Collection` | [collection.rs](../src/models/collection.rs) | `collection` |
| `links_to` | `Link` | [link.rs](../src/models/link.rs) | – |
| `datasets` | `Dataset` | [dataset.rs](../src/models/dataset.rs) | `/datasets` resources |
| `dataset_rows` | `DatasetRow` | [dataset_row.rs](../src/models/dataset_row.rs) | `rows` of `GET /datasets/{id}` |
| `endpoints` | `Endpoint` | [endpoint.rs](../src/models/endpoint.rs) | – (not routed yet) |

---

## Accounts

There are no paid accounts. The demo service has **guests** and **registered** users, and the
microservice deployment has **service** tenants. Each kind is an issuer with its own default
plan:

| Account | Principal | Identified by | Plan |
|---|---|---|---|
| guest | `guest/<hash>` | salted SHA-256 of client address + user agent (demo mode) | `plans:guest` |
| registered | `github/<id>`, `google/<sub>` | session cookie from a social login (demo mode) | `plans:registered` |
| service | `service/<ref>` | the reference the primary API forwards in `X-User-Ref` (service mode) | `plans:service` |

### Plan — `plans`

What a principal may do. Seeded as `plans:guest`, `plans:registered` and `plans:service`; the
credit and cost fields are kept but always 0.

| Field | Type | Notes |
|---|---|---|
| `name` | string | |
| `rank` | int | 0 guest, 1 registered, 2 service |
| `limits.imports_per_hour` | int | Enforced when a dataset is created, per principal and, for anonymous principals, per IP |
| `limits.max_rows_per_import` | int | Enforced when an import finishes |
| `limits.max_collections` | int? | Unset = unlimited |
| `limits.max_endpoints` | int? | Unset = unlimited; counts active endpoints |
| `limits.mapping_window` | duration? | Time from upload to import; unset = no deadline |
| `limits.source_retention` | duration | Uploaded files are purged after this |
| `limits.dataset_ttl` | duration? | Datasets and collections expire after this; unset = kept |
| `limits.endpoint_ttl` | duration? | |
| `limits.keep_revisions` | int | Superseded imports kept per collection (default 1) |
| `features` | `Feature[]` | `filter` `sort` `search` `custom_endpoints` `custom_slug` `custom_schema` `links` `auto_refresh` `charts_maps` `private_endpoints` |
| `monthly_credits` | int | |
| `costs.import` / `rows_imported` / `api_request` / `storage_mb_day` | int | Credits per 1,000 units; 0 = free |

A principal's **effective plan** (`fn::effective_plan`) is its own `plan` while `plan_until`
hasn't passed, otherwise its issuer's `default_plan`. Nothing sets `plan` today; it's there for
granting one account more by hand.

**Allowance** (`fn::allowance`, model `Allowance`) is what the upload response returns as
`allowance`:

```json
{ "plan": "Registered", "imports_left_hour": 29, "max_rows": 20000, "credits": 0,
  "features": ["filter", "sort", "search", "custom_endpoints", "custom_slug", "custom_schema", "links"] }
```

Features gate the API: `filter`, `sort` and `search` gate those query parameters on
`GET /datasets/{id}` (`403` otherwise), `custom_schema` gates everything in a mapping beyond
`source` (`422` naming each locked part), and `links` gates `primary_key`.

### Issuer — `issuers`

The part before the slash in a user reference: `issuers:guest`, `github`, `google` and
`service`. An unknown or disabled issuer gets `403`. Databases created before accounts keep
their old issuers (`device`, `microsoft`, `mainusers`, `system`), disabled.

| Field | Type | Notes |
|---|---|---|
| `name` | string | |
| `trust` | `anonymous` \| `social` \| `managed` \| `system` | Anonymous principals are also rate-limited by IP |
| `default_plan` | → `plans` | |
| `principal_ttl` | duration? | Inactive principals are deleted after this |
| `enabled` | bool | |

### Principal — `principals`

A caller, `<issuer>/<external id>`, keyed as `principals:['github', '583231']`. Who that is
depends on `AUTH_MODE` (see [Accounts](#accounts)); callers never choose it in demo mode,
where identity headers are ignored. A principal is created on its first request
(`store::touch_principal`). In Rust, `Me` is the reference and `auth::Caller` adds the
account kind and client address.

| Field | Type | Notes |
|---|---|---|
| `issuer` | → `issuers` | |
| `external_id` | string | |
| `status` | `active` \| `blocked` | Blocked principals get `403` |
| `plan` | → `plans`? | Plan override; unset = issuer default |
| `plan_until` | datetime? | When the override ends |
| `credits` | int | Running sum of `credit_ledger` (unused) |
| `last_seen_at` | datetime | Refreshed by upload, configure, import and `GET /datasets` |
| `expires_at` | datetime? | `last_seen_at + issuer.principal_ttl` (guests: a day) |
| `created_at`, `updated_at` | datetime | |

Deleting a principal cascades to everything it owns, so an idle guest's uploads disappear with
it. When a guest signs in, `store::claim_guest_data` moves their collections (those whose
names the account doesn't already use), datasets, schemas and uploads into the account.

### SocialUser — `social_users`

The limited profile kept for a social login, one per principal, refreshed on every sign-in.

| Field | Type |
|---|---|
| `principal` | → `principals` (unique) |
| `provider` | string (`github`, `google`) |
| `email`, `name`, `avatar_uri`, `locale` | string? (GitHub: the primary verified email; Google: only if verified) |
| `last_login_at` | datetime |

### AuthSession — `auth_sessions`

A browser session after a social login (demo mode). The `sheet_session` cookie (HttpOnly,
SameSite=Lax) holds a random token; only its SHA-256 is stored. Deleted on logout and swept
once expired.

| Field | Type | Notes |
|---|---|---|
| `principal` | → `principals` | Cascades |
| `token_hash` | string | Unique |
| `user_agent`, `ip` | string? | At sign-in |
| `created_at`, `last_seen_at` | datetime | `last_seen_at` refreshed at most hourly |
| `expires_at` | datetime | `SESSION_DAYS` after sign-in |

### AuthState — `auth_states`

A login in progress, keyed by its OAuth `state` and bound to the browser by a short-lived
`sheet_login` cookie. Read and deleted by the callback; swept after ten minutes.

| Field | Type | Notes |
|---|---|---|
| `provider` | string | |
| `pkce_verifier` | string | PKCE (S256) |
| `return_to` | string | A frontend URL, checked against `FRONTEND_URL` |
| `guest` | → `principals`? | The guest who started the login |
| `created_at`, `expires_at` | datetime | |

## Usage

### UsageEvent — `usage_events`

Append-only metering. The schema writes `import` when a dataset is created and `rows_imported`
when it finishes. Every event on a plan with a non-zero `costs[metric]` debits the ledger.

| Field | Type | Notes |
|---|---|---|
| `principal` | → `principals` | |
| `metric` | `import` \| `rows_imported` \| `api_request` \| `storage_mb_day` | |
| `qty` | int | API requests are meant to be aggregated, e.g. per endpoint per minute |
| `ref` | record? | The dataset, endpoint … |
| `ip` | string? | For anonymous rate limits |
| `at` | datetime | |

### CreditEntry — `credit_ledger`

Append-only: updates are rejected, so add a correcting entry instead. Each entry adds `delta`
to `principals.credits`.

| Field | Type | Notes |
|---|---|---|
| `principal` | → `principals` | |
| `delta` | int | + grant/purchase, − usage |
| `reason` | `monthly_grant` \| `purchase` \| `usage` \| `refund` \| `adjustment` | |
| `metric` | string? | For `usage` entries |
| `ref` | record? | The usage event behind it |
| `note` | string? | |
| `at` | datetime | |

---

## Sources and schemas

### Upload — `uploads`

A stored source file. Its worksheets each become a dataset. The file is deleted once every
worksheet is imported, or when the plan's `source_retention` runs out (`fn::sweep`).

| Field | Type | Notes |
|---|---|---|
| `owner` | → `principals` | |
| `source_kind` | `file` \| `url` \| `transfer` | Only `file` is used so far |
| `filename` | string | Original file name |
| `storage_key` | string | File name under `UPLOAD_DIR` |
| `content_type` | string? | |
| `size_bytes` | int | |
| `sheets` | string[] | Worksheet names, in workbook order; empty for CSV-like files |
| `purge_at` | datetime? | Set from the plan on create |
| `purged_at` | datetime? | Set once the file is gone |
| `created_at` | datetime | |

API: `POST /datasets/upload` returns `"upload": { "id", "filename", "size", "sheets" }`.

### ApiSchema — `api_schemas`

The named, versioned description of a collection's rows. It is created with the collection's
name on first configure and is unique per owner.

| Field | Type | Notes |
|---|---|---|
| `owner` | → `principals` | |
| `name` | string | |
| `description` | string? | From the mapping document's `description` |
| `inferred` | bool | `true` when inferred from a sheet; `custom_schema` plans may supply JSON Schema |
| `created_at`, `updated_at` | datetime | |

### ApiSchemaVersion — `api_schema_versions`

One version of a schema. A version is a `draft` until its first import publishes it.
Published versions can't change, so a later configure creates the next version.

| Field | Type | Notes |
|---|---|---|
| `schema` | → `api_schemas` | |
| `version` | int ≥ 1 | Unique per schema |
| `status` | `draft` \| `published` \| `deprecated` | |
| `definition` | object | JSON Schema (draft 2020-12) of one output row, with `x-source` per property, `x-primary-key` and `x-mapping` (the full mapping document) |
| `field_spec` | `FieldSpec[]` | See below |
| `created_at`, `updated_at` | datetime | |

**FieldSpec** ([`sheet/spec.rs`](../src/sheet/spec.rs)) is one output path and what queries may do
with it:

| Field | Type | Default |
|---|---|---|
| `path` | string | Dotted output path, e.g. `price.amount` |
| `type` | `string` \| `number` \| `integer` \| `boolean` \| `date` \| `datetime` \| `object` \| `array` \| `geo` | |
| `required` | bool | `false` |
| `searchable` | bool | `false` |
| `filterable` | bool | `true` |
| `sortable` | bool | `true` |

API: `schema_version: { "id", "version", "status" }` on datasets, `definition` from
`PATCH /datasets/{id}/configure`, and `field_spec` / `fields` from configure, import and
`GET /datasets/{id}`.

### Mapping — `mappings`

The source-column view of a version's mapping document, one row (`name: "default"`) per
version. The complete mapping document that clients send (`MappingDoc`, see the
[README](../README.md#mapping-document)) is stored in the version's `definition["x-mapping"]`.
This table is an index of it.

| Field | Type | Notes |
|---|---|---|
| `schema_version` | → `api_schema_versions` | |
| `name` | string | Unique per version |
| `sheet` | string? | |
| `header_row` | int | 1-based |
| `columns[].column` | string | The source column as the mapping names it |
| `columns[].path` | string | Output path |
| `columns[].transform` | string? | Format or type, e.g. `d2`, `integer` |
| `columns[].default` | any? | |
| `created_at`, `updated_at` | datetime | |

---

## Data

### Collection — `collections`

A stable, named data source, `<file> / <sheet>` by default. Every import is a new dataset in
the collection. Endpoints and links point at the collection, so the latest import replaces the
data behind all of them.

| Field | Type | Notes |
|---|---|---|
| `owner` | → `principals` | |
| `name` | string | Unique per owner. A mapping's `collection` (id or name) targets an existing one |
| `schema` | → `api_schemas`? | |
| `current` | → `datasets`? | The live dataset |
| `primary_key` | string[]? | Output paths, e.g. `["sku"]`; needs `links` |
| `auto_publish` | bool | A finished import becomes `current` (default `true`) |
| `expires_at` | datetime? | From the plan's `dataset_ttl` |
| `created_at`, `updated_at` | datetime | |

API: `collection: { "id", "name", "current" }` on datasets, plus `reused_mapping` on upload.
Re-uploading a sheet whose collection exists reuses its latest version.

### Link — `links_to`

A relation from a child collection (`in`) to a parent collection (`out`). It joins the child's
`from_path` values to the parent's `primary_key`. Both collections need the same owner, the
parent must have a primary key, and the owner's plan needs `links`.

| Field | Type | Notes |
|---|---|---|
| `in` / `out` | → `collections` | Rust: `from` / `to` |
| `name` | string | Key under which linked data appears; unique per child |
| `from_path` | string[] | Same length and order as the parent's `primary_key` |
| `cardinality` | `many_to_one` \| `one_to_one` | |
| `created_at` | datetime | |

### Dataset — `datasets`

One import of one worksheet into a collection. This is the `/datasets/{id}` resource.

| Field | Type | Notes |
|---|---|---|
| `owner` | → `principals` | |
| `collection` | → `collections` | |
| `upload` | → `uploads`? | |
| `sheet` | string? | Unset for CSV-like files |
| `schema_version` | → `api_schema_versions` | |
| `mapping` | → `mappings`? | |
| `series` | int | Import number within the collection, unique per collection |
| `status` | `DatasetStatus` | See below |
| `error` | string? | Why the import failed |
| `num_rows`, `num_invalid` | int | |
| `client_ip` | string? | For anonymous rate limits |
| `mapping_deadline` | datetime? | Import must start by then |
| `imported_at`, `expires_at` | datetime? | |
| `created_at`, `updated_at` | datetime | |

**Status:** `mapping` (uploaded, configurable) → `importing` → `imported` → `superseded`
(a later import became `current`). An import can also end in `failed` (see `error`; it can be
retried), and a dataset still in `mapping` becomes `expired` when its mapping window closes.
Configure and import return `409` while a dataset is `importing`.

### DatasetRow — `dataset_rows`

One imported row.

| Field | Type | Notes |
|---|---|---|
| `dataset` | → `datasets` | |
| `row_num` | int | Unique per dataset |
| `pk` | string? | Primary-key value(s) joined with `\|` |
| `data` | object | The mapped row, as `GET /datasets/{id}` returns it in `rows` |
| `source` | object | |
| `valid` | bool | Invalid rows are hidden unless `include_invalid=true` |
| `errors[]` | `{ path, message }` | Why the row is invalid |
| `search_text` | string? | Text of the `searchable` fields, full-text indexed for `q` |

### Endpoint — `endpoints`

A published view of a collection's current dataset, queried inside SurrealDB with `fn::find`.
**It isn't served by the HTTP routes yet**, so the model and table exist ahead of the API.

| Field | Type | Notes |
|---|---|---|
| `owner` | → `principals` | |
| `collection` | → `collections` | |
| `slug` | string | Globally unique; random unless `custom_slug` |
| `kind` | `temporary` \| `custom` | `custom` needs `custom_endpoints` |
| `status` | `active` \| `expired` \| `disabled` | |
| `visibility` | `unlisted` \| `public` \| `private` | `private` needs `private_endpoints` |
| `filterable`, `sortable` | string[] | Empty = the schema's flags |
| `include_links` | → `links_to`[] | Needs `links` |
| `output` | `json` \| `csv` \| `geojson` \| `series` | `geojson`/`series` need `charts_maps` |
| `config` | object | e.g. lat/lng paths, chart axes |
| `max_page_size` | int | 1–1000, default 100 |
| `expires_at` | datetime? | From the plan's `endpoint_ttl` |
| `created_at`, `updated_at` | datetime | |

---

## API views

The routes don't return records directly. They read projections that join a record with its
neighbours and return the shapes below.

**Dataset summary** (`DatasetInfo::summary`) is the `dataset` in configure, import, preview and
`GET /datasets/{id}` responses, and each item of `GET /datasets`:

```json
{
  "id": "0199…", "name": "catalogue / Products", "sheet": "Products", "status": "imported",
  "collection": { "id": "0199…", "name": "catalogue / Products", "current": "0199…" },
  "series": 2,
  "schema_version": { "id": "…", "version": 1, "status": "published" },
  "primary_key": ["sku"],
  "num_rows": 4, "num_invalid": 0, "error": null,
  "filename": "catalogue.xlsx", "source_available": false,
  "created_at": "…", "imported_at": "…", "mapping_deadline": null, "expires_at": null
}
```

`DatasetInfo` flattens a dataset with its upload, collection and schema version into one read.
`CollectionInfo` (`id`, `name`, `schema_id`) and `VersionInfo` (`id`, `version`, `status`,
`definition`) are the slices the upload and configure flows need.

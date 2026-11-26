# Models

[`data/schema.sql`](../data/schema.sql) is the full description of every table, with its
columns, checks and indexes. This page explains how the tables fit together, the rules the
service applies on top of them, and the shapes the API returns.

```
issuers ─< principals ─┬─ social_users, auth_sessions
   └─< issuer_keys     ├─< usage_events, credit_ledger
                       ├─< uploads
                       ├─< api_schemas ─< api_schema_versions ─< mappings
                       └─< collections ─< datasets ─< dataset_rows
                                (current_id = live dataset)
plans: referenced by issuers (default plan) and principals (plan override)
auth_states: logins in progress (demo mode)
revoked_tokens: access tokens withdrawn early
links_to, endpoints: reserved, not served yet
```

**Conventions.** Ids are plain text: UUIDs, the names of plans and issuers, and for a
principal its whole reference (`github/583231`). Times are RFC 3339 UTC text, durations are
whole seconds, lists and objects are JSON text, and booleans are 0 and 1. Rules that depend
on the caller's plan are applied by the service in [`src/store.rs`](../src/store.rs), not by
triggers.

## Accounts

| Table | What it holds |
|---|---|
| `plans` | Quotas, retention, time limits and features: `guest`, `registered`, `service` |
| `issuers` | Who vouches for a principal: `guest`, `github`, `google`, `service`, and each application registered in `ISSUERS_FILE` |
| `issuer_keys` | The Ed25519 public keys an application signs access tokens with |
| `principals` | A caller, `<issuer>/<external id>` |
| `social_users` | The profile kept for a GitHub or Google login: `profile` in `/auth/me` |
| `auth_sessions`, `auth_states` | Session cookies (stored as a hash) and logins in progress |
| `revoked_tokens` | Access tokens withdrawn before they expire, by `jti` |

A principal's **plan** is its own `plan` while `plan_until` has not passed, otherwise its
issuer's `default_plan`. **Allowance** is what `/auth/me` and the upload response show of it:

```json
{ "plan": "Registered", "imports_left_hour": 29, "max_rows": 20000, "credits": 0,
  "features": ["filter", "sort", "search", "custom_endpoints", "custom_slug", "custom_schema", "links"] }
```

Rules:

- A principal is created on first contact. Guests (`principal_ttl_secs` on their issuer)
  expire after a day without activity, and their data goes with them.
- A blocked principal, or one from a disabled or unknown issuer, is refused.
- Credits and `credit_ledger` are kept in the model but unused: there are no paid accounts.

## Usage

`usage_events` records one `import` per dataset created and one `rows_imported` per finished
import. The hourly import quota (`imports_per_hour`) counts the first, per principal and, for
guests, per client address as well.

## Sources and schemas

| Table | What it holds |
|---|---|
| `uploads` | A stored source file. Deleted once every worksheet in it is imported, or when the plan's `source_retention_secs` runs out |
| `api_schemas` | The named description of a collection's rows; its content is in the versions |
| `api_schema_versions` | `definition` (JSON Schema of one row, with `x_source`, `x_primary_key` and `x_mapping`, the whole mapping document) and `field_spec` (output paths with type and query flags) |
| `mappings` | A flat summary of a version's column mappings |

A version is a `draft` until its first import publishes it, with the types found over every
row. A published version is not changed: saving a different mapping creates the next version.

## Data

| Table | What it holds |
|---|---|
| `collections` | A stable, named data source. `current_id` is its live dataset, `primary_key` its key paths, `visibility` who may read it |
| `datasets` | One import of one worksheet: `mapping` → `importing` → `imported` → `superseded`, or `failed` / `expired` |
| `dataset_rows` | The rows: `row_num`, `data` (JSONB), `pk`, `valid`, `errors`, `search_text` |
| `dataset_rows_fts` | Full-text index over `search_text` |

Rules:

- Creating a collection checks `max_collections`; creating a dataset checks the hourly quota
  and sets `mapping_deadline` and `expires_at` from the plan.
- An import must start before `mapping_deadline`, and may hold at most `max_rows_per_import` rows.
- A finished import becomes its collection's current data, and the one it replaces is `superseded`.
- Deleting a dataset deletes its rows; deleting a collection deletes its datasets.

`visibility` is `unlisted` (anyone with a dataset's id may read, the default), `public` (the
same, and free to be listed) or `private` (the owner, or an access token granting `read`).
Private needs a plan with `private_endpoints`.

## API views

The routes don't return records directly. They read projections that join a record with its
neighbours and return the shapes below.

**Dataset summary** (`DatasetInfo::summary`) is the `dataset` in configure, import, preview,
access and `GET /datasets/{id}` responses, and each item of `GET /datasets`:

```json
{
  "id": "0199…", "name": "catalogue / Products", "sheet": "Products", "status": "imported",
  "collection": { "id": "0199…", "name": "catalogue / Products", "current": "0199…" },
  "visibility": "unlisted",
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

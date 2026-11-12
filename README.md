# Spreadsheet API

Upload spreadsheets, map each worksheet to JSON with a versioned, API-doc-style schema,
import the rows into an embedded SurrealDB, and query them as a Web endpoint.

The successor to [sheetapi](https://github.com/neilg63/sheetapi). It uses
[spreadsheet-to-json](https://crates.io/crates/spreadsheet-to-json) 0.5 and exposes
every [spread-cli](https://github.com/neilg63/spreadsheet_to_json_cli) option as
structured JSON that a frontend can build from form widgets.

```bash
cp env.example .env
cargo run --release
```

Formats: `xlsx`, `xlsm`, `xls`, `xlsb`, `ods`, `csv`, `tsv`, and semicolon-separated values
(`.ssv`, or `;`-delimited `.csv`; delimiters are detected from the content).

## Flow

| Step | Endpoint | What happens |
|---|---|---|
| 1 | `POST /datasets/upload` | The file is validated and stored. Each non-empty worksheet becomes a **dataset**, and the response shows each one's detected header row, its columns and its first 30 rows. |
| 2 | `PATCH /datasets/{id}/configure` | Saves a **mapping document** as a schema version and returns a fresh preview. Add `?dry_run=true` to preview only. `GET /datasets/{id}/preview` re-reads the file with the saved mapping. |
| 3 | `PATCH /datasets/{id}/import` | Imports every row. The source file is deleted once all of its worksheets are imported. |
| 4 | `GET /datasets/{id}` | Returns the imported rows, with filtering, sorting, full-text search and pagination. |

Also available: `GET /datasets/{id}/export?format=json|jsonl` (every row, streamed as a
download), `GET /datasets` (the caller's upload history), `GET /auth/me` (account type,
profile, allowance and sign-in links), `GET /mapping/options` (the value lists for the mapping
and filter widgets), and `GET /` (a list of all routes and the active mode).

```bash
curl -F file=@catalogue.xlsx localhost:3000/datasets/upload
curl -X PATCH localhost:3000/datasets/$ID/configure -H 'content-type: application/json' -d @mapping.json
curl -X PATCH localhost:3000/datasets/$ID/import
curl -G localhost:3000/datasets/$ID --data-urlencode 'filter={"price.amount":{"$gte":3}}' --data-urlencode 'sort=-price.amount'
```

Upload also accepts an optional multipart field `source` holding JSON source options for every
sheet, for example `{"decimal_comma": true}`.

## Deployments and accounts

`AUTH_MODE` picks one of two deployments. Plan limits and features come from the schema
(`plans:guest`, `plans:registered`, `plans:service`), so the account decides the rows per
import, imports per hour, retention, and which mapping and query features are available.

### `AUTH_MODE=demo` (default): public demo service

| | Guest | Registered |
|---|---|---|
| Who | Anyone, identified as `guest/<hash>` of the client address and user agent, salted with `GUEST_SALT` | Signed in with GitHub or Google; `github/<id>`, `google/<sub>` |
| Upload | Up to `GUEST_MAX_UPLOAD_SIZE` (2 MB) | Up to `MAX_UPLOAD_SIZE` |
| Convert and preview | Upload, fix header rows and other `source` options, import, query, export | Same |
| Customise the API | No: any other mapping part returns `422` with "sign in to customise" | Fields, patterns, primary keys and upserts, filters, target collections |
| Upload history (`GET /datasets`) | No (`403`) | Yes |
| Kept | A day after last activity | 90 days |

Identity headers are ignored, so callers cannot choose their own identity. Sign-in uses the OAuth
authorization code flow with PKCE, implemented here with the `oauth2` crate:

```
GET  /auth/{github|google}/login?return_to=/path   redirect to the provider
GET  /auth/{github|google}/callback                 sets the session cookie, redirects to FRONTEND_URL + path
POST /auth/logout
GET  /auth/me
```

Sessions are opaque random tokens in an HttpOnly `sheet_session` cookie; only their SHA-256 is
stored (`auth_sessions`). When a guest signs in, their uploads move into the account. Each
client address gets `RATE_LIMIT_PER_MINUTE` requests (uploads and imports cost 10) and then
`429` with `Retry-After`, on top of the schema's per-principal and per-IP import quotas.

Register OAuth apps with the callback `PUBLIC_URL/auth/github/callback` (and `/google/`), then
set `GITHUB_CLIENT_ID`/`_SECRET` and `GOOGLE_CLIENT_ID`/`_SECRET`. Behind nginx, set
`CLIENT_IP_HEADER=X-Real-IP` and have nginx overwrite it (`proxy_set_header X-Real-IP
$remote_addr;`); without a proxy, leave it unset so the socket address is used, since clients
could otherwise spoof it. If the frontend is on another origin, list it in `CORS_ORIGINS` so
the cookie is sent.

### `AUTH_MODE=service`: microservice behind nginx and the primary API

There are no login routes or rate limits here; nginx and the primary API control access. The
primary API forwards the tenant or user in `X-User-Ref` (`acme/42` becomes `service/acme/42`),
and datasets are scoped to it. Set `SERVICE_TOKEN` to also require `Authorization: Bearer
<token>` (or `X-Service-Token`). Files arrive in any of three ways:

```bash
# multipart, as in demo mode
curl -H 'X-User-Ref: acme/42' -F file=@sales.xlsx localhost:3000/datasets/upload
# a file the primary API left in SHARED_UPLOAD_DIR (copied; the original stays)
curl -H 'X-User-Ref: acme/42' -H 'content-type: application/json' -d '{"path": "inbox/sales.xlsx"}' localhost:3000/datasets/upload
# a URL to download (limit hosts with URL_UPLOAD_HOSTS)
curl -H 'X-User-Ref: acme/42' -H 'content-type: application/json' -d '{"url": "https://files.internal/sales.xlsx", "source": {"decimal_comma": true}}' localhost:3000/datasets/upload
```

Paths are resolved (including `..` and symlinks) and must stay inside `SHARED_UPLOAD_DIR`.
For local development, `DEFAULT_USER_REF` gives requests without the header an identity.

### Schema changes

`data/schema.surql` sets up a new database. Changes for existing databases go in
`data/migrations/*.surql`, which are applied once each, in name order, inside a transaction.
A new database records them all as applied.

## Mapping document

A mapping document lists only **overrides**. Any column it doesn't mention keeps its natural
key (the snake_cased header text) and its detected type. Unknown properties are rejected, so
typos get caught. Validation errors come back together, each tied to the widget that caused it:

```json
{ "error": "invalid_mapping", "errors": [
  { "path": "fields[0].source", "message": "'nope' matches no column in this sheet" },
  { "path": "filter.all[0].value", "message": "'between' needs a [low, high] pair" } ] }
```

```json
{
  "title": "Product catalogue",
  "collection": "catalogue / Products",
  "source": { "header_row": 4, "data_start": 5, "decimal_comma": false, "datetime": "date" },
  "primary_key": ["sku"],
  "fields": [
    { "source": "SKU", "required": true, "description": "Stock keeping unit" },
    { "source": "name", "searchable": true },
    { "source": "Unit Price", "path": "price.amount", "type": "number", "precision": 2 },
    { "source": "D", "path": "in_stock", "type": "boolean" },
    { "source": "tags", "format": "text[](;)" }
  ],
  "patterns": [{ "source": "sales_[year]", "target": "sales[].{year}{units}", "type": "integer" }],
  "exclude": ["colour", "width..depth"],
  "exclude_paths": ["size.depth"],
  "filter": { "all": [{ "field": "price.amount", "op": "gt", "value": 0 }] },
  "omit_nulls": true,
  "unmapped": "keep"
}
```

| Key | Purpose (spread-cli equivalent) |
|---|---|
| `source.header_row` / `header_rows` / `data_start` | 1-based header row, number of header rows (for merged headers), and first data row. Leave out `header_row` to auto-detect it (`-t`, `-b`). |
| `source.no_header`, `source.column_names` | `header` / `header_numbered` / `a1` / `numbered` (`--omit-header`, `-c`, `-A`, `-R`) |
| `source.decimal_comma`, `source.datetime` | `full` / `simple` / `date` / `time` / `hm` (`--euro-number-format`, `--date-only` …) |
| `source.max_rows` | Stops the import after this many rows (`-m`). The plan's row cap always applies. |
| `fields[].source` | The column, given as its natural key, header text, A1 letter or column number. |
| `fields[].path` | The output path: `price`, `size.width` (nested) or `tags[]` (appended to a list). `key` accepts a raw KeySegment tree for anything more complex. |
| `fields[].type` / `format` / `precision` | `string`, `integer`, `number`, `boolean`, `date`, `datetime`, `time`, `array`, or any library format such as `d2`, `truthy(si,no)`, `dt:%d/%m/%Y`, `int[](\|)` (`-k col\|fmt`). |
| `fields[].default`, `datetime`, `decimal_comma` | Per-column overrides. |
| `fields[].required`, `filterable`, `sortable`, `searchable`, `description` | Schema and query metadata. Rows missing a required value are imported but flagged invalid. |
| `patterns` | Placeholder patterns such as `file_[n]`→`files[]`, `sales_{region}`→`sales.{region}`, `sales_[year]`→`sales[].{year}{amount}`. |
| `exclude`, `exclude_paths` | Drop columns or ranges, or drop nested output paths (`-k -col`, `-K`). |
| `filter` | Only import rows that match. Accepts either a rule tree (as above) or spread-cli text such as `"age >= 18 and name ILIKE 'a%'"` (`-f`). |
| `omit_nulls` | Drop null values (`-X`). |
| `unmapped` | Set to `drop` to output only the listed columns. |
| `primary_key` | Output paths that identify a row. See the upsert section below. |
| `collection` | Import into an existing collection, given by ID or name, for example when this month's file has a new name. |

Each save creates or updates a row in `api_schema_versions`. Its `definition` holds a JSON
Schema (draft 2020-12) of one output row, with OpenAPI-style extensions: `x-source` on each
property, `x-primary-key`, and `x-mapping`, which holds the complete mapping document so every
version describes itself. `field_spec` holds the flat list of paths with their type and query
flags. A version stays a draft until its first import publishes it, and published versions are
immutable, so a later configure creates the next version.

## Collections, upserts and primary keys

A worksheet belongs to a **collection**, named `<file> / <sheet>` by default. Every import
creates a new dataset in that collection, and when it finishes it becomes the collection's
`current` dataset while the previous one is marked `superseded`.

- If you upload a file whose collection already exists, its latest mapping is reused
  automatically (`"reused_mapping": true`), so you can import straight away.
- With a `primary_key`, the import mode defaults to **upsert**. Incoming rows replace current
  rows that have the same key, current rows whose keys are missing from the file are kept, and
  duplicate keys within a file are flagged invalid. Send `{"mode": "replace"}` to keep only
  the new file's rows.
- Primary keys need a plan with the `links` feature, as defined in the schema. The same keys
  are what `links_to` joins collections on.

## Querying `GET /datasets/{id}`

| Parameter | Example |
|---|---|
| `filter` | MongoDB style: `{"price":{"$gte":10},"country":{"$in":["GB","IE"]},"$or":[…]}`. Or the rule tree used by the import filter: `{"all":[{"field":"price","op":"gte","value":10}]}` |
| `sort` | `price,-name` or `price:desc` |
| `q` | Full-text search over fields marked `searchable` |
| `limit`, `start` / `page` | Pagination. `limit` is capped at `MAX_PAGE_SIZE`. |
| `include_invalid` | `true` includes rows that failed validation |

MongoDB operators: `$eq $ne $gt $gte $lt $lte $in $nin $all $size $exists $regex` (with `$options`)
`$contains $startsWith $endsWith $between $has $and $or $nor $not`. Field paths are validated
before use, and every value is sent as a bound parameter.

## Layout

```
src/sheet/mapping.rs     mapping document → spreadsheet-to-json OptionSet
src/sheet/reader.rs      previews and streaming reads (on the blocking thread pool)
src/sheet/spec.rs        type inference, field_spec, JSON Schema
src/sheet/key_pattern.rs, filter_pattern.rs   ported verbatim from spread-cli
src/importer.rs          batched import, primary keys, validation, upsert merge
src/query.rs             filter/sort/search → parameterised SurrealQL
src/models/              one typed model per table in data/schema.surql
src/store.rs, db.rs      queries against data/schema.surql
src/routes/              HTTP handlers
src/auth/                demo/service identity, OAuth sign-in, sessions, rate limiting
data/migrations/         schema changes for existing databases
docs/models.md           the models, their fields and rules, and where they appear in the API
```

`cargo test` runs the unit tests and end-to-end tests against an in-memory database: upload →
configure → import → query → upsert, service-mode tenants and URL/path intake, guest limits and
rate limiting, and a full GitHub-style sign-in against a mock OAuth provider.
`cargo run --example make_fixtures` regenerates `tests/fixtures`.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENCE-APACHE](LICENCE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT licence ([LICENCE-MIT](LICENCE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without
any additional terms or conditions.

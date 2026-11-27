# Spreadsheet API

Upload spreadsheets, map each worksheet to JSON with a versioned, API-doc-style schema,
import the rows into SQLite, and query them as a Web endpoint. Other applications can
control who reads what by signing access tokens.

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
| 1 | `POST /datasets/upload` | The file is validated and stored. Each non-empty worksheet becomes a **dataset**, and the response shows each one's detected header row, its columns and its first rows (30 unless `?rows=` says otherwise). |
| 2 | `PATCH /datasets/{id}/configure` | Saves a **mapping document** as a schema version and returns a fresh preview. Add `?dry_run=true` to preview only. `GET /datasets/{id}/preview` re-reads the file with the saved mapping. |
| 3 | `PATCH /datasets/{id}/import` | Imports every row. The source file is deleted once all of its worksheets are imported. |
| 4 | `GET /datasets/{id}` | Returns the imported rows, with filtering, sorting, full-text search and pagination. |

Also available: `GET /datasets/{id}/export?format=json|jsonl` (every row, streamed as a
download), `PATCH /datasets/{id}/access` (who may read the rows; see Access tokens), `GET /datasets` (the caller's upload history), `GET /auth/me` (account type,
profile, allowance and sign-in links), `GET /mapping/options` (the value lists for the mapping
and filter widgets), and `GET /` (the service's limits, the active mode, and every route by name with its method, path, path, query and body parameters and who may call it).

```bash
curl -F file=@catalogue.xlsx localhost:3000/datasets/upload
curl -X PATCH localhost:3000/datasets/$ID/configure -H 'content-type: application/json' -d @mapping.json
curl -X PATCH localhost:3000/datasets/$ID/import
curl -G localhost:3000/datasets/$ID --data-urlencode 'filter={"price.amount":{"$gte":3}}' --data-urlencode 'sort=-price.amount'
```

Upload also accepts an optional multipart field `source` holding JSON source options for every
sheet, for example `{"decimal_comma": true}`.

Previews return `PREVIEW_ROWS` rows (30) unless the request says otherwise: upload, configure
and preview all take `?rows=`, from 10 up to `MAX_PREVIEW_ROWS` (1000). A longer preview also
improves the detected column types, since a column that is empty in the first 30 rows has no
type to detect until a value turns up. `GET /mapping/options` lists the sizes worth offering
in a menu under `preview_rows`.

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

### Access tokens: letting another application control access

In either mode, a request may carry `Authorization: Bearer <jwt>`, a token signed by an
application registered in `ISSUERS_FILE`. The application holds an Ed25519 private key; this
service holds only the public key, and verifies each token with it.

```json
[{ "id": "mainusers", "name": "Main app", "plan": "service", "max_token_secs": 900,
   "keys": [{ "kid": "2026-11", "public_key": "<PEM, or the 32 raw bytes in base64>" }] }]
```

The file is the whole list: at startup each application is added or updated, a key no longer
listed is revoked, and an application no longer listed is disabled. A token is a JWT signed
with `EdDSA`:

```json
{ "iss": "mainusers", "sub": "acme/42", "aud": "spreadsheet-api",
  "iat": 1795729200, "exp": 1795730100,
  "grants": [{ "resource": "dataset/0199…", "actions": ["read", "export"],
               "constraints": { "filter": { "region": "north" }, "fields": ["sku", "price"], "max_page_size": 100 } }] }
```

The caller is the principal `<iss>/<sub>` (`mainusers/acme/42`), on the application's plan, and
may do only what the grants list.

| Part | Values |
|---|---|
| `resource` | `datasets` (everything the token's own principal owns, and the right to create more), `dataset/<id>`, `collection/<id>` |
| `actions` | `read` (query rows, list datasets), `export`, `upload`, `configure` (preview, mapping, visibility), `import` |
| `constraints` | For reading. `filter`: always applied, in either filter form. `fields`: the only fields returned. `max_page_size`: caps `limit` |

Two tokens cover most uses. The application's server sends a **management token**
(`datasets` with `upload`, `configure`, `import`, `read`) to load and shape a tenant's data. It
hands a browser or a partner a short-lived **access token** for one dataset, perhaps narrowed
to some rows and fields.

What the service enforces:

- **Isolation.** A token only reaches data owned by principals of its own issuer, whatever it claims.
- **Expiry.** `iat` and `exp` are required, and `exp - iat` may not exceed the issuer's
  `max_token_secs` (or `MAX_TOKEN_SECS`). `aud` must be `TOKEN_AUDIENCE`.
- **Revocation.** A token with a `jti` listed in the `revoked_tokens` table is refused. Short
  lifetimes are the main control; revoking a key or dropping the application from the file
  cuts off everything it signed.
- **Visibility.** Rows are readable by anyone with the dataset id unless the collection is
  private: `PATCH /datasets/{id}/access` with `{"visibility": "private"}` (a plan with
  `private_endpoints`). Then only the owner or a token granting `read` gets them, and
  everyone else gets `404`. Constraints only bind on private data: anyone can read unlisted
  data without a token.

A request that may not touch a dataset is answered `404`, as if it did not exist. A token
that fails to verify is answered `401` with the reason.

### Schema changes

`data/schema.sql` sets up a new database. Changes for existing databases go in
`data/migrations/*.sql`, which are applied once each, in name order, inside a transaction.
A new database records them all as applied.

The database is one file (`DB_PATH`) in WAL mode, so it can be read while the service runs:

```bash
sqlite3 data/sheets.sqlite "SELECT id, status, num_rows FROM datasets"
```

Rows are stored as JSONB; read them with `json(data)`, and a field with
`data ->> '$.price.amount'`.

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
| `source.max_rows` | Stops the import after this many rows (`-m`). The plan's row cap always applies. A longer worksheet is imported as far as the lower of the two, and the import response says so with `truncated: true` and the `row_limit`. |
| `fields[].source` | The column, given as its natural key, header text, A1 letter or column number. |
| `fields[].path` | The output path: `price`, `size.width` (nested) or `tags[]` (appended to a list). `key` accepts a raw KeySegment tree for anything more complex. |
| `fields[].type` / `format` / `precision` | `string`, `integer`, `number`, `boolean`, `date`, `datetime`, `time`, `array`, or any library format such as `d2`, `truthy(si,no)`, `dt:%d/%m/%Y`, `int[](\|)` (`-k col\|fmt`). |
| `fields[].default`, `datetime`, `decimal_comma` | Per-column overrides. |
| `fields[].omit_empty` | Leaves the key out of a row whose cell is empty. For numbers, yes/no values, dates and times that means null: nothing there, or nothing readable as the type. For text it means null or an empty string. A nested object left with nothing in it goes too. `omit_nulls` does this for nulls in every column. |
| `fields[].required`, `filterable`, `sortable`, `searchable`, `description` | Schema and query metadata. Rows missing a required value are imported but flagged invalid. |
| `patterns` | Placeholder patterns for columns with part of their name in common. A placeholder (`[n]` a number, `{name}` text, `*` anything) stands where the names differ, at the start, the end or in the middle, and the target refers to it: `file_[n]`→`files[]`, `sales_{region}`→`sales.{region}`, `{fuel}_price`→`price.{fuel}`, `sales_[year]`→`sales[].{year}{amount}` (the `.` may be left out: `sales[]{year}{amount}`), `{fuel}_price_note_{n}`→`price_{fuel}`. An empty cell adds nothing to a list: no value in `files[]`, and no `{ "year": 2024 }` item in a list of objects. | The upload, preview and configure responses list the patterns a sheet's own column names suggest as `pattern_suggestions`: columns that start or end the same, each with a ready `source` and `target` and the `columns` it matches.
| `exclude`, `exclude_paths` | Drop columns, ranges (`width..depth`) or every column a pattern fits (`*_note`, `internal_*`), or drop nested output paths (`-k -col`, `-K`). |
| `filter` | Only import rows that match. Accepts either a rule tree (as above) or spread-cli text such as `"age >= 18 and name ILIKE 'a%'"` (`-f`). |
| `omit_nulls` | Drop null values (`-X`). |
| `unmapped` | Set to `drop` to output only the listed columns. |
| `primary_key` | Output paths that identify a row. See the upsert section below. |
| `collection` | Import into an existing collection, given by ID or name, for example when this month's file has a new name. |

Each save creates or updates a row in `api_schema_versions`. Its `definition` holds a JSON
Schema (draft 2020-12) of one output row, with three keywords of its own: `x_source` on each
property, `x_primary_key`, and `x_mapping`, which holds the complete mapping document so every
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

How values compare:

- **By type.** A number is only greater or less than a number, and text than text. ISO dates
  are text and order correctly. A condition on a missing or empty value is false, so its
  negation is true.
- **Case.** In a rule tree, `eq`, `starts_with`, `ends_with` and `contains` ignore case unless
  `case_sensitive` is set, in any alphabet. MongoDB-style equality is exact.
- **Sorting.** Rows with nothing in the sort field come last, in either direction.
- **Search.** `q` finds rows containing every word, ignoring case and accents.
- **Indexes.** Filters and sorts read every row of the dataset; only the dataset, row order
  and primary key are indexed. That takes about 0.4 s on a million rows.

## Layout

```
src/sheet/mapping.rs     mapping document → spreadsheet-to-json OptionSet
src/sheet/reader.rs      previews and streaming reads (on the blocking thread pool)
src/sheet/spec.rs        type inference, field_spec, JSON Schema
src/sheet/key_pattern.rs   placeholder patterns: began as spread-cli's, now also matches by suffix and middle
src/sheet/suggest.rs       pattern rules suggested by the column names
src/sheet/filter_pattern.rs   ported verbatim from spread-cli
src/importer.rs          batched import, primary keys, validation, upsert merge
src/query.rs             filter/sort/search → parameterised SQL over the JSON rows
src/models/              the records as the routes use them
src/store.rs             queries, and the rules that depend on the caller's plan
src/db.rs                the SQLite pool, schema set-up and helpers
src/routes/              HTTP handlers
src/auth/                demo/service identity, access tokens and grants, OAuth sign-in, sessions, rate limiting
data/schema.sql          every table
data/migrations/         schema changes for existing databases
docs/models.md           how the tables fit together, their rules, and the shapes the API returns
```

`cargo test` runs the unit tests and end-to-end tests against an in-memory database: upload →
configure → import → query → upsert, service-mode tenants and URL/path intake, guest limits and
rate limiting, a full GitHub-style sign-in against a mock OAuth provider, access tokens and
grants, and how filters compare values.
`cargo run --example make_fixtures` regenerates `tests/fixtures`.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENCE-APACHE](LICENCE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT licence ([LICENCE-MIT](LICENCE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without
any additional terms or conditions.

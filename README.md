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
(`.ssv`, or `;`-delimited `.csv`: delimiters are detected from content).

## Flow

| Step | Endpoint | What happens |
|---|---|---|
| 1 | `POST /datasets/upload` | The file is validated and stored. Each non-empty worksheet becomes a **dataset**, and the response shows each one's detected header row, its columns and its first 30 rows. |
| 2 | `PATCH /datasets/{id}/configure` | Saves a **mapping document** as a schema version and returns a fresh preview. Add `?dry_run=true` to preview only. `GET /datasets/{id}/preview` re-reads the file with the saved mapping. |
| 3 | `PATCH /datasets/{id}/import` | Imports every row. The source file is deleted once all of its worksheets are imported. |
| 4 | `GET /datasets/{id}` | Returns the imported rows, with filtering, sorting, full-text search and pagination. |

Also available: `GET /datasets` (the caller's datasets), `GET /mapping/options` (the value
lists for the mapping and filter widgets), and `GET /` (a list of all routes).

Callers identify themselves with `X-User-Ref: <issuer>/<id>` (for example `google/10983`). The
issuer must exist in `issuers`. Principals are created on first contact and get their
issuer's default plan. A request with no header acts as `DEFAULT_USER_REF`. Plan limits and
features come from the schema, so the plan decides the rows per import, the imports per
hour, and whether primary keys, filtering, sorting and search are available.

```bash
curl -F file=@catalogue.xlsx localhost:3000/datasets/upload
curl -X PATCH localhost:3000/datasets/$ID/configure -H 'content-type: application/json' -d @mapping.json
curl -X PATCH localhost:3000/datasets/$ID/import
curl -G localhost:3000/datasets/$ID --data-urlencode 'filter={"price.amount":{"$gte":3}}' --data-urlencode 'sort=-price.amount'
```

Upload also accepts an optional multipart field `source` holding JSON source options for every
sheet, for example `{"decimal_comma": true}`.

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
| `fields[].source` | The column, given as its natural key, header text, A1 letter or column number |
| `fields[].path` | The output path: `price`, `size.width` (nested) or `tags[]` (appended to a list). `key` accepts a raw KeySegment tree for anything more complex. |
| `fields[].type` / `format` / `precision` | `string`, `integer`, `number`, `boolean`, `date`, `datetime`, `time`, `array`, or any library format such as `d2`, `truthy(si,no)`, `dt:%d/%m/%Y`, `int[](\|)` (`-k col\|fmt`) |
| `fields[].default`, `datetime`, `decimal_comma` | Per-column overrides |
| `fields[].required`, `filterable`, `sortable`, `searchable`, `description` | Schema and query metadata. Rows missing a required value are imported but flagged invalid. |
| `patterns` | Placeholder patterns such as `file_[n]`→`files[]`, `sales_{region}`→`sales.{region}`, `sales_[year]`→`sales[].{year}{amount}` |
| `exclude`, `exclude_paths` | Drop columns or ranges, or drop nested output paths (`-k -col`, `-K`) |
| `filter` | Only import rows that match. Accepts either a rule tree (as above) or spread-cli text such as `"age >= 18 and name ILIKE 'a%'"` (`-f`). |
| `omit_nulls` | Drop null values (`-X`) |
| `unmapped` | Set to `drop` to output only the listed columns |
| `primary_key` | Output paths that identify a row. See the upsert section below. |
| `collection` | Import into an existing collection, given by id or name, for example when this month's file has a new name |

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

MongoDB operators: `$eq $ne $gt $gte $lt $lte $in $nin $all $size $exists $regex`(+`$options`)
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
src/store.rs, db.rs      records and queries against data/schema.surql
src/routes/              HTTP handlers
```

`cargo test` runs the unit tests and an end-to-end test (upload → configure → import → query →
upsert) against an in-memory database. `cargo run --example make_fixtures` regenerates
`tests/fixtures`.

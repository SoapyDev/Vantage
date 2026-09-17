# Test suites

The JSON files in this folder describe endpoint test suites and double as a
syntax reference. The folder's contents are environment-specific and not
versioned (see `.gitignore`); only this README is. Groups (the `groups/`
folder) reference the files to run explicitly: a file that isn't listed is never
run automatically.

A full JSON Schema is provided at `schema/suite.schema.json`; wiring it into
your editor (see the root README) gives autocomplete and validation as you type.

## Suite structure

- `url`, `method`, `headers`, `ignored_fields`, `sorts`: defaults inherited by
  every step/test (`update_from`).
- `steps`: run first and are not asserted; they prime the dictionary (via
  `capture`).
- `tests`: run next, compared against `expected_status` (default 200) and
  `expected_response`.
- `before_all` / `after_all`: CLI actions run once around the whole suite.

## Content types

Every request carries a `content_type` (default `application/json`):

- `application/json`: the `payload` is sent as JSON.
- `application/x-www-form-urlencoded`: the `payload` is sent form-URL-encoded.
- `multipart/form-data`: **not supported yet**. A request with a multipart
  `payload` fails with an explicit error.

## Templates

| Syntax | Meaning |
|---|---|
| `{{name}}` | look up a variable in the dictionary |
| `{{name:type}}` | look up + cast (`int`, `float`, `bool`, `string`) |
| `{{name/ptr/to/field}}` | look up + descend into the value with a JSON pointer |
| `${var}` | name interpolation, resolved before the lookup, inside `{{ }}` and write keys |

Examples: `{{user_details/${id}/name:string}}`, dynamic capture key
`"{{id}}-detail"`.

A placeholder that spans the whole string keeps its JSON type
(`"{{price:float}}"` becomes a number). An **unresolved variable fails the
request** with a message listing the variables in scope (the run continues with
the remaining requests). `expected_response` is injected like the payload; the
`name` is cosmetic and stays lenient when a variable is missing.

## Capture

`capture` pulls values out of a response into the dictionary for later use:
`"capture": {"access_token": "/access_token"}` (name -> JSON pointer, an empty
pointer = the whole body). The key may itself be a template. A pointer that is
not found logs a WARN and is skipped (it does not fail the request).

## Loops: `for_each`

Any step or test may carry a `for_each` block:

- **`in`**: item source — either the name of an array variable already in the
  dictionary, or an inline request run once. `items_path` points to the array
  in its response (empty = the source itself).
- **`as`**: names the current item in scope (default `item`); read its fields
  with `{{item/field}}`. `index` holds the position.
- **`sequence`**: the loop body — an ordinary request sequence (same grammar as
  the suite: hooks, `capture`, even a nested `for_each`). Empty = the host
  request is the body (single-call loop).
- **`capture`**: the only values that escape the (isolated) iteration scope,
  accumulated as a map: `"capture": {"details": {"key": "{{item/id}}",
  "value": "{{detail}}"}}` builds `details = { "<id>": <detail> }`.
- **`limit`**: a cap on the number of iterations (**100 by default**).

Semantics: iterations run sequentially; a failing iteration does not stop the
loop; an empty list yields no result (success); a non-array source yields a
single failing result.

```json
{
  "name": "detail per item",
  "for_each": {
    "in": "items",
    "as": "item",
    "sequence": [
      {
        "name": "detail {{item/id}}",
        "url": "{{BASE_URL}}/detail",
        "payload": { "id": "{{item/id}}" },
        "capture": { "detail": "" }
      }
    ],
    "capture": {
      "details": { "key": "{{item/id}}", "value": "{{detail}}" }
    }
  }
}
```

## CLI actions

A step, a test, or the suite may declare actions: shell commands run
before/after.

- **Hooks**: `before` / `after` on a step/test, `before_all` / `after_all` on
  the suite. Sequential. `after` hooks run even when the test fails.
- **Fields**: `name`, `run` (template), `shell` (`sh`, `bash`, `powershell`,
  `cmd`...; default: powershell on Windows, sh elsewhere), `env` (map of
  templates), `capture` (variable receiving the trimmed stdout, JSON
  auto-parsed), `on_failure`, `timeout_ms` (default 10s), `cwd`.
- **`after` scope**: a full `result` object — `{{result/body/...}}`,
  `{{result/status}}`, `{{result/is_success}}`, `{{result/expected/...}}`,
  `{{result/error}}`, `{{result/duration}}`. Inside a `for_each`, actions also
  see the iteration scope (`{{item/...}}`, `{{index}}`); captures stay there
  (to surface them, use the `for_each`'s `capture`).
- **`on_failure`**: `continue` (default) = log a WARN, verdict unchanged;
  `fail` = the owning line becomes FAIL (a failing `before` skips the request
  and the remaining actions); `abort` = stop the suite.
- In compare mode, actions run twice (once per environment, capturing into the
  matching dictionary).
- Values injected into `run` are not escaped: internal tool, trusted suites
  only.

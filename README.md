# VANTAGE

A command-line tool for testing the HTTP endpoints for accuracy and performance. 
You describe requests and expected responses in JSON *suites*, pick an *environment*, 
and `vantage` runs them, compares the responses, and reports pass/fail. 
It can also compare two environments against each other and benchmark an endpoint's 
latency.

## Build

```shell
cargo build --release
```

The binary is produced at `target/release/cli.exe` (`target/release/cli` on
Linux/macOS). Use a debug build (`cargo build`) for faster compiles while
developing.

## Environments and secrets

Environments live in `environments.yaml` at the repo root. Each one is a flat
map of template variables exposed to suites as `{{VARIABLE}}` (currently
`BASE_URL`, `IDENTITY_SERVER_BASE_URL`, `CLIENT_ID`, `CLIENT_SECRET`,
`TENANT_ID`). Values may embed `${ENV_KEY}` placeholders, resolved at startup
from the process environment (loaded from a local `.env`), so **secrets stay in
`.env`, never in the repo**.

Resolution is lazy: a variable whose `${ENV_KEY}` is unset is simply skipped, so
you only need the secrets for the variables a given run actually uses. A
variable that *is* referenced but missing fails that request with an
unresolved-variable error listing what's in scope, rather than blocking startup.

`environments.yaml` is validated and embedded into the binary at build time, so
adding or changing an environment requires a rebuild. The valid
`-e/--environment` and `-c/--compare-with` names come from its keys.

`environments.yaml` is gitignored because it holds your own hosts and secret
names; a domain-agnostic template is committed as `environments.example.yaml`.
On a fresh checkout, copy it: `cp environments.example.yaml environments.yaml`,
then edit it for your services.

To run against a different set of environments **without rebuilding**, set
`VANTAGE_ENVIRONMENTS_FILE` to a YAML file with the same shape; it replaces the
embedded copy for that run. Its environment names may differ from the built-in
ones — with the override set, `-e/-c` accept any non-empty name and an unknown
one is reported (with the available list) when the file is loaded.

Create a `.env` in the repo root with the secrets referenced by
`environments.yaml`:

```
CLIENT_ID=...
CLIENT_SECRET=...
TENANT_ID=...
OTHER_ENV_CLIENT_ID=...
OTHER_ENV_CLIENT_SECRET=...
```

If you build with an email feature and use `--email`, the SMTP settings also
come from `.env` (see **Email reports** for the full reference). Only `SMTP_FROM`
plus either `SMTP_HOST` or a preset `SMTP_PROVIDER` are strictly required;
credentials are required for real sending:

```
# Email reports (only needed with a build --features email)
SMTP_FROM=no-reply@example.com
SMTP_HOST=smtp.office365.com
SMTP_PORT=587
SMTP_SECURITY=starttls
SMTP_USERNAME=...
SMTP_PASSWORD=...            # app password if the mailbox has MFA
# SMTP_PROVIDER=o365         # preset alternative to SMTP_HOST/PORT (needs the matching feature)
# SMTP_TRANSPORT=file        # write .eml to SMTP_FILE_DIR instead of sending (dev/testing)
# SMTP_FILE_DIR=./mail-outbox
```

These are read only when `--email` is used, so they can be omitted otherwise.

Output location is configurable too (not a secret, but convenient in `.env`):

```
# Base dir for reports/ and benchmarks/ (overridden by --output-dir)
VANTAGE_OUTPUT_DIR=./out
```

## Running

On a fresh checkout, scaffold the working folders and a starter suite:

```shell
.\target\release\cli.exe --init
```

This creates `sandbox/`, `test-suite/`, `groups/` and `reports/` when missing
and writes an annotated `sandbox/example.json` (existing files are left
untouched). Edit that example, then run it.

Pick *what* to run and *where*:

```shell
# a single suite against the `dev` environment
.\target\release\cli.exe -e dev -f .\test-suite\getUserById.json

# a group of suites
.\target\release\cli.exe -e dev -g .\groups\live-service.yaml

# everything in .\sandbox\ (created with an example on first use); uses the
# default `sandbox` environment
.\target\release\cli.exe --sandbox
```

Key options (`--help` lists them all):

- `--init` — scaffold the workspace (folders + `sandbox/example.json`) and exit.
  Scaffolds under `--output-dir` / `VANTAGE_OUTPUT_DIR` when set (default: the
  current directory); `--sandbox` looks in the same place.
- `-f, --file <PATH>` — run one suite. Mutually exclusive with `--group`.
- `-g, --group <PATH>` — run the suites listed in a group file (see `groups/`).
- `-s, --sandbox` — run every suite in `./sandbox/`.
- `-e, --environment <NAME>` — environment to run against (default `sandbox`).
- `-c, --compare-with <NAME>` — a second environment; enables **compare mode**
  (each request is sent to both and the two responses are diffed).
- `-r, --reports` — write an HTML report to `./reports` (a grouped run is
  aggregated into a single report) instead of printing to the console.
- `-o, --output-dir <DIR>` — base directory for generated output: reports go to
  `<DIR>/reports`, benchmarks to `<DIR>/benchmarks`. Precedence is
  `--output-dir` > `VANTAGE_OUTPUT_DIR` (env) > the current directory (default).
  The directory is created if missing; a path that exists as a file or is not
  writable fails fast with a clear error. It also relocates `--init`
  scaffolding and the `--sandbox` folder.
- `-n, --names <NAME[,NAME...]>` — run only the tests whose `name` matches an
  entry in this comma-separated list. Steps still run (so captures like auth
  tokens stay available); other tests are skipped. Matching is applied across
  the whole run, so in a group a name only needs to match one suite. A name
  that matches no test in any suite prints an error but does not stop the run.
  Works in every mode (file, group, sandbox, dry-run, benchmark).
- `--email <ADDR[,ADDR...]>` — after the run, email the HTML report to these
  addresses (comma-separated). Implies `--reports`. Subject is the group or
  suite name; the self-contained `index.html` is attached. Requires a build
  with an email feature (see **Email reports**). *Currently covers group and
  benchmark runs.*
- `-t, --threads <N>` — parallel suite workers (default: available cores − 1).
- `--metrics` — print a per-step timing table (load_suite, prepare, http,
  grade, extract, hooks, report). Works in every mode.
- `--dry-run` — resolve and print each request without sending anything; response captures show as `<captured:...>` (pre-flight). Not available with `--benchmark`.
- `--verbose` — more detail per request.

The process exits non-zero if any suite fails, so it drops into CI directly.

### Compare mode

```shell
.\target\release\cli.exe -e dev -c prod -g .\groups\live-service.yaml
```

Each request runs against both environments concurrently and the responses are
graded against each other (honoring the suite's `ignored_fields` and `sorts`).
Captures and hooks run once per side, each against its own variables.

## Benchmark mode

Characterize one endpoint's latency under a bounded request budget:

```shell
.\target\release\cli.exe -e dev -f .\test-suite\getUserById.json --benchmark --concurrency 16
```

- `-b, --benchmark [POOL_SIZE]` — requires `--file`. Runs a warm-up on the
  expected-fail tests, then a randomized pool of the expected-success ones,
  sequentially and then at doubling parallel levels, stopping early on HTTP 429
  or a >20% error rate. `POOL_SIZE` is optional (default 16); when it exceeds
  the eligible tests they are repeated to fill the pool.
- `--concurrency <LEVEL>` — highest parallel level to escalate to: one of 1, 2,
  4, 8, 16, 32, 64, 128 (default 8), capped to the machine's cores.

A summary is printed and an HTML report is written under `./benchmarks`.

### Load profiles (`--load`)

The escalation above is *closed-loop*: it fires as fast as the machine allows
to find the ceiling. A **load profile** is *open-loop* instead — requests are
scheduled at a target rate (calls per second) that follows a curve over
wall-clock time, independent of how fast responses come back. This answers
"how does the service behave under a controlled, realistic load over time?".

```shell
# escalation, then a custom curve: ramp to 30 CPS over 3 min, then hold 2 min
cli -e dev -f ./test-suite/example-load.json --benchmark --load "ramp:3m:30,hold:2m"

# a preset, load profile only (escalation disabled)
cli -e dev -f ./test-suite/example-load.json --benchmark --load spike --no-escalation
```

Flags (all require `--benchmark`):

- `--load <profile>` — a spec string of comma-separated stages
  (`ramp:3m:30,hold:2m`) or a preset name. Overrides a `load` block declared in
  the suite.
- `--no-escalation` — skip the escalation phases and run the profile alone.
  Requires a profile (from `--load` or the suite).
- `--max-in-flight <n>` — cap on concurrent in-flight requests (default 256), so
  a slow server cannot pile up unbounded tasks.
- `--no-adaptive-stop` — keep running through HTTP 429s / high error rates
  (by default the run stops on either, evaluated per one-second bucket).

| Flags | Behavior |
|-------|----------|
| `--benchmark` | escalation phases (default) |
| `--benchmark --load <p>` | escalation phases, then the load profile |
| `--benchmark --load <p> --no-escalation` | load profile only |

A suite whose `load` block supplies the profile can drop the `--load` flag:
`--benchmark` then runs escalation + the suite's profile, and
`--benchmark --no-escalation` runs the suite's profile alone.

**Stage grammar:** `<shape>:<duration>[:<target_cps>[:<period>]]`. Durations take
an `s`, `m`, or `h` suffix (`30s`, `3m`, `1h`).

| Shape  | Meaning |
|--------|---------|
| `ramp` | linear from the previous stage's CPS (0 initially) to `target` — down as well as up |
| `hold` | constant at `target` when given, otherwise the previous stage's CPS |
| `step` | jump instantly to `target` and hold for the duration |
| `sine` | oscillate between the previous CPS and `target`, one full wave per `period` (e.g. `sine:5m:30:1m`) |

**Presets** (each expands to the same grammar):

| Preset   | Expansion | Use case |
|----------|-----------|----------|
| `smoke`  | `hold:30s:1` | sanity at trivial load |
| `ramp`   | `ramp:3m:30,hold:2m` | find the degradation point |
| `spike`  | `hold:1m:5,step:30s:50,hold:1m:5` | burst recovery |
| `stairs` | `step:1m:5,step:1m:10,step:1m:20,step:1m:40` | capacity in increments |
| `soak`   | `ramp:1m:10,hold:10m` | sustained-load leaks |

The same profile can be declared in the suite file (the CLI `--load` wins on
conflict):

```json
{
  "load": {
    "stages": [
      { "shape": "ramp", "duration": "3m", "target_cps": 30 },
      { "shape": "hold", "duration": "2m" }
    ]
  }
}
```

When a load profile runs, the HTML report gains two time-series graphs —
throughput (target CPS vs achieved) and response time over time (median with a
P90–P99 band, errors flagged) — and `data.json` keeps every raw sample with its
time offset for external analysis.

## Email reports

Emailing is **opt-in at compile time** so the mail dependency is never in a
default build. Build the CLI with the `email` feature (plus any provider
presets you want):

```shell
cargo build --release --features email                 # generic SMTP
cargo build --release --features email-o365            # + Office 365 preset
cargo build --release --features email-all             # + o365, gmail, protonmail
```

Then run with `--email`:

```shell
cli -e test -g ./groups/live-service.yaml \
    --email reports@example.com
```

Configuration comes from the environment (so secrets stay in `.env`):

- `SMTP_FROM` — the sender address (required).
- `SMTP_HOST` / `SMTP_PORT` — relay host/port. Optional if `SMTP_PROVIDER` is set.
- `SMTP_PROVIDER` — a compiled-in preset: `o365`, `gmail`, or `protonmail`
  (fills in host/port/security). Requires the matching feature.
- `SMTP_SECURITY` — `starttls` (default), `tls`, or `plain`.
- `SMTP_USERNAME` / `SMTP_PASSWORD` — credentials (required for real sending;
  for Microsoft 365 / Gmail with MFA this is an app password).
- `SMTP_TRANSPORT` — `smtp` (default) or `file`. `file` writes each message as
  an `.eml` into `SMTP_FILE_DIR` (default `./mail-outbox`) instead of sending —
  handy for local development and testing, or point `SMTP_HOST` at a local
  catcher like Mailpit.

Notes:

- The mail transport is built lazily, once, on the first message — nothing
  mail-related is touched on runs that do not email.
- A send failure logs a warning and does **not** change the run's pass/fail
  verdict; the report is still written to `./reports`.
- Deliverability requires an *authorized* relay for your domain (an internal
  relay or Microsoft 365). A self-hosted "temporary" mail server cannot reliably
  deliver to real inboxes (SPF/DKIM/DMARC, reverse DNS, IP reputation), so it is
  only useful as the `file` transport for testing.

## Writing suites

A suite is a JSON file (examples in `test-suite/`). A JSON Schema for the format
lives at `schema/suite.schema.json` — wiring it into your editor gives
autocomplete and inline validation, so a typo'd field is flagged as you type
instead of at run time. The quickest way is a `$schema` pointer (path relative
to the suite file):

```json
{
  "$schema": "../schema/suite.schema.json",
  "url": "{{BASE_URL}}/api/...",
  "tests": []
}
```

Editors that honor `$schema` (VS Code and others) then validate automatically.
Or map it once: **RustRover / IntelliJ** — Settings → Languages & Frameworks →
Schemas and DTDs → JSON Schema Mappings, add `schema/suite.schema.json` with the
patterns `test-suite/*.json` and `sandbox/*.json`. **VS Code** — add a
`json.schemas` entry in `.vscode/settings.json`.

### Structure

A suite carries defaults (`url`, `method`, `headers`, `ignored_fields`,
`sorts`) that each request inherits unless it overrides them, plus:

- **`steps`** — setup requests run first. They are *not* asserted; their job is
  to populate the variable dictionary (e.g. fetch an auth token).
- **`tests`** — the asserted requests. Each compares the response against its
  `expected_response` (and `expected_status`, default 200), after removing
  `ignored_fields` and applying `sorts`.
- **`before_all` / `after_all`** — CLI actions run once around the whole suite.

```json
{
  "url": "{{BASE_URL}}/api/...",
  "method": "POST",
  "ignored_fields": ["$id"],
  "steps": [
    {
      "name": "Get auth token",
      "url": "{{IDENTITY_SERVER_BASE_URL}}/{{TENANT_ID}}/oauth2/v2.0/token",
      "content_type": "application/x-www-form-urlencoded",
      "payload": {
        "client_id": "{{CLIENT_ID}}",
        "client_secret": "{{CLIENT_SECRET}}",
        "grant_type": "client_credentials",
        "scope": "{{BASE_URL}}/.default"
      },
      "capture": { "access_token": "/access_token" }
    }
  ],
  "tests": [
    {
      "name": "fetch a known user",
      "headers": { "Authorization": "Bearer {{access_token}}" },
      "payload": { "id": "42" },
      "expected_response": { "name": "Ada Lovelace" }
    }
  ]
}
```

### Templating

Any string in a request can reference dictionary variables:

- `{{VAR}}` — insert a variable. When the whole string is one placeholder, its
  JSON type is preserved (`{{obj}}` stays an object).
- `{{VAR/json/pointer}}` — read a nested value with a JSON pointer.
- `{{VAR:int}}` / `:float` / `:bool` / `:string` — cast the value.
- `{{${name}-suffix}}` — build a variable name from another variable.

An unresolved variable fails that request with a clear message listing the
variables in scope; the rest of the run continues.

### Capturing values

`capture` pulls values out of a response into the dictionary for later use:
`"capture": { "name": "/json/pointer" }` (an empty pointer captures the whole
body). The name may itself be templated. A pointer that doesn't match logs a
warning and is skipped.

### Loops (`for_each`)

Any request can run once per item of a list:

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

- `in` — an array variable name, or an inline request that fetches one
  (`items_path` points to the array in its response).
- `as` — names the current item (default `item`); read fields with
  `{{item/field}}`.
- `sequence` — the loop body: an ordinary request list (same grammar as the
  suite, so it can nest hooks, captures, and further `for_each`). Empty = the
  host request is the body.
- `capture` — the only values that escape the (otherwise isolated) iteration
  scope, accumulated as a keyed map.

### Actions (hooks)

`before` / `after` on a request, and `before_all` / `after_all` on the suite,
run shell commands whose `run`, `env`, and `name` are templated:

```json
{
  "after": [
    { 
      "name": "log total", 
      "run": "echo '{{result/body/total}}' >> results.csv"
    }
  ]
}
```

After-hooks additionally see a `{{result/...}}` object (`body`, `status`,
`is_success`, `error`, ...). An action can `capture` its stdout into a variable,
and `on_failure` controls what a failure does: `continue` (default, warn),
`fail` (fail the owning step/test), or `abort` (stop the run).

## Testing the tool

```shell
cargo test --workspace
```

## Benchmarking the tool itself

Criterion benches cover the CLI's own hot paths (template engine, suite
parsing, pool building, and the full pipeline against a local mock server from
`crates/mock-server`):

```shell
cargo bench                                                    # run all benches

# before/after comparison of a change (scope to the criterion bench so the
# default libtest harnesses don't reject the --baseline flag):
cargo bench -p runner --bench pipeline -- --save-baseline before
cargo bench -p runner --bench pipeline -- --baseline before
```

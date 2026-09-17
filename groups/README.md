# Groups

The YAML files in this folder group several test suites (from `test-suite/`)
under one name, to run them in a single command. A group references the files to
run explicitly: a suite that isn't listed is never run by the group.

Like the suites, this folder's contents are environment-specific and not
versioned (see `.gitignore`); only this README is. Everyone provides their own
groups locally.

## Group structure

```yaml
name: Live services
files:
  - ./test-suite/getUserById.json
  - ./test-suite/listOrders.json
  - ./test-suite/createOrder.json
```

- `name`: the group's display name. With `--reports` it titles and names the
  single aggregated report.
- `files`: an ordered list of paths to JSON suites. Paths are resolved relative
  to the current working directory (where the command runs), hence the `./`
  prefix.
- `config` (optional): one or more run profiles that the group runs over all of
  its files — see below.

For the suite syntax itself, see `test-suite/README.md` and the schema at
`schema/suite.schema.json`.

## Run config (`config`)

A group can bake its run configuration into the file, so calling the group
applies that config to every suite it lists. `config` is a flag string, or a
list of them; each string is a **profile** parsed exactly like command-line
flags:

```yaml
name: Nightly checks
config:
  - "--reports"            # run the suites and write the aggregated report
  - "--benchmark --metrics" # then benchmark each file, with per-step metrics
files:
  - ./test-suite/getInventory.json
  - ./test-suite/listItems.json
```

- A single profile can be written inline: `config: "--reports"`.
- Each profile runs over **all** the group's files, in order, one profile after
  another. The example above first runs the suites with a report, then
  benchmarks each file — a benchmark profile benchmarks every file in turn.
- Only **run-mode** flags are allowed in a profile: `--reports`,
  `--benchmark [POOL_SIZE]`, `--concurrency`, `--metrics`, `--verbose`,
  `--threads`. Target selectors (`--file`/`--group`/`--sandbox`), `--environment`
  and `--compare-with` are command-line only; a profile that sets one is a hard
  error. This keeps a group about *how* it runs, not *where* it points.
- The command line is layered on top of each profile: an explicit value wins
  for `--benchmark`/`--concurrency`/`--threads`, and boolean flags are added
  (e.g. running the group with `--verbose` makes every profile verbose). There
  is no way to switch a profile's flag off from the command line.
- Without `config`, the group runs once, driven entirely by the command line
  (the previous behavior).

## Running

```shell
cli -e <environment> -g ./groups/<group>.yaml
```

- `-e/--environment` is optional (default `sandbox`).
- `-g/--group` is mutually exclusive with `-f/--file` and `-s/--sandbox`.
- Each listed suite is loaded and run; a failing suite is reported but does not
  stop the others, and the run exits non-zero so CI notices.
- With `--reports`, all the group's suites are aggregated into a single HTML
  report, titled and named after `name`. Without `--reports`, each suite prints
  to the console.
- `-c/--compare-with <environment>` also works on a group: each suite is run
  against both environments and diffed.

# Privacy

**oprobe sends no telemetry.** No usage statistics, crash reports or analytics, and no setting to
turn any on. Neither does `oprobe-trace`.

## What oprobe connects to

| When | Where |
|---|---|
| `run`, `prove`, the app | the storage endpoint of the operator you give it (and your proxy, if `HTTPS_PROXY` or `http.proxy` is set); that cloud's credential providers (instance metadata, STS, SSO) when the credential chain needs them |
| `demo`, `lab`, `lab://…` operators | nothing: the lab runs inside oprobe on 127.0.0.1 |
| `show`, `show --list`, `show --rm` | nothing |

Nothing else. oprobe never checks for updates by itself; the install scripts talk to GitHub when
you run them.

## What oprobe stores

- **Runs** in `<data dir>/oprobe/runs/` (or `$OPROBE_DATA_DIR`): the report, the operator spec,
  the workload and one row per operation and request. Object keys, request IDs and remote IP
  addresses are part of a run. Secrets are not: credential options (`secret_access_key`,
  `account_key`, `sas_token`, `password`, `*_key`, …) are redacted before anything is printed or
  saved, and URLs lose embedded credentials and query strings.
- **The host** is recorded as OS, architecture, CPU count and a hash of the host name, never the
  name itself.
- **The config file** holds what you put there.

Delete runs with `oprobe show --rm RUN…`, or remove the data directory.

## `.oprobe` files from `oprobe-trace`

`Handle::save` writes what the layer saw in your process: the aggregates and a sample of
operations and their requests, with their API, method, status, timing, bytes, connection and
request ID. Object keys are never stored: an operation's key is its first path segment (at most
32 bytes) and a hash of the whole key (`logs/#3f2a…`), so a top-level directory name can be in
the file, but not a key. Request IDs and remote addresses are. Credentials never are. Review a
file before you attach it to an issue.

## Test data in your bucket

`oprobe run` writes under `<prefix>/oprobe-run-<id>/` and deletes it at the end, verifying the
prefix is empty. `--read-only` and `--existing` never write; `--dry-run` sends nothing. A run
killed hard (a second Ctrl-C, OOM, power loss) can leave its prefix behind; delete it with your
usual tools.

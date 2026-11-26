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
  saved. URLs lose their credentials wherever they appear, in an operator, an option, a proxy
  setting or the text of an error: `user:password@`, and the query parameters that carry a
  signature or a token (`X-Amz-Signature`, `X-Amz-Credential`, `X-Amz-Security-Token`,
  `Signature`, `AWSAccessKeyId`, an Azure SAS's `sig`, `X-Goog-Signature`, `x-oss-signature`,
  `q-signature`, `access_token`, `token`, …, whatever their case). What only describes a
  request stays readable (`X-Amz-Expires`, an SAS's `se` and `sp`). Errors, warnings and log
  events are printed the same way.
- **The host** is recorded as OS, architecture, CPU count and a hash of the host name, never the
  name itself.
- **Pending-cleanup markers** in `<data dir>/oprobe/pending/`: while a run that writes is going,
  one small file names its target and its `oprobe-run-*` prefix. It is removed once the test data
  is deleted; one left by a killed run tells the next run against that target what to delete.
- **The config file** holds what you put there.

Delete runs with `oprobe show --rm RUN…`, or remove the data directory.

## `.oprobe` files from `oprobe-trace`

`Handle::save` writes what the layer saw in your process: the aggregates and a sample of
operations and their requests, with their API, method, status, timing, bytes, connection and
request ID. Object keys are never stored: an operation's key is its first path segment (at most
32 bytes) and a hash of the whole key (`logs/#3f2a…`), so a top-level directory name can be in
the file, but not a key. Request IDs and remote addresses are. Credentials never are: the
text of a failed request or operation loses the credentials of every URL in it (the signature
and token of a presigned or SAS URL included) when it is recorded, so neither an `Exporter`,
a `Snapshot` nor a file ever holds them. Review a file before you attach it to an issue.

## Test data in your bucket

`oprobe run` writes under `<prefix>/oprobe-run-<id>/` and deletes it at the end, verifying the
prefix is empty. `--read-only` and `--existing` never write; `--dry-run` sends nothing. A run
killed hard (a second Ctrl-C, OOM, power loss) can leave its prefix behind; delete it with your
usual tools.

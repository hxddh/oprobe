# Privacy

**oprobe sends no telemetry.** No usage statistics, crash reports or analytics, and no setting to turn any on.

## What oprobe connects to

| When | Where |
|---|---|
| `run`, `tune`, `doctor`, `target test`, `probe`, `watch`, `matrix`, `compat`, `cleanup`, the app | the storage endpoint you give it (and your proxy, if `HTTPS_PROXY` / `--proxy` is set); credential providers of that cloud (instance metadata, STS, SSO) when the provider chain needs them |
| `demo`, `lab`, `lab://…` targets | nothing: the lab runs inside oprobe on 127.0.0.1 |
| `self-update` | `api.github.com` and GitHub's release download host (set `OPROBE_UPDATE_API` to use a mirror) |
| the GitHub Action | GitHub (to download a pinned release and comment on the pull request) |

Nothing else. The HTML report is one self-contained file: opening it loads nothing from the network.

## What oprobe stores

- **Runs** in `<data dir>/oprobe/runs/` (`oprobe config path`): the report, the resolved configuration and one
  row per operation and request. Object keys, request IDs and remote IP addresses are part of a run; secrets
  are not: credential options (`secret_access_key`, `account_key`, `sas_token`, `password`, `*_key`, …) are
  redacted before anything is printed or saved, and URLs lose embedded credentials and query strings.
- **The host** is recorded as OS, architecture, CPU count and a hash of the host name, never the name itself.
- **The config file** holds what you put there (`oprobe target add` stores the URL, region, endpoint, the
  credential *source* such as `profile:prod`, and options — never keys unless you write them yourself).

Delete runs with `oprobe runs rm` / `oprobe runs gc`, or remove the data directory.

## Test data in your bucket

`oprobe run` writes under `<prefix>/oprobe-run-<id>/` and deletes it at the end, verifying the prefix is empty.
`--read-only` and `--existing` never write; `--dry-run` sends nothing; `oprobe doctor` writes only with
`--write-probe`. Leftovers from killed runs are listed with `oprobe cleanup TARGET` (a dry run) and
removed with `oprobe cleanup TARGET --yes`.

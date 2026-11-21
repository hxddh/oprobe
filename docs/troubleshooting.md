# Troubleshooting

Start with the doctor:

```bash
oprobe target test prod            # a configured target
oprobe doctor s3://bucket/prefix   # any URL
```

It checks configuration, credentials, the network path (DNS, proxy, TCP, TLS, HTTP), clock skew and
list/write/read/stat/delete permissions one by one, and prints a fix for each failure (`--read-only` skips
the writes). Every error of every command carries a stable code (`-o json`: `error.code`) and usually a
`hint:` line.

## Errors

| Code / symptom | Cause | Fix |
|---|---|---|
| `usage.unknown_target` | the name is not in the config file | `oprobe target list`; pass a URL; `oprobe target add NAME URL` |
| `usage.unknown_run` | no run matches the RUNREF | `oprobe runs list`; use more characters of the id |
| `usage.unknown_baseline` | no baseline has that name | `oprobe baseline list`; `oprobe baseline set NAME RUNREF…` |
| `config.invalid`, `config.unknown_option` | a typo in the config file or an option the service ignores | `oprobe config edit` (validated on save); the error names the closest valid option |
| `auth.no_credentials`, `auth.denied`, `AccessDenied`, `SignatureDoesNotMatch` | missing or wrong credentials, IAM | `--credentials profile:NAME`, export `AWS_*` / `GOOGLE_APPLICATION_CREDENTIALS` / `AZURE_STORAGE_*`; oprobe needs list/read/write/delete on the prefix — or measure read-only with `--existing PREFIX`, or `--anonymous` for public buckets |
| `net.tls`, `invalid peer certificate: UnknownIssuer` | private CA or a TLS-inspecting proxy | `--ca-cert corp.pem` (or `SSL_CERT_FILE`); `--insecure` for development endpoints only |
| `net.dns`, `no addresses` | wrong endpoint, or a network where only the proxy resolves names | check `--endpoint`; set `HTTPS_PROXY` |
| `net.unreachable`, `connection refused` | nothing listening | check the endpoint and port; is MinIO / `oprobe lab` running? |
| `proxy refused CONNECT` | egress proxy policy or credentials | check `HTTPS_PROXY` (with `user:pass@`), `NO_PROXY`, or `--proxy none` |
| `RequestTimeTooSkewed` | local clock off by > 15 min | enable NTP; `oprobe doctor` shows the skew |
| `PermanentRedirect`, `AuthorizationHeaderMalformed`, `region is missing` | wrong region | `--region` or `AWS_REGION`; R2 uses `auto` |
| `storage.throttled`, many 429 / 503 SlowDown | rate limiting | lower `-c` / `--max-inflight`, or `oprobe tune` |
| `safety.read_only` (exit 5) | a write under `--read-only` / `--existing` | drop the flag, or use a read-only preset |
| `safety.budget_exceeded` (exit 4) | a budget stopped the run | raise `--max-*`; the partial run is saved |
| exit 3 after a run | the SLA was breached (> 1% failed operations or corrupted data by default) or `--fail-on` matched | read the findings; `--sla none` reports without failing |

## Situations

| Situation | What to do |
|---|---|
| leftover `oprobe-run-*` prefixes | a run was killed (second Ctrl-C, OOM, power loss): `oprobe cleanup TARGET --dry-run`, then without `--dry-run` |
| "client processing-bound" on a remote store | client CPU, CRC verification or tiny objects: check `client cpu` in `runs show --detail`; `--no-verify`; larger objects |
| `oprobe run` asks questions in a script | it never does: prompts appear only on an interactive terminal with text output; pass a target, or `--non-interactive` |
| `oprobe top` fails in CI | it needs an interactive terminal; use `oprobe run` |
| `cannot open a window` / `built without native windows` | Linux server packages are headless: `oprobe top`, the `oprobe-desktop` tarball, or `cargo install oprobe --features gui` |
| `runs open` says no display | over SSH: `oprobe runs show RUNREF`, or `oprobe top --open FILE` in the terminal |
| `config edit` opens the wrong editor | set `VISUAL` or `EDITOR` (`export EDITOR="code -w"`) |
| `self-update` refuses | oprobe was installed by a package manager; the message names its update command (`brew upgrade oprobe`, `winget upgrade oprobe`, …) |
| `self-update` rate limited | GitHub's anonymous API limit: wait an hour or set `GITHUB_TOKEN` |
| macOS: "cannot be opened because the developer cannot be verified" / Windows SmartScreen | the packages are unsigned: [first open](install.md#first-open-of-unsigned-packages) |

## Error codes

Every error carries one of these codes (`-o json`: `error.code`, plus `error.docs` linking here).
The exit code follows the code's class: `2` usage/config, `1` runtime, `3` gate, `4` budget,
`5` safety refusal, `130` interrupted.

<a id="usage-invalid_argument"></a>
### `usage.invalid_argument` (exit 2)

A flag, argument or tool input is malformed: an unknown flag, a value that does not parse
(`--max-bytes lots`), an output format the command does not have (`-o html` where there is no
HTML renderer). With `-o json|ndjson` argument errors are JSON too. Fix the argument; `--help`
lists what the command accepts.

<a id="usage-unknown_target"></a>
### `usage.unknown_target` (exit 2)

The target name is not in the config file, or no target was given. `oprobe target list`; pass a
URL (`s3://bucket/prefix`, `memory://`); `oprobe target add NAME URL`.

<a id="usage-unknown_run"></a>
### `usage.unknown_run` (exit 2)

No stored run matches the RUNREF. `oprobe runs list`; use more characters of the id, `last`,
`last~N`, a label or a path to a `.oprobe` file.

<a id="usage-unknown_baseline"></a>
### `usage.unknown_baseline` (exit 2)

No baseline has that name. `oprobe baseline list`; create one with `oprobe baseline set NAME
RUNREF…` or `oprobe run … --baseline NAME`.

<a id="config-invalid"></a>
### `config.invalid` (exit 2)

The config file (or a profile) does not parse or has an invalid value, e.g. an unknown key under
`[defaults]` or `[defaults.budgets]`. `oprobe config edit` validates on save; `oprobe config show`
prints every effective value and its source.

<a id="config-unknown_option"></a>
### `config.unknown_option` (exit 2)

A backend option the service does not know (`--option`, `[targets.NAME.options]`). The message
names the closest valid option.

<a id="auth-no_credentials"></a>
### `auth.no_credentials` (exit 1)

No credentials were found. Export the provider's variables (`AWS_*`,
`GOOGLE_APPLICATION_CREDENTIALS`, `AZURE_STORAGE_*`), set `credentials = "profile:NAME"` on the
target, or use `--anonymous` for public buckets.

<a id="auth-denied"></a>
### `auth.denied` (exit 1)

The service refused the credentials or the operation (`AccessDenied`, `SignatureDoesNotMatch`,
403). oprobe needs list/read/write/delete on the prefix; measure read-only with
`--existing PREFIX`. `oprobe doctor TARGET` checks each permission.

<a id="net-unreachable"></a>
### `net.unreachable` (exit 1, retryable)

Nothing answered (`connection refused`, reset, no route). Check the endpoint and port; is MinIO /
`oprobe lab` running? Behind an egress proxy check `HTTPS_PROXY` / `NO_PROXY` or `--proxy none`.

<a id="net-dns"></a>
### `net.dns` (exit 1, retryable)

The endpoint's name did not resolve. Check `--endpoint`; on networks where only the proxy
resolves names set `HTTPS_PROXY`.

<a id="net-tls"></a>
### `net.tls` (exit 1)

The TLS handshake failed (`invalid peer certificate: UnknownIssuer`, expired or mismatched
certificate): a private CA or a TLS-inspecting proxy. `--ca-cert corp.pem` (or `SSL_CERT_FILE`);
`--insecure` for development endpoints only.

<a id="net-timeout"></a>
### `net.timeout` (exit 1, retryable)

A request or connection timed out. Check the network path with `oprobe doctor`; lower the
concurrency if the client or the link is saturated.

<a id="storage-not_found"></a>
### `storage.not_found` (exit 1)

The bucket, container, root or `--existing` prefix does not exist, or holds no objects. Check the
URL and region; with `--existing PREFIX` list the prefix first.

<a id="storage-throttled"></a>
### `storage.throttled` (exit 1, retryable)

The service rate-limited the requests (429, 503 SlowDown). Lower `-c` / `--max-inflight`, or let
`oprobe tune` find the limit.

<a id="storage-unsupported"></a>
### `storage.unsupported` (exit 1)

The service does not support an operation the workload needs (e.g. multipart, copy, presign).
Pick another preset; `oprobe doctor` prints the service's capability matrix.

<a id="safety-budget_exceeded"></a>
### `safety.budget_exceeded` (exit 4)

A budget (`--max-cost-usd`, `--max-requests`, `--max-bytes`, `--max-duration`, or
`[defaults.budgets]`) stopped the command. Budgets cover the whole command: every run of
`run --repeat`, every cell of `matrix`, every window of `watch`. The partial run is saved and
test data cleaned up. Raise the budget or shorten the run; `--dry-run` shows the estimates.

<a id="safety-read_only"></a>
### `safety.read_only` (exit 5)

The command would write or delete under `--read-only` / `--existing`, or on a read-only MCP
server. Drop the flag, benchmark existing objects with `--existing PREFIX`, or start the MCP
server with `--allow-write`.

<a id="safety-refused"></a>
### `safety.refused` (exit 5)

A safety rail refused the command: a target outside the MCP server's `--allow-target` list (or a
call that overrides the endpoint, bucket, root, proxy or TLS settings of an allowed target), or
`oprobe mcp --http` on a non-loopback address without `--http-allow-remote`.

<a id="run-sla_failed"></a>
### `run.sla_failed` (exit 3)

The run breached its SLA (by default > 1% failed operations or corrupted data) or `--fail-on`
matched. Read the findings; `--sla none` reports without failing.

<a id="run-regression"></a>
### `run.regression` (exit 3)

`oprobe compare` (or `run --baseline`) found a significant regression against the baseline.
`oprobe compare A B` shows each check; see [CI](ci.md) for thresholds.

<a id="run-interrupted"></a>
### `run.interrupted` (exit 130, retryable)

Ctrl-C, Stop or `cancel_run` ended the command. Test data was cleaned up unless Ctrl-C was
pressed twice (then `oprobe cleanup TARGET`). Run it again.

<a id="internal"></a>
### `internal` (exit 1)

An unexpected failure. Rerun with the same arguments; if it persists, report it with the
command line and the error message.

[Reading the report](reading-the-report.md) explains each verdict and section.

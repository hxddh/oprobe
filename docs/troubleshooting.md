# Troubleshooting

`oprobe run` checks the operator before it measures (a preflight: credentials, the network path,
list and write permission on the prefix). When something is wrong it stops with an error that
carries a stable code (`-o json`: `error.code`, plus `error.docs` linking to this page) and usually
a `hint:` line. `--dry-run` shows the plan without sending a request.

## Common problems

| Code / symptom | Cause | Fix |
|---|---|---|
| `usage.unknown_target` | the `OPERATOR` is neither a URL, an `op.toml` file nor a configured name | pass a URL (`s3://bucket/prefix`), a file, or define `[operators.NAME]` in the config file |
| `usage.unknown_run` | no run matches the reference | `oprobe show --list`; use more characters of the id |
| `config.invalid`, `config.unknown_option` | a typo in `op.toml` or the config file, or an option the service does not know | the error names the key and the closest valid one ([op.toml](operator.md)) |
| `auth.no_credentials`, `auth.denied`, `AccessDenied`, `SignatureDoesNotMatch` | missing or wrong credentials | export `AWS_*` / `GOOGLE_APPLICATION_CREDENTIALS` / `AZURE_STORAGE_*`, or set them in `[options]`; oprobe needs list/read/write/delete on the prefix, or measure read-only with `--existing PREFIX` |
| `net.tls`, `invalid peer certificate: UnknownIssuer` | a private CA or a TLS-inspecting proxy | `http.ca_cert = "corp.pem"` in `op.toml` (or `--set http.ca_cert=corp.pem`); `http.insecure = true` for development endpoints only |
| `net.dns`, `no addresses` | a wrong endpoint, or a network where only the proxy resolves names | check `options.endpoint`; set `HTTPS_PROXY` |
| `net.unreachable`, `connection refused` | nothing listening | check the endpoint and port; is MinIO or `oprobe lab` running? |
| `RequestTimeTooSkewed` | the local clock is off by more than 15 minutes | enable NTP |
| `PermanentRedirect`, `AuthorizationHeaderMalformed`, `region is missing` | wrong region | `--set options.region=…` or `AWS_REGION`; R2 uses `auto` |
| `safety.read_only` (exit 5) | a write under `--read-only` / `--existing` | drop the flag, or profile existing objects with `--existing PREFIX` |
| `safety.budget_exceeded` (exit 4) | a budget stopped the run | raise `--max-*`; the partial run is saved |
| exit 3 after `prove` | the change is worse on the objective | read the evidence; try another value with `--with` |

## Situations

| Situation | What to do |
|---|---|
| leftover `oprobe-run-*` prefixes | a run was killed hard (a second Ctrl-C, OOM, power loss): the next run against the same target deletes them (it says so), except under `--read-only`; or delete the prefix with your usual tools |
| `client_bound` on a remote store | the profiling process is the bottleneck (CPU, tiny objects); the profile's `self` section says why. Larger objects or fewer workers |
| `client_bound`: "cannot tell too few workers from a service at its capacity" | an open loop (`--rate`) fell behind with every worker busy: a steady overload shows one level of requests in flight. Vary the load (`--set shape=ramp:FROM..TO/D`): a service that queues is then `throttling`, one with room leaves the workers as the bound |
| "N of M scheduled arrivals never completed" and a Critical verdict | the open loop stalled: more than 1 % of its arrivals never completed, so its p99 has no upper bound. Lower `--rate`, or see the finding beside it for who fell behind |
| `oprobe` asks nothing in a script | it never prompts: pass an operator, and use `--non-interactive` to rule out redraws |
| `cannot open a window` | no display (SSH, a server): the app opens in the terminal; Linux packages built with `--features gui` need GTK 4 |
| `oprobe show FILE` fails on a file from `oprobe-trace` | the file is from a newer major version, or was cut short; write it again with `Handle::save` |
| macOS "cannot be opened because the developer cannot be verified" / Windows SmartScreen | the packages are unsigned: [first open](install.md#first-open-of-unsigned-packages) |

## Error codes

Every error carries one of these codes. The exit code follows the class: `2` usage/config, `1`
runtime, `3` the proven change is worse, `4` budget, `5` safety refusal, `130` interrupted.

<a id="usage-invalid_argument"></a>
### `usage.invalid_argument` (exit 2)

A flag or argument is malformed: an unknown flag, a value that does not parse
(`--max-bytes lots`), an unknown `--set` path, or an address `oprobe lab --bind` cannot listen
on (the port is taken: `--bind 127.0.0.1:0` takes a free one). With `-o json` argument errors
are JSON too. Fix the argument; `--help` lists what the command accepts.

<a id="usage-unknown_target"></a>
### `usage.unknown_target` (exit 2)

The `OPERATOR` is neither a URL, an existing `op.toml` file nor a name under `[operators.NAME]`
in the config file, or no operator was given. The hint lists the configured names.

<a id="usage-unknown_run"></a>
### `usage.unknown_run` (exit 2)

No stored run matches the reference. `oprobe show --list`; use more characters of the id,
`last`, `last~N`, or a path to a `.oprobe` file.

<a id="usage-unknown_baseline"></a>
### `usage.unknown_baseline` (exit 2)

A 5.x baseline name was given. oprobe 6 has no baselines: compare runs in the app, or prove a
change with `oprobe prove`.

<a id="config-invalid"></a>
### `config.invalid` (exit 2)

An operator file or the config file does not parse, or has an invalid value (an unknown key, a
size that does not parse). The message names the key.

"`op.toml` now names …, not the operator the run used": `prove` replays a stored run with the
secrets of the operator it was run with, and the operator file or configured operator it names
now has another URL or endpoint. Its keys are not sent to the run's endpoint. Put the file back
as the run had it, or start a new run with the operator you mean.

<a id="config-unknown_option"></a>
### `config.unknown_option` (exit 2)

A service option the OpenDAL service does not know (`[options]`, `--set options.KEY=…`). The
message names the closest valid option.

<a id="auth-no_credentials"></a>
### `auth.no_credentials` (exit 1)

No credentials were found. Export the provider's variables (`AWS_*`,
`GOOGLE_APPLICATION_CREDENTIALS`, `AZURE_STORAGE_*`) or set them in `[options]` (values may
reference `${ENV_VARS}`).

<a id="auth-denied"></a>
### `auth.denied` (exit 1)

The service refused the credentials or the operation (`AccessDenied`, `SignatureDoesNotMatch`,
403). oprobe needs list/read/write/delete on the prefix; profile read-only with
`--existing PREFIX`.

<a id="net-unreachable"></a>
### `net.unreachable` (exit 1, retryable)

Nothing answered (`connection refused`, reset, no route). Check the endpoint and port; is MinIO or
`oprobe lab` running? When the requests go through a proxy (`http.proxy`, or `HTTPS_PROXY` /
`HTTP_PROXY` / `ALL_PROXY` unless `NO_PROXY` exempts the endpoint), the proxy is what the client
connects to, and the message names it ("connection refused by the proxy 127.0.0.1:3128"): check
that it is running, or connect directly with `--set http.proxy=none`.

<a id="net-dns"></a>
### `net.dns` (exit 1, retryable)

The endpoint's name did not resolve. Check `options.endpoint`; on networks where only the proxy
resolves names set `HTTPS_PROXY`.

<a id="net-tls"></a>
### `net.tls` (exit 1)

The TLS handshake failed (`invalid peer certificate: UnknownIssuer`, an expired or mismatched
certificate): a private CA or a TLS-inspecting proxy. Set `http.ca_cert` (or `SSL_CERT_FILE`);
`http.insecure = true` for development endpoints only.

<a id="net-timeout"></a>
### `net.timeout` (exit 1, retryable)

A request or connection timed out. Check the network path; lower the concurrency if the client or
the link is saturated. A timeout inside a run is a finding (`hung_requests`), not an error.

<a id="net-rate_limited"></a>
### `net.rate_limited` (exit 1, retryable)

An HTTP API other than the storage operator refused the request because its rate limit is
exhausted (429, or 403 with no quota left). Wait and retry. Your storage credentials are not
involved.

<a id="net-http"></a>
### `net.http` (exit 1)

An HTTP API other than the storage operator answered with an error. The message names the URL and
the status. Your storage credentials are not involved.

<a id="storage-not_found"></a>
### `storage.not_found` (exit 1)

The bucket, container, root or `--existing` prefix does not exist, or holds no objects. Check the
URL and region; with `--existing PREFIX` list the prefix first.

<a id="storage-throttled"></a>
### `storage.throttled` (exit 1, retryable)

The service rate-limited the preflight itself (429, 503 SlowDown). Inside a run, throttling is a
finding (`throttling`) and never fails the run: `oprobe prove RUN throttling` sizes the
`ConcurrentLimitLayer` that avoids it.

<a id="storage-unsupported"></a>
### `storage.unsupported` (exit 1)

The service does not support an operation the workload needs (multipart, list). Pick another
preset.

<a id="safety-budget_exceeded"></a>
### `safety.budget_exceeded` (exit 4)

A budget (`--max-requests`, `--max-bytes`, `--max-duration`, or `[defaults.budgets]`) stopped the
command. Budgets cover the whole command: every operator of `run`, every trial of `prove`, and
every HTTP request sent to the target before cleanup (the preflight, the dataset upload and
retries included; for `fs://` and `memory://`, every call of the service). The request the budget had no room for was not sent, the operations in flight
were cut, their multipart uploads aborted, the partial run saved and the test data cleaned up.
When the budget runs out during the preflight or the dataset upload there is no run to save (and
nothing is left on the target: the preflight's probe object is deleted outside the budget).
"the next request (8.0 MiB) does not fit the byte budget: 1 B of 4.0 MiB used" means
`--max-bytes` is smaller than one request body (a multipart part, an object): raise it above
that size. "the dataset alone (…) does not fit the byte budget" means the objects to upload are
larger than `--max-bytes`: nothing was uploaded. Lower `--objects` or `--object-size`, or raise
the budget. Raise the budget or shorten the run; `--dry-run` shows the estimates.

<a id="safety-read_only"></a>
### `safety.read_only` (exit 5)

The command would write or delete under `--read-only` / `--existing`. Drop the flag, or profile
existing objects with `--existing PREFIX`.

<a id="safety-refused"></a>
### `safety.refused` (exit 5)

A safety rail refused the command, for example deleting a run that is still being written, or a
lab limit.

<a id="run-sla_failed"></a>
### `run.sla_failed` (exit 3)

A 5.x SLA check failed. oprobe 6 has no SLA: it reports failures as findings, and `--set sla…`
is refused. Exit 3 now means that `prove` found the change worse on the objective.

<a id="run-regression"></a>
### `run.regression` (exit 3)

The change made things worse on the objective: `prove`'s verdict is `worse`. Read the evidence
(the trials and the secondary metrics); try another value with `--with`.

<a id="run-interrupted"></a>
### `run.interrupted` (exit 130, retryable)

Ctrl-C, SIGTERM, SIGHUP, a closed stderr (the terminal or SSH session went away, or
`2>&1 | head` read enough) or Stop ended the command, at any point: while it opened the target,
uploaded its dataset or measured. The requests in flight were cut, the multipart uploads they
belonged to aborted, and the test data cleaned up, unless a second signal ended the cleanup too.
A run that was measuring is saved as a partial run. Run it again.

"N multipart uploads could not be aborted and may still hold parts (…)" in the message or in the
cleanup error names uploads the stop could not remove: an abort the service refused or did not
answer within 30 s, an upload whose creation did not answer within 10 s of the stop, or one
still in flight 45 s after it. Their parts are billed until the upload is aborted: list and abort
them with the service's tools (`aws s3api list-multipart-uploads` / `abort-multipart-upload`).

<a id="internal"></a>
### `internal` (exit 1)

An unexpected failure. Rerun with the same arguments; if it persists, report it with the command
line and the error message.

[Reading the profile](reading-the-profile.md) explains each verdict and section.

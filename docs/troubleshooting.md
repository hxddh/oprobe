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

[Reading the report](reading-the-report.md) explains each verdict and section.

# Troubleshooting

Start with `oprobe doctor <target>`: it checks configuration, credentials, the network path
(DNS, proxy, TCP, TLS, HTTP), clock skew and list/write/read/stat/delete permissions one by one and
prints a fix for each failure. Errors from every command come with the same hints.

| Symptom | Cause | Fix |
|---|---|---|
| `invalid peer certificate: UnknownIssuer` | private CA or TLS-inspecting proxy | `--ca-cert corp.pem` (or `SSL_CERT_FILE`), `--insecure` for dev endpoints only |
| `proxy refused CONNECT` | egress proxy policy or credentials | check `HTTPS_PROXY` (with `user:pass@`), add the host to `NO_PROXY`, or `--proxy none` |
| `dns failed` / `no addresses` | wrong endpoint, or a network where only the proxy resolves names | check the endpoint; set `HTTPS_PROXY` |
| `PermissionDenied` / `AccessDenied` / `SignatureDoesNotMatch` | credentials or IAM | export credentials or use `${ENV}` in `[target.options]`; oprobe needs list/read/write/delete on the prefix — or use `--existing` (read-only) / `--anonymous` |
| `RequestTimeTooSkewed` | local clock off by > 15 min | enable NTP; `oprobe probe` shows the skew |
| `region is missing` / `PermanentRedirect` / `AuthorizationHeaderMalformed` | wrong region | `--region` or `AWS_REGION`; R2 uses `auto` |
| many `429` / `503 SlowDown` | rate limiting | lower `--concurrency` / `--max-inflight`, or `oprobe tune` |
| `connection refused` | nothing listening | check the endpoint/port; is MinIO / `oprobe lab` running? |
| leftover `oprobe-run-*` prefixes | a run was killed (second Ctrl-C, OOM, power loss) | `oprobe cleanup <target> --dry-run`, then without `--dry-run` |
| results show "client processing-bound" on a remote store | client CPU, CRC verification or tiny objects | check `client cpu` in the report; `--no-verify`; larger objects or batching |
| `oprobe top` fails in CI | needs an interactive terminal | use `oprobe run` |
| `built without native windows` | Linux server packages are headless | use `oprobe top`, or the `oprobe-desktop` tarball / `cargo install oprobe --features gui` |

## Reading the report

* **Server latency (TTFB)-bound** — the store (or the round trip to it) dominates. More
  parallelism usually helps until throttling; check the TCP connect time to see how much of TTFB is
  network RTT.
* **Download/upload-bound** — bandwidth per stream × number of streams is the limit; increase
  `io_concurrency`/`chunk_size`, check the NIC and any proxy.
* **Connection setup** — connections are not reused (keep-alive off, bodies not drained, tiny idle
  pool) or the proxy/TLS handshake is slow.
* **Client queue** — requests wait for `max_inflight_requests` permits.
* **Client processing** — CPU, OpenDAL overhead, signing, CRC or your consumer.

The *SLOWEST OPERATION* block shows the request-by-request timeline of the worst operation and an
async call stack captured while it was in flight; the request IDs can be given to your provider's
support.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | error (invalid input, unreachable target, failed checks in `doctor`) |
| 3 | gate failed: the run breached its SLA (default: > 1% failed operations or any corrupted data; `--sla none` disables), `--fail-on` matched, or `oprobe diff` found a regression |
| 130 | interrupted with Ctrl-C (partial results) |

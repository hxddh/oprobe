---
name: oprobe
description: Measure, diagnose, tune and regression-gate object-storage performance (S3, GCS, Azure Blob, R2, MinIO, OSS, COS, B2) with the oprobe CLI or its MCP server. Use when asked why reads/writes to a bucket are slow or expensive, to benchmark a storage endpoint, to find better client settings (concurrency, chunk size, HTTP version), to compare two runs, or to set up a CI performance gate. Safe by default - plan first, run with budgets, never write without permission.
---

# oprobe

oprobe drives realistic workloads through Apache OpenDAL and traces every backend request
(queue, DNS/TCP/TLS, send, time-to-first-byte, transfer). Every run is stored and can be
explained, sliced and compared. Everything has JSON output with a published schema.

## Rules

1. **Never write to a real bucket without the user's explicit permission.** Prefer
   `--existing PREFIX` (read-only benchmark of existing objects) or `memory://` (in-process,
   no credentials). With MCP, the server is read-only unless the user started it with
   `--allow-write`; do not ask the user to change that casually.
2. **Plan before running**: `--dry-run` (CLI) or `plan_run` (MCP) shows requests, bytes, objects
   written, estimated cost and ETA. Tell the user the cost and ETA of anything non-trivial.
3. **Always set budgets** on real targets: `--max-requests`, `--max-duration`, `--max-cost-usd`
   (exit code 4 / `safety.budget_exceeded` when hit; the partial run is still saved).
4. **Parse JSON, not text**: use `-o json` (one document) or `-o ndjson` (events). Branch on
   `error.code` and exit codes, not on messages.
5. Never pass credentials in arguments; they come from the environment or configured targets.
6. Clean up after interrupted runs: `oprobe cleanup TARGET --dry-run` first.

## Workflow (CLI)

```sh
oprobe capabilities -o json                     # services, presets, error/finding codes, defaults
oprobe doctor s3://bucket/prefix -o json        # preflight; "ready": true required
oprobe run s3://bucket/prefix --existing data/ -d 15s -c 16 \
  --max-requests 20000 --max-duration 1m --dry-run -o json   # plan
oprobe run s3://bucket/prefix --existing data/ -d 15s -c 16 \
  --max-requests 20000 --max-duration 1m -o json             # measure (saves the run)
oprobe explain last -o json                     # verdict, findings + evidence, slowest-1% attribution
oprobe explain throttling                       # what a finding code means and how to verify it
oprobe compare last~1 last -o json              # regression gate (exit 3 on regression)
oprobe schema report                            # JSON Schema of any output
```

Exit codes: 0 ok · 1 runtime · 2 usage/config · 3 SLA/regression · 4 budget · 5 safety refusal ·
130 interrupted. Errors: `{"type":"error","error":{"code","message","hint","retryable"}}`.

## Workflow (MCP)

Tools: `list_targets` → `doctor` → `plan_run` → `start_run` (returns a run id; poll `get_run`)
→ `explain_run` → `analyze_run` → change one knob → `start_run` → `compare_runs`. Also
`probe_endpoint`, `estimate_cost`, `start_tune`, `cancel_run`, `list_runs`, `cleanup`.
Read `oprobe://docs/getting-started` and `oprobe://docs/findings`.

Setup: `claude mcp add oprobe -- oprobe mcp --max-requests 50000 --max-duration 2m`.

## Reading results

- `report.verdict.level`: `ok`, `review`, `attention`, `critical`; `headline` says where time went.
- `report.findings[]`: `code`, `severity`, `cause`, `evidence` (numbers), `advice`. Explain codes
  with `oprobe explain CODE`.
- `report.stages.shares`: share of operation time per layer (queue, connect, server, download, retry…).
- Outlier attribution (`explain` / `analyze_run`): attribute values over-represented among the
  slowest requests, with their lift (e.g. cold connections 6×).
- Change **one** knob at a time and confirm with `compare`.

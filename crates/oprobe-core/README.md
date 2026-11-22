# oprobe-core

The engine of [oprobe](https://github.com/hxddh/oprobe): workloads and sessions over Apache
OpenDAL, diagnosis (findings, capacity hypotheses, cost), the tuner, the run store
(`oprobe.run/5`), report rendering, the in-process S3 lab, and `oprobe_core::app` — the
service layer that the `oprobe` CLI, its MCP server and its terminal/native app all call.

The instrumentation itself lives in [`oprobe-trace`](https://crates.io/crates/oprobe-trace).

License: Apache-2.0.

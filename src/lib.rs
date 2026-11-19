//! oprobe — a white-box microscope for cloud object storage built on Apache OpenDAL.
//!
//! Every backend request issued through OpenDAL is traced at the transport level (queue wait,
//! DNS/TCP/proxy/TLS on fresh connections, request send, time-to-first-byte, transfer and
//! consumer stall) and attributed to the logical operation that caused it. On top of that:
//! bottleneck diagnosis, a measured cost model, a closed-loop tuner and a CI regression gate.

pub mod analysis;
pub mod cli;
pub mod config;
pub mod engine;
pub mod lab;
pub mod layers;
pub mod net;
pub mod report;
pub mod trace;
pub mod tune;
pub mod util;

//! The OpenDAL layer stack oprobe measures, as data.
//!
//! [`ClientStack`] is the single source of truth for the layers wrapped around the operator
//! under test (`measured_operator`) *and* for the code `oprobe tune` emits
//! (`tune::codegen`): both are derived from the same values, so the generated client is the
//! client that was measured — no layer or parameter more, none less.

use std::time::Duration;

use opendal::layers::{RetryLayer, TailCutLayer, TimeoutLayer};
use serde::{Deserialize, Serialize};

use crate::config::Knobs;
use crate::trace::EffectiveKnobs;

/// The chunk OpenDAL writes with when asked for `requested` bytes: it raises it to the
/// service's minimum multipart part size and lowers it to the maximum
/// (`opendal_core` `WriteContext::calculate_chunk_size`).
pub fn effective_write_chunk(cap: &opendal::Capability, requested: usize) -> usize {
    let mut size = requested;
    if let Some(max) = cap.write_multi_max_size {
        size = size.min(max);
    }
    if let Some(min) = cap.write_multi_min_size {
        size = size.max(min);
    }
    size
}

/// The effective knobs for `k` on a service with capability `cap`.
pub fn effective_knobs(cap: &opendal::Capability, k: &Knobs) -> EffectiveKnobs {
    EffectiveKnobs {
        requested_chunk_size: k.chunk_size.0,
        write_chunk_size: effective_write_chunk(cap, k.chunk_size.as_usize()) as u64,
        service_min_part_size: cap.write_multi_min_size.map(|v| v as u64),
        service_max_part_size: cap.write_multi_max_size.map(|v| v as u64),
    }
}

/// Longest per-I/O timeout (`TimeoutLayer::with_io_timeout`).
pub const MAX_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// `RetryLayer` parameters (exponential backoff, OpenDAL's default factor of 2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryParams {
    pub max_times: usize,
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub factor: f32,
    pub jitter: bool,
}

/// `TailCutLayer` parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TailCutParams {
    pub percentile: u8,
    pub safety_factor: f64,
    pub min_samples: usize,
    pub min_deadline: Duration,
    pub max_deadline: Duration,
    pub window: Duration,
}

/// Every OpenDAL layer (and its parameters) of the measured client, outermost last.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientStack {
    /// `TimeoutLayer::with_timeout`.
    pub timeout: Duration,
    /// `TimeoutLayer::with_io_timeout`.
    pub io_timeout: Duration,
    /// `TailCutLayer`, when enabled.
    pub tail_cut: Option<TailCutParams>,
    /// `RetryLayer`, unless retries are disabled.
    pub retry: Option<RetryParams>,
    /// Cap on in-flight HTTP requests (`ConcurrentLimitLayer::with_http_concurrent_limit`;
    /// measured by oprobe's transports, which hold a permit until the response body is done).
    pub http_concurrent_limit: Option<usize>,
}

impl ClientStack {
    pub fn from_knobs(k: &Knobs) -> Self {
        let timeout = k.timeout.get();
        Self {
            timeout,
            io_timeout: timeout.min(MAX_IO_TIMEOUT),
            tail_cut: k.tail_cut.then_some(TailCutParams {
                percentile: 95,
                safety_factor: 1.5,
                min_samples: 32,
                min_deadline: Duration::from_millis(50),
                max_deadline: timeout,
                window: Duration::from_secs(60),
            }),
            retry: (k.retries > 0).then_some(RetryParams {
                max_times: k.retries,
                min_delay: Duration::from_millis(50),
                max_delay: Duration::from_secs(5),
                factor: 2.0,
                jitter: true,
            }),
            http_concurrent_limit: (k.max_inflight_requests > 0).then_some(k.max_inflight_requests),
        }
    }

    pub fn timeout_layer(&self) -> TimeoutLayer {
        TimeoutLayer::new()
            .with_timeout(self.timeout)
            .with_io_timeout(self.io_timeout)
    }

    pub fn tail_cut_layer(&self) -> Option<TailCutLayer> {
        self.tail_cut.map(|t| {
            TailCutLayer::builder()
                .percentile(t.percentile)
                .safety_factor(t.safety_factor)
                .min_samples(t.min_samples)
                .min_deadline(t.min_deadline)
                .max_deadline(t.max_deadline)
                .window(t.window)
                .build()
        })
    }

    pub fn retry_layer(&self) -> Option<RetryLayer> {
        self.retry.map(|r| {
            let l = RetryLayer::default()
                .with_factor(r.factor)
                .with_min_delay(r.min_delay)
                .with_max_delay(r.max_delay)
                .with_max_times(r.max_times);
            if r.jitter { l.with_jitter() } else { l }
        })
    }
}

//! `ProbedSemaphore` implementing `opendal::layers::ConcurrentLimitSemaphore`
//! to record exact client concurrency queueing wait inside `ConcurrentLimitLayer`.

use std::sync::Arc;
use std::time::Instant;

use await_tree::InstrumentAwait;
use opendal::layers::ConcurrentLimitSemaphore;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::telemetry::TelemetryRegistry;

/// Permit returned by [`ProbedSemaphore`], held for the lifetime of an OpenDAL operation or stream.
pub struct ProbedSemaphorePermit {
    _permit: OwnedSemaphorePermit,
}

/// Instrumented semaphore plugged directly into `ConcurrentLimitLayer::with_semaphore`.
#[derive(Clone)]
pub struct ProbedSemaphore {
    inner: Arc<Semaphore>,
    telemetry: Arc<TelemetryRegistry>,
}

impl ProbedSemaphore {
    pub fn new(permits: usize, telemetry: Arc<TelemetryRegistry>) -> Self {
        Self {
            inner: Arc::new(Semaphore::new(permits.max(1))),
            telemetry,
        }
    }
}

impl ConcurrentLimitSemaphore for ProbedSemaphore {
    type Permit = ProbedSemaphorePermit;

    async fn acquire(&self) -> Self::Permit {
        let t_wait = Instant::now();
        let permit = self
            .inner
            .clone()
            .acquire_owned()
            .instrument_await("layer::semaphore_acquire")
            .await
            .expect("ProbedSemaphore never closed");
        let wait_us = t_wait.elapsed().as_micros() as u64;
        self.telemetry.record_semaphore_wait(wait_us);
        ProbedSemaphorePermit { _permit: permit }
    }
}

//! White-box OpenDAL middleware instrumentation layers:
//! - [`ProbedSemaphore`]: `ConcurrentLimitSemaphore` measuring exact client concurrency permit wait time;
//! - [`WhiteboxCacheLayer`]: Pure-Rust `moka` W-TinyLFU range & object cache layer measuring exact lookup time and hit/miss bytes;
//! - [`WhiteboxObserveLayer`]: Low-level `Service` observer measuring `oio::Write::close` (Multipart commit) and non-HTTP backend RPC/byte flow.

pub mod cache;
pub mod observe;
pub mod semaphore;

pub use cache::WhiteboxCacheLayer;
pub use observe::WhiteboxObserveLayer;
pub use semaphore::ProbedSemaphore;

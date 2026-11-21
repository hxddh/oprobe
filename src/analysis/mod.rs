//! Interpretation of measurements: findings (diagnosis), outlier attribution, trace-driven
//! what-if estimates, time segments, re-slicing, run comparison statistics, the cost model
//! and error hints.

pub mod capacity;
pub mod compare;
pub mod cost;
pub mod diagnosis;
pub mod findings;
pub mod hints;
pub mod outliers;
pub mod segments;
pub mod simulate;
pub mod slice;
pub mod stats;
pub mod verification;

//! Interpretation of measurements: findings and their remedies, the limit models they rest on
//! (capacity, hypotheses, limit fitting), outlier attribution, trace replays, time segments,
//! re-slicing (`show --by`) and the statistics prove decides with.

pub mod capacity;
pub mod catalog;
pub mod diagnosis;
pub mod findings;
pub mod hints;
pub mod hypothesis;
pub mod limits;
pub mod outliers;
pub mod segments;
pub mod simulate;
pub mod slice;
pub mod stats;
pub mod verdict;

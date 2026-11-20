//! Capability-aware workloads: the operation mix adapted to what the service supports
//! (`operator.info().capability()`), the per-run state of the newer operations (keys to
//! delete, etags of conditional writes) and the service limits that shape them.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use opendal::Capability;

use crate::config::{OpKind, OpSpec};

/// Keys a `delete` operation may remove, at most this many remembered.
const MAX_DELETABLE: usize = 100_000;

/// Adapt `mix` to the service: an operation it cannot run is replaced by its closest
/// supported form (or removed) with a warning saying what was measured instead.
pub fn adapt_mix(mix: &mut Vec<OpSpec>, cap: &Capability, scheme: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut warn = |w: String| {
        if !warnings.contains(&w) {
            warnings.push(w);
        }
    };
    mix.retain_mut(|m| {
        match &mut m.op {
            OpKind::ParquetScan { suffix, .. } if *suffix && !cap.read_with_suffix => {
                *suffix = false;
                warn(format!(
                    "`{scheme}` cannot read suffix ranges: parquet scans stat each file and read the footer by offset"
                ));
            }
            OpKind::ListPaged { .. } if !cap.list_with_limit => {
                m.op = OpKind::List { recursive: false };
                warn(format!(
                    "`{scheme}` lists without a page size: measured plain listings instead of paged ones"
                ));
            }
            OpKind::ListPaged { .. } | OpKind::List { .. } if !cap.list => {
                warn(format!("`{scheme}` cannot list: list operations were removed"));
                return false;
            }
            OpKind::Delete { batch } => {
                if !cap.delete {
                    warn(format!("`{scheme}` cannot delete: delete operations were removed"));
                    return false;
                }
                let per_req = cap.delete_max_size.unwrap_or(1).max(1);
                if (*batch as usize) > per_req {
                    warn(format!(
                        "`{scheme}` deletes at most {per_req} key(s) per request: a batch of {batch} takes {} requests",
                        (*batch as usize).div_ceil(per_req)
                    ));
                }
            }
            OpKind::Copy if !cap.copy => {
                m.op = OpKind::Read { size: None };
                warn(format!(
                    "`{scheme}` has no server-side copy: measured whole-object reads of the sources instead"
                ));
            }
            OpKind::PresignedRead if !cap.presign_read => {
                m.op = OpKind::Read { size: None };
                warn(format!(
                    "`{scheme}` cannot presign: measured signed reads instead of presigned ones"
                ));
            }
            OpKind::ConditionalWrite { size }
                if !cap.write_with_if_not_exists && !cap.write_with_if_match =>
            {
                m.op = OpKind::Write { size: *size };
                warn(format!(
                    "`{scheme}` has no conditional writes: measured unconditional writes instead"
                ));
            }
            OpKind::MetadataWrite { size, .. } if !cap.write_with_user_metadata => {
                m.op = OpKind::Write { size: *size };
                warn(format!(
                    "`{scheme}` cannot store user metadata: measured plain writes instead"
                ));
            }
            _ => {}
        }
        true
    });
    warnings
}

/// Requests one `delete` of `batch` keys takes on a service (OpenDAL's `Deleter` splits
/// batches at `delete_max_size`; services without batch deletes take one request per key).
pub fn delete_requests(cap: &Capability, batch: u32) -> u64 {
    (batch as u64).div_ceil(cap.delete_max_size.unwrap_or(1).max(1) as u64)
}

/// Requests one paged listing of `objects` keys takes with `limit` keys per page.
pub fn list_pages(cap: &Capability, objects: u64, limit: u32) -> u64 {
    if cap.list_with_limit {
        objects.div_ceil(limit.max(1) as u64).max(1)
    } else {
        1
    }
}

/// Mutable state of one measurement window's newer operations.
pub struct WorkloadState {
    pub capability: Capability,
    /// Writes use a fresh key each time (so `delete` operations have something to remove).
    pub unique_writes: bool,
    deletable: Mutex<VecDeque<String>>,
    seq: AtomicU64,
    /// Per write slot: the etag of the object the last conditional write left there (empty:
    /// the service returned none).
    etags: Mutex<HashMap<u64, String>>,
}

impl WorkloadState {
    pub fn new(capability: Capability, mix: &[OpSpec]) -> Self {
        Self {
            capability,
            unique_writes: mix
                .iter()
                .any(|m| m.weight > 0 && matches!(m.op, OpKind::Delete { .. })),
            deletable: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
            etags: Mutex::new(HashMap::new()),
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn push_deletable(&self, key: String) {
        let mut q = self.deletable.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < MAX_DELETABLE {
            q.push_back(key);
        }
    }

    pub fn take_deletable(&self, n: usize) -> Vec<String> {
        let mut q = self.deletable.lock().unwrap_or_else(|e| e.into_inner());
        let n = n.min(q.len());
        q.drain(..n).collect()
    }

    pub fn etag(&self, slot: u64) -> Option<String> {
        self.etags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&slot)
            .cloned()
    }

    pub fn set_etag(&self, slot: u64, etag: Option<&str>) {
        self.etags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slot, etag.unwrap_or_default().to_owned());
    }
}

impl Default for WorkloadState {
    fn default() -> Self {
        Self::new(Capability::default(), &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ByteSize;

    fn spec(op: OpKind) -> OpSpec {
        OpSpec { weight: 1, op }
    }

    #[test]
    fn unsupported_operations_are_adapted_with_a_warning() {
        let cap = Capability {
            read: true,
            write: true,
            list: true,
            delete: true,
            stat: true,
            delete_max_size: Some(1000),
            ..Default::default()
        };
        let mut mix = vec![
            spec(OpKind::Copy),
            spec(OpKind::PresignedRead),
            spec(OpKind::ListPaged { limit: 10 }),
            spec(OpKind::ConditionalWrite {
                size: ByteSize::kib(1),
            }),
            spec(OpKind::MetadataWrite {
                size: ByteSize::kib(1),
                entries: 2,
            }),
            spec(OpKind::ParquetScan {
                columns: 1,
                column_size: ByteSize::kib(1),
                footer_size: ByteSize::kib(1),
                suffix: true,
            }),
            spec(OpKind::Delete { batch: 10 }),
        ];
        let w = adapt_mix(&mut mix, &cap, "x");
        assert_eq!(w.len(), 6, "{w:#?}");
        assert_eq!(mix.len(), 7);
        assert_eq!(mix[0].op, OpKind::Read { size: None });
        assert_eq!(mix[2].op, OpKind::List { recursive: false });
        assert!(matches!(mix[3].op, OpKind::Write { .. }));
        assert!(matches!(
            mix[5].op,
            OpKind::ParquetScan { suffix: false, .. }
        ));
        assert_eq!(mix[6].op, OpKind::Delete { batch: 10 });

        // No batch deletes: a batch costs one request per key, and says so.
        let mut mix = vec![spec(OpKind::Delete { batch: 10 })];
        let one = Capability {
            delete: true,
            ..Default::default()
        };
        let w = adapt_mix(&mut mix, &one, "y");
        assert!(w[0].contains("10 requests"), "{w:?}");
        assert_eq!(delete_requests(&one, 10), 10);
        assert_eq!(delete_requests(&cap, 2500), 3);
        let no_delete = Capability::default();
        let mut mix = vec![spec(OpKind::Delete { batch: 10 }), spec(OpKind::Stat)];
        adapt_mix(&mut mix, &no_delete, "z");
        assert_eq!(mix.len(), 1);
    }

    #[test]
    fn list_pages_follow_the_limit() {
        let cap = Capability {
            list_with_limit: true,
            ..Default::default()
        };
        assert_eq!(list_pages(&cap, 128, 32), 4);
        assert_eq!(list_pages(&cap, 0, 32), 1);
        assert_eq!(list_pages(&Capability::default(), 128, 32), 1);
    }

    #[test]
    fn state_queues_keys_and_etags() {
        let s = WorkloadState::new(Capability::default(), &[spec(OpKind::Delete { batch: 2 })]);
        assert!(s.unique_writes);
        s.push_deletable("a".into());
        s.push_deletable("b".into());
        s.push_deletable("c".into());
        assert_eq!(s.take_deletable(2), vec!["a", "b"]);
        assert_eq!(s.take_deletable(5), vec!["c"]);
        assert!(s.etag(1).is_none());
        s.set_etag(1, Some("\"e\""));
        assert_eq!(s.etag(1).as_deref(), Some("\"e\""));
    }
}

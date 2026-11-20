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

/// The provider capability matrix (doctor, capabilities): what the OpenDAL service supports
/// and the limits it applies.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct CapabilityMatrix {
    pub read: bool,
    pub write: bool,
    pub stat: bool,
    pub list: bool,
    pub delete: bool,
    pub copy: bool,
    pub rename: bool,
    pub presign_read: bool,
    pub presign_write: bool,
    /// `Range: bytes=-N` (footer reads without HEAD).
    pub read_with_suffix: bool,
    pub read_with_if_match: bool,
    pub read_with_version: bool,
    pub write_can_multi: bool,
    pub write_can_append: bool,
    pub write_can_empty: bool,
    pub write_with_if_not_exists: bool,
    pub write_with_if_match: bool,
    pub write_with_user_metadata: bool,
    pub write_with_content_type: bool,
    pub list_with_limit: bool,
    pub list_with_recursive: bool,
    pub list_with_versions: bool,
    /// Keys per batch delete request (1: no batch deletes).
    pub delete_max_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_multi_min_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_multi_max_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_total_max_size_bytes: Option<u64>,
}

impl From<&Capability> for CapabilityMatrix {
    fn from(c: &Capability) -> Self {
        let b = |v: Option<usize>| v.map(|v| v as u64);
        Self {
            read: c.read,
            write: c.write,
            stat: c.stat,
            list: c.list,
            delete: c.delete,
            copy: c.copy,
            rename: c.rename,
            presign_read: c.presign_read,
            presign_write: c.presign_write,
            read_with_suffix: c.read_with_suffix,
            read_with_if_match: c.read_with_if_match,
            read_with_version: c.read_with_version,
            write_can_multi: c.write_can_multi,
            write_can_append: c.write_can_append,
            write_can_empty: c.write_can_empty,
            write_with_if_not_exists: c.write_with_if_not_exists,
            write_with_if_match: c.write_with_if_match,
            write_with_user_metadata: c.write_with_user_metadata,
            write_with_content_type: c.write_with_content_type,
            list_with_limit: c.list_with_limit,
            list_with_recursive: c.list_with_recursive,
            list_with_versions: c.list_with_versions,
            delete_max_count: c.delete_max_size.unwrap_or(1) as u64,
            write_multi_min_size_bytes: b(c.write_multi_min_size),
            write_multi_max_size_bytes: b(c.write_multi_max_size),
            write_total_max_size_bytes: b(c.write_total_max_size),
        }
    }
}

impl CapabilityMatrix {
    /// Short labels of what is and is not supported, for text output.
    pub fn summary(&self) -> (Vec<String>, Vec<String>) {
        let mut yes = Vec::new();
        let mut no = Vec::new();
        let mut f = |on: bool, label: String| {
            if on {
                yes.push(label)
            } else {
                no.push(label.split('(').next().unwrap_or_default().to_owned())
            }
        };
        let size = |v: Option<u64>| v.map(|v| crate::util::ByteSize(v).to_string());
        f(self.read, "read".into());
        f(self.write, "write".into());
        f(self.list, "list".into());
        f(
            self.delete,
            if self.delete_max_count > 1 {
                format!("delete(batch {})", self.delete_max_count)
            } else {
                "delete".into()
            },
        );
        f(self.copy, "copy".into());
        f(self.presign_read, "presign".into());
        f(self.read_with_suffix, "suffix-read".into());
        f(self.write_with_if_not_exists, "create-if-absent".into());
        f(self.write_with_if_match, "if-match-write".into());
        f(self.write_with_user_metadata, "user-metadata".into());
        f(
            self.write_can_multi,
            match (
                size(self.write_multi_min_size_bytes),
                size(self.write_multi_max_size_bytes),
            ) {
                (Some(a), Some(b)) => format!("multipart({a}–{b})"),
                _ => "multipart".into(),
            },
        );
        f(self.write_can_append, "append".into());
        f(self.list_with_limit, "paged-list".into());
        f(self.list_with_versions, "versions".into());
        (yes, no)
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

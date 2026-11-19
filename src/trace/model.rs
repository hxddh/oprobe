//! Raw trace records produced by instrumentation (one per backend request).

use serde::{Deserialize, Serialize};

/// Storage API classification of a single backend request. Derived from the real HTTP
/// request line (S3 / GCS / Azure / OSS / COS / B2 conventions) or the OpenDAL call for
/// in-process backends.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    #[default]
    GetObject,
    HeadObject,
    PutObject,
    CreateMultipart,
    UploadPart,
    CompleteMultipart,
    AbortMultipart,
    ListObjects,
    DeleteObject,
    DeleteObjects,
    CopyObject,
    /// Auth/token/control-plane calls (e.g. B2 authorize, GCS resumable session init).
    Control,
    Other,
}

/// Billing bucket used by the cost model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingClass {
    /// PUT / POST / COPY (S3 "tier 1", GCS "class A").
    Write,
    /// LIST requests (class A on S3/GCS, class C on B2).
    List,
    /// GET / HEAD (S3 "tier 2", GCS "class B").
    Read,
    /// DELETE / abort, free on most providers.
    Delete,
    /// Not billed (control plane, in-process backends).
    Free,
}

impl Api {
    pub fn label(&self) -> &'static str {
        match self {
            Api::GetObject => "GetObject",
            Api::HeadObject => "HeadObject",
            Api::PutObject => "PutObject",
            Api::CreateMultipart => "CreateMultipartUpload",
            Api::UploadPart => "UploadPart",
            Api::CompleteMultipart => "CompleteMultipartUpload",
            Api::AbortMultipart => "AbortMultipartUpload",
            Api::ListObjects => "ListObjects",
            Api::DeleteObject => "DeleteObject",
            Api::DeleteObjects => "DeleteObjects",
            Api::CopyObject => "CopyObject",
            Api::Control => "Control",
            Api::Other => "Other",
        }
    }

    pub fn billing_class(&self) -> BillingClass {
        match self {
            Api::PutObject
            | Api::CreateMultipart
            | Api::UploadPart
            | Api::CompleteMultipart
            | Api::CopyObject => BillingClass::Write,
            Api::ListObjects => BillingClass::List,
            Api::GetObject | Api::HeadObject | Api::Other => BillingClass::Read,
            Api::DeleteObject | Api::DeleteObjects | Api::AbortMultipart => BillingClass::Delete,
            Api::Control => BillingClass::Free,
        }
    }
}

/// Transport protocol a request travelled over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Http1,
    Http2,
    /// memory / fs: no network.
    InProcess,
}

/// Sequential, non-overlapping phases of one backend request in microseconds.
/// `total_us == queue + dns + tcp + proxy + tls + send + ttfb + body + stall` (±rounding).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestTiming {
    /// Waiting for an in-flight request permit or a usable pooled connection.
    pub queue_us: u64,
    /// DNS resolution (only for requests that dialed a fresh connection).
    pub dns_us: u64,
    /// TCP handshake (fresh connections only).
    pub tcp_us: u64,
    /// HTTP CONNECT through an egress proxy (fresh connections only).
    pub proxy_us: u64,
    /// TLS handshake incl. ALPN (fresh connections only).
    pub tls_us: u64,
    /// Writing the request (headers + body) until the last byte left the client.
    pub send_us: u64,
    /// Request fully sent -> response headers received (server think time + 1 RTT).
    pub ttfb_us: u64,
    /// Response body transfer time while the client was waiting on the network.
    pub body_us: u64,
    /// Time the response body sat idle because the *consumer* had not polled it yet.
    pub stall_us: u64,
    pub total_us: u64,
}

impl RequestTiming {
    pub fn connect_us(&self) -> u64 {
        self.dns_us + self.tcp_us + self.proxy_us + self.tls_us
    }
}

/// One backend request (HTTP request or in-process backend call).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub api: Api,
    pub method: String,
    /// HTTP status; 0 when the request failed before a response (network error).
    pub status: u16,
    pub protocol: Protocol,
    /// Whether the request reused a pooled (warm) connection.
    pub reused: bool,
    pub conn_id: u64,
    /// Offset from the start of the owning logical operation.
    pub start_offset_us: u64,
    pub timing: RequestTiming,
    pub bytes_up: u64,
    pub bytes_down: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RequestRecord {
    pub fn is_throttle(&self) -> bool {
        matches!(self.status, 429 | 503)
    }
    pub fn is_error(&self) -> bool {
        self.status == 0 || self.status >= 400
    }
}

/// Classify an HTTP request into a storage API from method, path and query.
pub fn classify_http(method: &http::Method, path: &str, query: Option<&str>) -> Api {
    use http::Method;
    // Runs once per backend request: no allocation unless the path has upper-case letters.
    let q = query.unwrap_or_default();
    let has = |k: &str| {
        q.split('&')
            .any(|p| p.split_once('=').map_or(p, |(name, _)| name) == k)
    };
    let path_l: std::borrow::Cow<'_, str> = if path.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(path.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(path)
    };

    // Backblaze B2 native API.
    if path_l.contains("/b2api/") {
        return if path_l.contains("b2_authorize_account")
            || path_l.contains("b2_get_upload_url")
            || path_l.contains("b2_get_upload_part_url")
        {
            Api::Control
        } else if path_l.contains("b2_list_file") {
            Api::ListObjects
        } else if path_l.contains("b2_upload_part") {
            Api::UploadPart
        } else if path_l.contains("b2_start_large_file") {
            Api::CreateMultipart
        } else if path_l.contains("b2_finish_large_file") {
            Api::CompleteMultipart
        } else if path_l.contains("b2_cancel_large_file") {
            Api::AbortMultipart
        } else if path_l.contains("b2_upload_file") {
            Api::PutObject
        } else if path_l.contains("b2_delete_file") || path_l.contains("b2_hide_file") {
            Api::DeleteObject
        } else if path_l.contains("b2_copy_file") {
            Api::CopyObject
        } else if path_l.contains("b2_get_file_info") {
            Api::HeadObject
        } else {
            Api::Other
        };
    }

    // Google Cloud Storage JSON API.
    if path_l.starts_with("/storage/v1/") || path_l.starts_with("/upload/storage/v1/") {
        if path_l.starts_with("/upload/") {
            return if has("upload_id") {
                Api::UploadPart
            } else if q.contains("uploadType=resumable") {
                Api::CreateMultipart
            } else {
                Api::PutObject
            };
        }
        if path_l.ends_with("/compose") {
            return Api::CompleteMultipart;
        }
        if path_l.contains("/copyto/") || path_l.contains("/rewriteto/") {
            return Api::CopyObject;
        }
        return match *method {
            Method::DELETE => Api::DeleteObject,
            Method::GET if path_l.ends_with("/o") => Api::ListObjects,
            Method::GET if q.contains("alt=media") => Api::GetObject,
            Method::GET => Api::HeadObject,
            Method::POST if path_l.ends_with("/batch/storage/v1") => Api::DeleteObjects,
            Method::POST | Method::PUT => Api::PutObject,
            _ => Api::Other,
        };
    }

    // Azure Blob.
    if has("comp") {
        if q.contains("comp=blocklist") {
            return Api::CompleteMultipart;
        }
        if q.contains("comp=list") {
            return Api::ListObjects;
        }
        if q.contains("comp=batch") {
            return Api::DeleteObjects;
        }
        if has("blockid") && *method == Method::PUT {
            return Api::UploadPart;
        }
    }

    // S3 / OSS / COS / MinIO / R2 (S3-compatible REST).
    match *method {
        Method::GET => {
            if has("list-type")
                || has("prefix")
                || has("delimiter")
                || has("continuation-token")
                || has("marker")
                || has("restype")
            {
                Api::ListObjects
            } else if has("uploadId") {
                Api::Other // ListParts
            } else {
                Api::GetObject
            }
        }
        Method::HEAD => Api::HeadObject,
        Method::PUT => {
            if has("partNumber") && has("uploadId") {
                Api::UploadPart
            } else {
                Api::PutObject
            }
        }
        Method::POST => {
            if has("uploads") {
                Api::CreateMultipart
            } else if has("uploadId") {
                Api::CompleteMultipart
            } else if has("delete") {
                Api::DeleteObjects
            } else {
                Api::PutObject
            }
        }
        Method::DELETE => {
            if has("uploadId") {
                Api::AbortMultipart
            } else {
                Api::DeleteObject
            }
        }
        _ => Api::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn classifies_s3_requests() {
        assert_eq!(classify_http(&Method::GET, "/b/k", None), Api::GetObject);
        assert_eq!(
            classify_http(&Method::GET, "/b", Some("list-type=2&prefix=a")),
            Api::ListObjects
        );
        assert_eq!(
            classify_http(&Method::POST, "/b/k", Some("uploads")),
            Api::CreateMultipart
        );
        assert_eq!(
            classify_http(&Method::PUT, "/b/k", Some("partNumber=1&uploadId=x")),
            Api::UploadPart
        );
        assert_eq!(
            classify_http(&Method::POST, "/b/k", Some("uploadId=x")),
            Api::CompleteMultipart
        );
        assert_eq!(
            classify_http(&Method::POST, "/b", Some("delete")),
            Api::DeleteObjects
        );
        assert_eq!(
            classify_http(&Method::DELETE, "/b/k", Some("uploadId=x")),
            Api::AbortMultipart
        );
        assert_eq!(classify_http(&Method::HEAD, "/b/k", None), Api::HeadObject);
    }

    #[test]
    fn classifies_gcs_and_azure_and_b2() {
        assert_eq!(
            classify_http(&Method::GET, "/storage/v1/b/bk/o/obj", Some("alt=media")),
            Api::GetObject
        );
        assert_eq!(
            classify_http(&Method::GET, "/storage/v1/b/bk/o", Some("prefix=x")),
            Api::ListObjects
        );
        assert_eq!(
            classify_http(&Method::PUT, "/c/blob", Some("comp=block&blockid=AAA")),
            Api::UploadPart
        );
        assert_eq!(
            classify_http(&Method::PUT, "/c/blob", Some("comp=blocklist")),
            Api::CompleteMultipart
        );
        assert_eq!(
            classify_http(&Method::POST, "/b2api/v2/b2_list_file_names", None),
            Api::ListObjects
        );
    }
}

//! Raw trace records produced by instrumentation (one per backend request).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Storage API classification of a single backend request. Derived from the real HTTP
/// request line (S3 / GCS / Azure / OSS / COS / B2 conventions) or the OpenDAL call for
/// in-process backends.
///
/// Serialized as the API's name (`GetObject`, `UploadPart`, …): the same string a request row
/// carries in `api`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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

impl Serialize for Api {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for Api {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Ok(Api::from_label(&s))
    }
}

impl JsonSchema for Api {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Api".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Storage API name, e.g. GetObject (OpenDAL ServiceOperation)"
        })
    }
}

impl Api {
    pub const ALL: [Api; 13] = [
        Api::GetObject,
        Api::HeadObject,
        Api::PutObject,
        Api::CreateMultipart,
        Api::UploadPart,
        Api::CompleteMultipart,
        Api::AbortMultipart,
        Api::ListObjects,
        Api::DeleteObject,
        Api::DeleteObjects,
        Api::CopyObject,
        Api::Control,
        Api::Other,
    ];

    /// Inverse of [`Api::label`]; unknown names are [`Api::Other`].
    pub fn from_label(s: &str) -> Api {
        Api::ALL
            .iter()
            .copied()
            .find(|a| a.label() == s)
            .unwrap_or(Api::Other)
    }

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

    /// The API class of an OpenDAL `ServiceOperation` name (the exact backend API a service
    /// attached to its request), across providers: S3 `ListObjectsV2`, Azure `ListBlobs` and
    /// B2 `ListFileNames` are all [`Api::ListObjects`]. Unknown names are [`Api::Other`].
    /// The API of one HTTP request: the name the service attached (OpenDAL `ServiceOperation`)
    /// when it is known, else the URL heuristic. Services label a metadata read with the same
    /// name as a download (GCS: `GetObject` for `GET …/o/key` without `alt=media`, and for HEAD),
    /// so a `GetObject` the request itself shows to be metadata is reported as `HeadObject`.
    /// Returns the class and the exact name to record.
    pub fn resolve(
        service_op: Option<&'static str>,
        method: &http::Method,
        path: &str,
        query: Option<&str>,
    ) -> (Api, Option<&'static str>) {
        let heuristic = || classify_http(method, path, query);
        match service_op.map(|n| (n, Api::from_service_operation(n))) {
            Some((_, Api::GetObject)) if heuristic() == Api::HeadObject => {
                (Api::HeadObject, Some("HeadObject"))
            }
            Some((name, a)) if a != Api::Other => (a, Some(name)),
            _ => (heuristic(), service_op),
        }
    }

    pub fn from_service_operation(name: &str) -> Api {
        match name {
            "GetObject" | "Get" | "GetBlob" | "DownloadFileByName" | "PresignedGetObject" => {
                Api::GetObject
            }
            "HeadObject" | "Head" | "GetBlobProperties" | "GetFileInfo" | "StatObject" => {
                Api::HeadObject
            }
            "PutObject"
            | "Put"
            | "PutBlob"
            | "InsertObject"
            | "UploadFile"
            | "AppendObject"
            | "AppendBlock"
            | "Mkcol"
            | "Proppatch"
            | "ConditionalPutObject" => Api::PutObject,
            "CreateMultipartUpload"
            | "InitiateMultipartUpload"
            | "InitiateResumableUpload"
            | "StartLargeFile" => Api::CreateMultipart,
            "UploadPart"
            | "PutBlock"
            | "UploadResumableChunk"
            | "UploadPartCopy"
            | "PutBlockFromUrl" => Api::UploadPart,
            "CompleteMultipartUpload" | "PutBlockList" | "ComposeObject" | "FinishLargeFile" => {
                Api::CompleteMultipart
            }
            "AbortMultipartUpload" | "CancelResumableUpload" | "CancelLargeFile" => {
                Api::AbortMultipart
            }
            "ListObjects" | "ListObjectsV2" | "ListObjectVersions" | "ListBlobs"
            | "ListFileNames" | "ListFileVersions" => Api::ListObjects,
            "DeleteObject" | "Delete" | "DeleteBlob" | "HideFile" | "DeleteFileVersion" => {
                Api::DeleteObject
            }
            "DeleteObjects"
            | "BatchDeleteObjects"
            | "BatchDeleteBlobs"
            | "DeleteMultipleObjects" => Api::DeleteObjects,
            "CopyObject" | "Copy" | "CopyBlob" | "CopyFile" | "RewriteObject" | "Move" => {
                Api::CopyObject
            }
            "AuthorizeAccount"
            | "GetUploadUrl"
            | "GetUploadPartUrl"
            | "GetDownloadAuthorization"
            | "Test" => Api::Control,
            _ => Api::Other,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Remote address of the connection (the proxy's when tunnelling), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub remote: Option<std::net::SocketAddr>,
    /// The exact backend API (OpenDAL `ServiceOperation`) when the service attached one; the
    /// request row's `api`. `api` above is its class (aggregation, billing).
    #[serde(skip)]
    pub service_op: Option<&'static str>,
    /// TLS handshake of the connection, on the request that dialed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Box<crate::model::TlsRow>>,
    /// Kernel TCP statistics of the connection at request end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<Box<crate::model::TcpRow>>,
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

    // Google Cloud Storage JSON API. Batch requests (OpenDAL batches deletes) go to
    // `/batch/storage/v1`, outside the `/storage/v1/` tree.
    if *method == Method::POST && path_l.trim_end_matches('/') == "/batch/storage/v1" {
        return Api::DeleteObjects;
    }
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
            classify_http(&Method::POST, "/batch/storage/v1", None),
            Api::DeleteObjects
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

    #[test]
    fn service_operations_map_to_api_classes() {
        for (name, api) in [
            ("ListObjectsV2", Api::ListObjects),
            ("ListBlobs", Api::ListObjects),
            ("PutBlockList", Api::CompleteMultipart),
            ("InsertObject", Api::PutObject),
            ("GetBlobProperties", Api::HeadObject),
            ("BatchDeleteBlobs", Api::DeleteObjects),
            ("AuthorizeAccount", Api::Control),
            ("PresignedGetObject", Api::GetObject),
            ("Whatever", Api::Other),
        ] {
            assert_eq!(Api::from_service_operation(name), api, "{name}");
        }
        // Canonical names map onto themselves.
        for a in Api::ALL {
            if !matches!(a, Api::Control | Api::Other) {
                assert_eq!(Api::from_service_operation(a.label()), a);
            }
        }
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::{Api, classify_http};
    use http::Method;

    #[test]
    fn gcs_metadata_reads_labelled_get_object_are_head_object() {
        let meta = Api::resolve(
            Some("GetObject"),
            &Method::GET,
            "/storage/v1/b/bench/o/data%2Fobj-1.bin",
            None,
        );
        assert_eq!(
            meta,
            (Api::HeadObject, Some("HeadObject")),
            "{:?}",
            classify_http(&Method::GET, "/storage/v1/b/bench/o/x", None)
        );
        let media = Api::resolve(
            Some("GetObject"),
            &Method::GET,
            "/storage/v1/b/bench/o/data%2Fobj-1.bin",
            Some("alt=media"),
        );
        assert_eq!(media, (Api::GetObject, Some("GetObject")));
        let head = Api::resolve(Some("GetObject"), &Method::HEAD, "/bench/key", None);
        assert_eq!(head.0, Api::HeadObject);
        assert_eq!(
            Api::resolve(Some("PutBlob"), &Method::PUT, "/c/k", None),
            (Api::PutObject, Some("PutBlob"))
        );
        assert_eq!(Api::resolve(None, &Method::GET, "/b/k", None).1, None);
    }
}

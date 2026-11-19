//! Built-in S3-compatible HTTP/1.1 + HTTP/2 + TLS 1.3 object storage & chaos fault-injection server
//! (`S3ChaosServer`), supporting `PutObject`, `GetObject` (`200` & `206 Range`), `HeadObject`,
//! `DeleteObject`, `DeleteObjects` batch XML, `ListObjectsV2`, full Multipart Upload
//! (`Create` / `UploadPart` / `Complete` / `Abort`), straggler tail delays, bandwidth shaping,
//! and real `429`/`503 SlowDown` rate-limiting.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody};
use http_body::Frame;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoServerBuilder;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::tls::build_tls_acceptor;
use crate::workload::{ProfileConfig, TargetConfig, WorkloadKind, WorkloadSpec};

/// Embedded deterministic ECDSA P-256 Root CA PEM (valid 2026–2046) used to verify `S3ChaosServer` TLS 1.3 leaf certs.
pub const TESTBED_TLS_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBhTCCASygAwIBAgIUa9W+2hG6ppa6pjBvtenbnhcEHMgwCgYIKoZIzj0EAwIw
ITEfMB0GA1UEAwwWb3Byb2JlLXRlc3RiZWQtcm9vdC1jYTAeFw0yNjExMTkwOTEy
MDlaFw00NjExMTQwOTEyMDlaMCExHzAdBgNVBAMMFm9wcm9iZS10ZXN0YmVkLXJv
b3QtY2EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQuAYbKF3tfOz1qFokIPVb/
fa7HhDz+LJL2XCUIcKbdFAlNbT+wjbdaQtrrnkGGJ8x+NUn9sqasepW7hAAmn2CY
o0IwQDAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQU
BJnBwfx6Mh+dzBVBk0QD7YlvPmAwCgYIKoZIzj0EAwIDRwAwRAIgKFBVQkv9CbaQ
g+h0wZ1oZTOvgY9ehjpDE8YFFHpyBFQCIEwV9BcopOiVmG3LDJ6fFf9z/EVCkpw5
rRocfRJPshFC
-----END CERTIFICATE-----";

/// Embedded leaf server certificate (`CA:FALSE`, `SAN=DNS:localhost,IP:127.0.0.1`) signed by `TESTBED_TLS_CERT_PEM`.
pub const TESTBED_SERVER_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIByjCCAXCgAwIBAgIUPN+3iZibqp0LH3ZkdgE0Apa8tRUwCgYIKoZIzj0EAwIw
ITEfMB0GA1UEAwwWb3Byb2JlLXRlc3RiZWQtcm9vdC1jYTAeFw0yNjExMTkwOTEy
MDlaFw00NjExMTQwOTEyMDlaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqG
SM49AgEGCCqGSM49AwEHA0IABNwTFYJv7wJbEK36aX0L4mgPK/vemQLiDVK0M+EZ
WAWmawR8TCFGdSpU+RZlccjAN0szEzsz0uMNVx1zy7Apmu+jgZIwgY8wDAYDVR0T
AQH/BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMB0GA1UdDgQWBBQYBX8ySBr7sjxhmvYH
2/RH69uByjAfBgNVHSMEGDAWgBQEmcHB/HoyH53MFUGTRAPtiW8+YDAKBggqhkjO
PQQDAgNIADBFAiEA2ezDulVa+RtjGSQEZCkutz/f5yExHQX61LxN82kK7KcCIADy
QurYeaTbo4+Uaparj0ZJ6zGLQL/yFzPmyfW8e1J6
-----END CERTIFICATE-----";

pub const TESTBED_TLS_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQguAIvbvO5nkk9fTiw
35UNv9IgygTjxfIde+RrTbzseIehRANCAATcExWCb+8CWxCt+ml9C+JoDyv73pkC
4g1StDPhGVgFpmsEfEwhRnUqVPkWZXHIwDdLMxM7M9LjDVcdc8uwKZrv
-----END PRIVATE KEY-----";

/// Programmable network, tail-straggler, and `429/503` rate-limit fault profile for [`S3ChaosServer`].
#[derive(Clone, Debug, Default)]
pub struct ChaosFaultConfig {
    pub enable_tls: bool,
    pub enable_http2: bool,
    pub base_ttfb_delay: Duration,
    pub multipart_merge_delay: Duration,
    pub straggler_every_n: usize,
    pub straggler_delay: Duration,
    pub max_inflight_before_503: Option<usize>,
    pub throttle_every_n_requests: Option<usize>,
    pub throttle_status_code: u16,
    pub stream_bandwidth_kib_s: Option<u64>,
}

impl ChaosFaultConfig {
    pub fn clean_https_h2() -> Self {
        Self {
            enable_tls: true,
            enable_http2: true,
            throttle_status_code: 503,
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct StoredObject {
    data: Bytes,
    etag: String,
}

struct MultipartUploadState {
    bucket: String,
    key: String,
    parts: BTreeMap<usize, Bytes>,
}

struct S3Store {
    objects: RwLock<BTreeMap<String, StoredObject>>,
    uploads: RwLock<HashMap<String, MultipartUploadState>>,
    completed_uploads: RwLock<HashMap<String, String>>,
    req_seq: AtomicU64,
    upload_seq: AtomicU64,
    active_inflight: AtomicUsize,
    peak_inflight: AtomicUsize,
    emitted_throttles: AtomicU64,
    faults: RwLock<ChaosFaultConfig>,
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full_body(bytes: impl Into<Bytes>) -> BoxBody {
    Full::new(bytes.into()).boxed()
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query_map(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(percent_decode(k), percent_decode(v));
            }
            None => {
                map.insert(percent_decode(pair), String::new());
            }
        }
    }
    map
}

fn extract_xml_tag_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        if let Some(end) = after.find(&close) {
            out.push(percent_decode(after[..end].trim()));
            rest = &after[end + close.len()..];
        } else {
            break;
        }
    }
    out
}

fn parse_http_range_header(hdr: &str, total_len: usize) -> Option<(usize, usize)> {
    let spec = hdr.trim().strip_prefix("bytes=")?;
    let first_range = spec.split(',').next()?.trim();
    let (start_s, end_s) = first_range.split_once('-')?;
    if start_s.is_empty() {
        let suffix: usize = end_s.parse().ok()?;
        if suffix == 0 || total_len == 0 {
            return None;
        }
        let start = total_len.saturating_sub(suffix);
        Some((start, total_len - 1))
    } else {
        let start: usize = start_s.parse().ok()?;
        if start >= total_len {
            return None;
        }
        let end = if end_s.is_empty() {
            total_len - 1
        } else {
            end_s.parse::<usize>().ok()?.min(total_len - 1)
        };
        if start > end {
            None
        } else {
            Some((start, end))
        }
    }
}

struct ServerInflightGuard {
    store: Arc<S3Store>,
}

impl Drop for ServerInflightGuard {
    fn drop(&mut self) {
        self.store.active_inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn build_shaped_body(payload: Bytes, bandwidth_kib_s: Option<u64>) -> BoxBody {
    let Some(rate_kib) = bandwidth_kib_s.filter(|&r| r > 0) else {
        return full_body(payload);
    };
    if payload.len() <= 16_384 {
        return full_body(payload);
    }
    let chunk_size = 32 * 1024_usize;
    let sleep_per_chunk_us =
        ((chunk_size as f64 / (rate_kib as f64 * 1024.0)) * 1_000_000.0) as u64;

    let stream = futures::stream::unfold((payload, 0_usize), move |(buf, pos)| async move {
        if pos >= buf.len() {
            return None;
        }
        let end = (pos + chunk_size).min(buf.len());
        let slice = buf.slice(pos..end);
        if pos > 0 && sleep_per_chunk_us > 0 {
            tokio::time::sleep(Duration::from_micros(sleep_per_chunk_us)).await;
        }
        Some((Ok::<_, Infallible>(Frame::data(slice)), (buf, end)))
    });
    StreamBody::new(stream).boxed()
}

async fn handle_s3_request(
    req: Request<Incoming>,
    store: Arc<S3Store>,
) -> std::result::Result<Response<BoxBody>, Infallible> {
    let cur_inflight = store.active_inflight.fetch_add(1, Ordering::AcqRel) + 1;
    store
        .peak_inflight
        .fetch_max(cur_inflight, Ordering::Relaxed);
    let _inflight_guard = ServerInflightGuard {
        store: store.clone(),
    };

    let seq = store.req_seq.fetch_add(1, Ordering::Relaxed) + 1;
    let req_id = format!("oprobe-s3-req-{seq:06}");
    let faults = store.faults.read().expect("faults lock").clone();

    let method = req.method().clone();
    let uri = req.uri().clone();
    let qmap = parse_query_map(uri.query());
    let is_delete_cleanup = method == Method::DELETE || qmap.contains_key("delete");

    // 1. Optional 429 / 503 SlowDown Rate-Limit Injection (never block cleanup DELETEs so teardown verify stays clean)
    if !is_delete_cleanup {
        let over_conc = faults
            .max_inflight_before_503
            .map(|limit| cur_inflight > limit)
            .unwrap_or(false);
        let periodic_throttle = faults
            .throttle_every_n_requests
            .filter(|&n| n > 0)
            .map(|n| seq.is_multiple_of(n as u64))
            .unwrap_or(false);

        if over_conc || periodic_throttle {
            store.emitted_throttles.fetch_add(1, Ordering::Relaxed);
            let status_u16 = if faults.throttle_status_code == 429 {
                429
            } else {
                503
            };
            let code_str = if status_u16 == 429 {
                "TooManyRequests"
            } else {
                "SlowDown"
            };
            let err_xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <Error><Code>{code_str}</Code>\
                 <Message>Please reduce your request rate.</Message>\
                 <RequestId>{req_id}</RequestId></Error>"
            );
            return Ok(Response::builder()
                .status(StatusCode::from_u16(status_u16).unwrap_or(StatusCode::SERVICE_UNAVAILABLE))
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(err_xml))
                .unwrap());
        }
    }

    // 2. Base Gateway TTFB & Straggler Tail Latency Injection
    if !faults.base_ttfb_delay.is_zero() {
        tokio::time::sleep(faults.base_ttfb_delay).await;
    }
    if !is_delete_cleanup
        && faults.straggler_every_n > 0
        && seq.is_multiple_of(faults.straggler_every_n as u64)
        && !faults.straggler_delay.is_zero()
    {
        tokio::time::sleep(faults.straggler_delay).await;
    }

    let trimmed_path = uri.path().trim_start_matches('/');
    if trimmed_path.is_empty() {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("x-amz-request-id", &req_id)
            .body(full_body(Bytes::new()))
            .unwrap());
    }

    let (bucket, raw_key) = match trimmed_path.split_once('/') {
        Some((b, k)) => (percent_decode(b), percent_decode(k)),
        None => (percent_decode(trimmed_path), String::new()),
    };

    // Bucket-level operations (`ListObjectsV2` and batch `POST /{bucket}?delete`)
    if raw_key.is_empty() {
        if method == Method::POST && qmap.contains_key("delete") {
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let xml_str = String::from_utf8_lossy(&body_bytes);
            let keys = extract_xml_tag_values(&xml_str, "Key");
            if let Ok(mut objs) = store.objects.write() {
                for k in keys {
                    let full_k = format!("{bucket}/{}", k.trim_start_matches('/'));
                    objs.remove(&full_k);
                }
            }
            let resp_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                <DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></DeleteResult>";
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(resp_xml))
                .unwrap());
        }

        if method == Method::HEAD {
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::GET {
            let prefix = qmap.get("prefix").cloned().unwrap_or_default();
            let bucket_prefix = format!("{bucket}/");
            let full_search_prefix = format!("{bucket_prefix}{prefix}");
            let mut items_xml = String::new();
            let mut count = 0_usize;

            if let Ok(objs) = store.objects.read() {
                for (full_k, obj) in objs.range(full_search_prefix.clone()..) {
                    if !full_k.starts_with(&full_search_prefix) {
                        break;
                    }
                    let rel_key = &full_k[bucket_prefix.len()..];
                    count += 1;
                    items_xml.push_str(&format!(
                        "<Contents><Key>{}</Key><LastModified>2026-11-19T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                        xml_escape(rel_key),
                        xml_escape(&obj.etag),
                        obj.data.len()
                    ));
                    if count >= 1000 {
                        break;
                    }
                }
            }

            let list_xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{count}</KeyCount>\
                 <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>{items_xml}</ListBucketResult>",
                xml_escape(&bucket),
                xml_escape(&prefix)
            );
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(list_xml))
                .unwrap());
        }
    }

    let store_key = format!("{bucket}/{raw_key}");

    // Multipart Upload APIs
    if method == Method::POST && qmap.contains_key("uploads") {
        let upload_id = format!(
            "mpu-{}",
            store.upload_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        if let Ok(mut ups) = store.uploads.write() {
            ups.insert(
                upload_id.clone(),
                MultipartUploadState {
                    bucket: bucket.clone(),
                    key: raw_key.clone(),
                    parts: BTreeMap::new(),
                },
            );
        }
        let init_xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket><Key>{}</Key><UploadId>{upload_id}</UploadId>\
             </InitiateMultipartUploadResult>",
            xml_escape(&bucket),
            xml_escape(&raw_key)
        );
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/xml")
            .header("x-amz-request-id", &req_id)
            .body(full_body(init_xml))
            .unwrap());
    }

    if let Some(upload_id) = qmap.get("uploadId") {
        if method == Method::PUT {
            let part_num: usize = qmap
                .get("partNumber")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let part_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let crc = crc32c::crc32c(&part_bytes);
            let etag = format!("\"{crc:08x}\"");

            if let Ok(mut ups) = store.uploads.write() {
                if let Some(st) = ups.get_mut(upload_id) {
                    st.parts.insert(part_num, part_bytes);
                }
            }
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("etag", etag)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::POST {
            let _ = req.into_body().collect().await;
            if !faults.multipart_merge_delay.is_zero() {
                tokio::time::sleep(faults.multipart_merge_delay).await;
            }
            let merged_opt = store
                .uploads
                .write()
                .ok()
                .and_then(|mut ups| ups.remove(upload_id));

            if let Some(st) = merged_opt {
                let total_len: usize = st.parts.values().map(|b| b.len()).sum();
                let mut merged = BytesMut::with_capacity(total_len);
                let num_parts = st.parts.len();
                for (_, p_bytes) in st.parts {
                    merged.extend_from_slice(&p_bytes);
                }
                let frozen = merged.freeze();
                let crc = crc32c::crc32c(&frozen);
                let etag = format!("\"{crc:08x}-{num_parts}\"");

                if let Ok(mut objs) = store.objects.write() {
                    objs.insert(
                        format!("{}/{}", st.bucket, st.key),
                        StoredObject {
                            data: frozen,
                            etag: etag.clone(),
                        },
                    );
                }
                let comp_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                     <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                     <Location>http://localhost/{}/{}</Location>\
                     <Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag>\
                     </CompleteMultipartUploadResult>",
                    xml_escape(&st.bucket),
                    xml_escape(&st.key),
                    xml_escape(&st.bucket),
                    xml_escape(&st.key),
                    xml_escape(&etag)
                );
                if let Ok(mut done) = store.completed_uploads.write() {
                    done.insert(upload_id.clone(), comp_xml.clone());
                }
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/xml")
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(comp_xml))
                    .unwrap());
            }
            if let Some(prev_xml) = store
                .completed_uploads
                .read()
                .ok()
                .and_then(|done| done.get(upload_id).cloned())
            {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/xml")
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(prev_xml))
                    .unwrap());
            }
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::DELETE {
            if let Ok(mut ups) = store.uploads.write() {
                ups.remove(upload_id);
            }
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }
    }

    // Standard Single-Object CRUD
    match method {
        Method::PUT => {
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let crc = crc32c::crc32c(&body_bytes);
            let etag = format!("\"{crc:08x}\"");
            if let Ok(mut objs) = store.objects.write() {
                objs.insert(
                    store_key,
                    StoredObject {
                        data: body_bytes,
                        etag: etag.clone(),
                    },
                );
            }
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("etag", etag)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap())
        }
        Method::HEAD => {
            let found = store
                .objects
                .read()
                .ok()
                .and_then(|objs| objs.get(&store_key).cloned());
            match found {
                Some(obj) => Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-length", obj.data.len())
                    .header("etag", obj.etag)
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(Bytes::new()))
                    .unwrap()),
                None => Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(Bytes::new()))
                    .unwrap()),
            }
        }
        Method::GET => {
            let range_hdr = req
                .headers()
                .get(http::header::RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let found = store
                .objects
                .read()
                .ok()
                .and_then(|objs| objs.get(&store_key).cloned());

            match found {
                Some(obj) => {
                    let total_len = obj.data.len();
                    if let Some(ref r_str) = range_hdr {
                        if let Some((start, end)) = parse_http_range_header(r_str, total_len) {
                            let slice = obj.data.slice(start..=end);
                            let slice_len = slice.len();
                            let body =
                                build_shaped_body(slice, faults.stream_bandwidth_kib_s).await;
                            return Ok(Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header("content-length", slice_len)
                                .header(
                                    "content-range",
                                    format!("bytes {start}-{end}/{total_len}"),
                                )
                                .header("accept-ranges", "bytes")
                                .header("etag", obj.etag)
                                .header("x-amz-request-id", &req_id)
                                .body(body)
                                .unwrap());
                        }
                    }
                    let body =
                        build_shaped_body(obj.data, faults.stream_bandwidth_kib_s).await;
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-length", total_len)
                        .header("accept-ranges", "bytes")
                        .header("etag", obj.etag)
                        .header("x-amz-request-id", &req_id)
                        .body(body)
                        .unwrap())
                }
                None => {
                    let not_found_xml = format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                         <Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message>\
                         <RequestId>{req_id}</RequestId></Error>"
                    );
                    Ok(Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header("content-type", "application/xml")
                        .header("x-amz-request-id", &req_id)
                        .body(full_body(not_found_xml))
                        .unwrap())
                }
            }
        }
        Method::DELETE => {
            if let Ok(mut objs) = store.objects.write() {
                objs.remove(&store_key);
            }
            Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap())
        }
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("x-amz-request-id", &req_id)
            .body(full_body(Bytes::new()))
            .unwrap()),
    }
}

/// Live handle to a running [`S3ChaosServer`] instance.
pub struct S3ChaosServerHandle {
    pub addr: SocketAddr,
    pub endpoint_url: String,
    store: Arc<S3Store>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl S3ChaosServerHandle {
    /// Update fault-injection parameters dynamically at runtime.
    pub fn update_faults(&self, next: ChaosFaultConfig) {
        if let Ok(mut f) = self.store.faults.write() {
            *f = next;
        }
    }

    /// Number of stored objects currently alive in the S3 testbed (used to prove zero leaked data after cleanup).
    pub fn stored_object_count(&self) -> usize {
        self.store
            .objects
            .read()
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Total `429`/`503` throttle responses emitted by this server.
    pub fn emitted_throttles(&self) -> u64 {
        self.store.emitted_throttles.load(Ordering::Relaxed)
    }

    /// Peak concurrent server-side requests observed.
    pub fn peak_server_inflight(&self) -> usize {
        self.store.peak_inflight.load(Ordering::Relaxed)
    }

    /// Build a ready-to-run [`ProfileConfig`] pointing at this S3 server (including embedded TLS CA verification).
    pub fn make_s3_profile(&self, bucket: &str, kind: WorkloadKind) -> ProfileConfig {
        let mut opts = BTreeMap::new();
        opts.insert("bucket".to_owned(), bucket.to_owned());
        opts.insert("region".to_owned(), "us-east-1".to_owned());
        opts.insert("endpoint".to_owned(), self.endpoint_url.clone());
        opts.insert("access_key_id".to_owned(), "oprobe-test-ak".to_owned());
        opts.insert("secret_access_key".to_owned(), "oprobe-test-sk".to_owned());
        opts.insert("enable_virtual_host_style".to_owned(), "false".to_owned());
        opts.insert("root".to_owned(), "/bench".to_owned());
        if self.endpoint_url.starts_with("https://") {
            opts.insert("ca_cert_pem".to_owned(), TESTBED_TLS_CERT_PEM.to_owned());
        }

        ProfileConfig {
            target: TargetConfig {
                name: format!("S3ChaosTestbed ({})", self.endpoint_url),
                scheme: "s3".to_owned(),
                options: opts,
            },
            workload: WorkloadSpec {
                kind,
                rounds: 16,
                duration_secs: None,
                warmup_secs: 0.0,
                object_size_mib: 2,
                verify_crc32c: true,
                enable_foyer_cache: true,
                foyer_memory_mib: 64,
                internet_egress_ratio: 0.15,
                auto_cleanup: true,
            },
            ..ProfileConfig::memory_demo(kind)
        }
    }
}

impl Drop for S3ChaosServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn an ephemeral or bound S3-compatible HTTP/HTTPS chaos server.
pub struct S3ChaosServer;

impl S3ChaosServer {
    pub async fn spawn(bind_addr: &str, mut faults: ChaosFaultConfig) -> Result<S3ChaosServerHandle> {
        if faults.throttle_status_code == 0 {
            faults.throttle_status_code = 503;
        }
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;

        let tls_acceptor = if faults.enable_tls {
            Some(build_tls_acceptor(
                TESTBED_SERVER_CERT_PEM,
                TESTBED_TLS_KEY_PEM,
                faults.enable_http2,
            )?)
        } else {
            None
        };

        let scheme = if faults.enable_tls { "https" } else { "http" };
        let host_for_url = if local_addr.ip().is_loopback() {
            "localhost".to_owned()
        } else {
            local_addr.ip().to_string()
        };
        let endpoint_url = format!("{scheme}://{host_for_url}:{}", local_addr.port());

        let store = Arc::new(S3Store {
            objects: RwLock::new(BTreeMap::new()),
            uploads: RwLock::new(HashMap::new()),
            completed_uploads: RwLock::new(HashMap::new()),
            req_seq: AtomicU64::new(0),
            upload_seq: AtomicU64::new(0),
            active_inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            emitted_throttles: AtomicU64::new(0),
            faults: RwLock::new(faults.clone()),
        });

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let store_bg = store.clone();
        let allow_h2 = faults.enable_http2;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    accepted = listener.accept() => {
                        let Ok((tcp, _peer)) = accepted else {
                            continue;
                        };
                        let _ = tcp.set_nodelay(true);
                        let st = store_bg.clone();
                        let tls_acc = tls_acceptor.clone();

                        tokio::spawn(async move {
                            let svc = service_fn(move |req| handle_s3_request(req, st.clone()));
                            let mut builder = AutoServerBuilder::new(TokioExecutor::new());
                            builder.http2().adaptive_window(true);
                            if !allow_h2 {
                                let io_plain_or_tls = tls_acc;
                                if let Some(acc) = io_plain_or_tls {
                                    if let Ok(tls_s) = acc.accept(tcp).await {
                                        let _ = hyper::server::conn::http1::Builder::new()
                                            .keep_alive(true)
                                            .serve_connection(TokioIo::new(tls_s), svc)
                                            .await;
                                    }
                                } else {
                                    let _ = hyper::server::conn::http1::Builder::new()
                                        .keep_alive(true)
                                        .serve_connection(TokioIo::new(tcp), svc)
                                        .await;
                                }
                                return;
                            }

                            if let Some(acc) = tls_acc {
                                if let Ok(tls_s) = acc.accept(tcp).await {
                                    let _ = builder.serve_connection(TokioIo::new(tls_s), svc).await;
                                }
                            } else {
                                let _ = builder.serve_connection(TokioIo::new(tcp), svc).await;
                            }
                        });
                    }
                }
            }
        });

        Ok(S3ChaosServerHandle {
            addr: local_addr,
            endpoint_url,
            store,
            shutdown_tx: Some(shutdown_tx),
        })
    }
}

//! `--client aws-sdk`: aws-sdk-s3 (Rust) driving the workload (design v5 §7, cargo feature
//! `aws-sdk`).
//!
//! The SDK's HTTP client is oprobe's instrumented transport (the same fidelity or microscope
//! transport the OpenDAL client uses), so every request the SDK sends, retries included, is
//! recorded with its phases. Only the SDK's own retry backoff is invisible (it shows as client
//! time).

use std::collections::HashMap;
use std::sync::Arc;

use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
    retry::RetryConfig, timeout::TimeoutConfig,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use futures::future::BoxFuture;
use opendal::raw::oio::ReadStream as _;
use opendal::{Buffer, HttpTransporter};

use super::service::{Backend, ClientOp, ClientReply, GetRange, ListEntry, error_for};
use crate::config::{Knobs, TargetSpec};
use crate::errors::{ErrorCode, OprobeError};

/// The aws-sdk-s3 version this build links.
pub const VERSION: &str = aws_sdk_s3::meta::PKG_VERSION;

/// The SDK's HTTP client: oprobe's traced transport.
#[derive(Clone, Debug)]
struct TracedHttp(HttpTransporter);

impl HttpConnector for TracedHttp {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let t = self.0.clone();
        HttpConnectorFuture::new(async move {
            let req = request
                .try_into_http1x()
                .map_err(|e| ConnectorError::other(Box::new(e), None))?;
            let (parts, body) = req.into_parts();
            let body = match body.bytes() {
                Some(b) => Buffer::from(Bytes::copy_from_slice(b)),
                None => {
                    return Err(ConnectorError::other(
                        "streaming request bodies are not supported".into(),
                        None,
                    ));
                }
            };
            let resp = t
                .fetch(http::Request::from_parts(parts, body))
                .await
                .map_err(|e| {
                    if e.kind() == opendal::ErrorKind::Unexpected && e.is_temporary() {
                        ConnectorError::io(Box::new(e))
                    } else {
                        ConnectorError::other(Box::new(e), None)
                    }
                })?;
            let (parts, mut body) = resp.into_parts();
            // Read the body here, inside the operation's scope: its transfer is the request's
            // `body` stage.
            let data = body
                .read_all()
                .await
                .map_err(|e| ConnectorError::io(Box::new(e)))?
                .to_bytes();
            HttpResponse::try_from(http::Response::from_parts(parts, SdkBody::from(data)))
                .map_err(|e| ConnectorError::other(Box::new(e), None))
        })
    }
}

impl HttpClient for TracedHttp {
    fn http_connector(
        &self,
        _: &HttpConnectorSettings,
        _: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(self.clone())
    }
}

/// aws-sdk-s3 as a [`Backend`].
#[derive(Clone, Debug)]
pub struct AwsBackend {
    client: aws_sdk_s3::Client,
    bucket: String,
}

/// Credentials from the target, else the standard environment variables.
fn credentials(target: &TargetSpec) -> Option<Credentials> {
    let opts = target.resolved_options().unwrap_or_default();
    let get = |opt: &str, var: &str| {
        opts.get(opt)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| std::env::var(var).ok().filter(|v| !v.is_empty()))
    };
    let ak = get("access_key_id", "AWS_ACCESS_KEY_ID")?;
    let sk = get("secret_access_key", "AWS_SECRET_ACCESS_KEY")?;
    Some(Credentials::new(
        ak,
        sk,
        get("session_token", "AWS_SESSION_TOKEN"),
        None,
        "oprobe",
    ))
}

impl AwsBackend {
    /// A client for `target` whose requests go through `http` (recorded).
    pub fn new(target: &TargetSpec, knobs: &Knobs, http: HttpTransporter) -> anyhow::Result<Self> {
        if target.scheme != "s3" {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "the aws-sdk client drives S3 targets, not `{}`",
                    target.scheme
                ),
            )
            .with_hint("use the default OpenDAL client for this service")
            .into());
        }
        let opts = target.resolved_options()?;
        let bucket = opts.get("bucket").cloned().ok_or_else(|| {
            OprobeError::new(ErrorCode::ConfigInvalid, "the target has no bucket")
        })?;
        let creds = credentials(target).ok_or_else(|| {
            OprobeError::new(
                ErrorCode::AuthNoCredentials,
                "the aws-sdk client found no credentials",
            )
            .with_hint(
                "set access_key_id and secret_access_key on the target, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
            )
        })?;
        let region = opts
            .get("region")
            .cloned()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .unwrap_or_else(|| "us-east-1".into());
        let endpoint = opts.get("endpoint").cloned();
        let virtual_host = opts
            .get("enable_virtual_host_style")
            .is_some_and(|v| v == "true");
        let timeout = knobs.timeout.get();
        let mut b = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region))
            .credentials_provider(creds)
            .http_client(TracedHttp(http))
            .retry_config(RetryConfig::standard().with_max_attempts(knobs.retries as u32 + 1))
            .timeout_config(
                TimeoutConfig::builder()
                    .operation_attempt_timeout(timeout)
                    .build(),
            )
            // No checksum headers or trailers the workload did not ask for (the OpenDAL client
            // sends none either, unless `checksum` is set).
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .force_path_style(endpoint.is_some() && !virtual_host);
        if let Some(ep) = endpoint {
            b = b.endpoint_url(ep);
        }
        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(b.build()),
            bucket,
        })
    }
}

/// The OpenDAL error of an SDK error.
fn sdk_error<E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static>(
    e: SdkError<E, HttpResponse>,
) -> opendal::Error {
    let status = e.raw_response().map(|r| r.status().as_u16()).unwrap_or(0);
    let code = e.code().unwrap_or_default().to_owned();
    let msg = match e.message() {
        Some(m) => m.to_owned(),
        None => {
            // Network errors: the whole chain.
            let mut s = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(c) = src {
                s.push_str(": ");
                s.push_str(&c.to_string());
                src = c.source();
            }
            s
        }
    };
    error_for(status, &code, &msg)
}

/// `bytes=a-b/total` → `total`.
fn total_of(content_range: Option<&str>) -> Option<u64> {
    content_range?.rsplit('/').next()?.parse().ok()
}

/// Percent-encode a key for `x-amz-copy-source` (slashes kept).
fn encode_key(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    for b in k.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Backend for AwsBackend {
    fn call(&self, op: ClientOp) -> BoxFuture<'static, opendal::Result<ClientReply>> {
        let c = self.client.clone();
        let bucket = self.bucket.clone();
        Box::pin(async move {
            Ok(match op {
                ClientOp::Get { key, range } => {
                    let range = match range {
                        GetRange::Whole => None,
                        GetRange::From {
                            offset,
                            length: Some(l),
                        } => Some(format!("bytes={offset}-{}", offset + l.max(1) - 1)),
                        GetRange::From {
                            offset,
                            length: None,
                        } => Some(format!("bytes={offset}-")),
                        GetRange::Suffix(n) => Some(format!("bytes=-{n}")),
                    };
                    let out = c
                        .get_object()
                        .bucket(&bucket)
                        .key(&key)
                        .set_range(range)
                        .send()
                        .await
                        .map_err(sdk_error)?;
                    let size =
                        total_of(out.content_range()).or(out.content_length().map(|l| l as u64));
                    let etag = out.e_tag().map(str::to_owned);
                    let data = out
                        .body
                        .collect()
                        .await
                        .map_err(|e| error_for(0, "", &e.to_string()))?
                        .into_bytes();
                    ClientReply {
                        bytes: data.len() as u64,
                        data: Some(Buffer::from(data)),
                        size,
                        etag,
                        ..Default::default()
                    }
                }
                ClientOp::Put {
                    key,
                    data,
                    if_match,
                    if_none_match,
                    content_type,
                    metadata,
                } => {
                    let n = data.len() as u64;
                    let out = c
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .set_if_match(if_match)
                        .set_if_none_match(if_none_match)
                        .set_content_type(content_type)
                        .set_metadata(
                            (!metadata.is_empty())
                                .then(|| metadata.into_iter().collect::<HashMap<_, _>>()),
                        )
                        .send()
                        .await
                        .map_err(sdk_error)?;
                    ClientReply {
                        bytes: n,
                        etag: out.e_tag().map(str::to_owned),
                        ..Default::default()
                    }
                }
                ClientOp::Head { key } => {
                    let out = c
                        .head_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                        .map_err(sdk_error)?;
                    ClientReply {
                        size: out.content_length().map(|l| l as u64),
                        etag: out.e_tag().map(str::to_owned),
                        ..Default::default()
                    }
                }
                ClientOp::Delete { keys } if keys.len() == 1 => {
                    c.delete_object()
                        .bucket(&bucket)
                        .key(&keys[0])
                        .send()
                        .await
                        .map_err(sdk_error)?;
                    ClientReply::default()
                }
                ClientOp::Delete { keys } => {
                    let objects = keys
                        .iter()
                        .map(|k| ObjectIdentifier::builder().key(k).build())
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| error_for(0, "", &e.to_string()))?;
                    let del = Delete::builder()
                        .set_objects(Some(objects))
                        .quiet(true)
                        .build()
                        .map_err(|e| error_for(0, "", &e.to_string()))?;
                    let out = c
                        .delete_objects()
                        .bucket(&bucket)
                        .delete(del)
                        .send()
                        .await
                        .map_err(sdk_error)?;
                    ClientReply {
                        failed_keys: out
                            .errors()
                            .iter()
                            .map(|e| {
                                (
                                    e.key().unwrap_or_default().to_owned(),
                                    e.message().unwrap_or("not deleted").to_owned(),
                                )
                            })
                            .collect(),
                        ..Default::default()
                    }
                }
                ClientOp::List {
                    prefix,
                    recursive,
                    limit,
                } => {
                    let mut entries = Vec::new();
                    let mut token: Option<String> = None;
                    loop {
                        let out = c
                            .list_objects_v2()
                            .bucket(&bucket)
                            .prefix(&prefix)
                            .set_delimiter((!recursive).then(|| "/".to_owned()))
                            .set_max_keys(limit.map(|l| l.min(1000) as i32))
                            .set_continuation_token(token.take())
                            .send()
                            .await
                            .map_err(sdk_error)?;
                        entries.extend(out.contents().iter().map(|o| ListEntry {
                            key: o.key().unwrap_or_default().to_owned(),
                            size: o.size().unwrap_or(0).max(0) as u64,
                            dir: false,
                        }));
                        entries.extend(out.common_prefixes().iter().map(|p| ListEntry {
                            key: p.prefix().unwrap_or_default().to_owned(),
                            size: 0,
                            dir: true,
                        }));
                        match out.next_continuation_token() {
                            Some(t) if out.is_truncated() == Some(true) => {
                                token = Some(t.to_owned())
                            }
                            _ => break,
                        }
                    }
                    ClientReply {
                        entries,
                        ..Default::default()
                    }
                }
                ClientOp::Copy { source, key } => {
                    let res = c
                        .copy_object()
                        .bucket(&bucket)
                        .key(&key)
                        .copy_source(format!("{bucket}/{}", encode_key(&source)))
                        .send()
                        .await;
                    let etag = match res {
                        Ok(out) => out
                            .copy_object_result()
                            .and_then(|r| r.e_tag())
                            .map(str::to_owned),
                        // The copy succeeded (200) but its result document did not parse (some
                        // S3-compatible servers, the lab among them, write `LastModified` in a
                        // form the SDK rejects): the object is copied all the same. An error
                        // S3 embeds in a 200 is a service error with a code, not this.
                        Err(e)
                            if e.code().is_none()
                                && e.raw_response().is_some_and(|r| r.status().as_u16() == 200) =>
                        {
                            None
                        }
                        Err(e) => return Err(sdk_error(e)),
                    };
                    ClientReply {
                        etag,
                        ..Default::default()
                    }
                }
            })
        })
    }
}

/// What the SDK client can do, as an OpenDAL service.
pub fn capability() -> opendal::Capability {
    opendal::Capability {
        stat: true,
        read: true,
        read_with_suffix: true,
        write: true,
        write_can_empty: true,
        write_with_content_type: true,
        write_with_if_match: true,
        write_with_if_none_match: true,
        write_with_if_not_exists: true,
        write_with_user_metadata: true,
        delete: true,
        delete_max_size: Some(1000),
        list: true,
        list_with_recursive: true,
        list_with_limit: true,
        copy: true,
        shared: true,
        ..Default::default()
    }
}

/// Keep `Arc<AwsBackend>` usable where a backend is shared.
impl Backend for Arc<AwsBackend> {
    fn call(&self, op: ClientOp) -> BoxFuture<'static, opendal::Result<ClientReply>> {
        self.as_ref().call(op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(total_of(Some("bytes 0-9/100")), Some(100));
        assert_eq!(encode_key("a b/c+d"), "a%20b/c%2Bd");
    }
}

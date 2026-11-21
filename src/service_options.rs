//! Validation of `[target.options]` keys against the OpenDAL service's configuration.
//!
//! OpenDAL deserializes service options leniently: an unknown key (a typo such as `regin`, or an
//! option of another service) is silently ignored and the run measures a different setup than
//! intended. oprobe rejects such keys up front, with a "did you mean" suggestion.
//!
//! The accepted keys are read from the service's `*Config` type itself (its serde field names
//! and aliases), so they always match the OpenDAL version oprobe is built with.

use std::collections::BTreeSet;
use std::fmt;

use anyhow::Result;
use serde::Deserialize;
use serde::de::{self, Visitor};

/// A deserializer that answers every request with an error carrying the struct's field names
/// (`Deserializer::deserialize_struct` receives them from serde's derive, aliases included).
struct FieldNames;

#[derive(Debug)]
struct Captured(Vec<&'static str>);

impl fmt::Display for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "captured {} fields", self.0.len())
    }
}

impl std::error::Error for Captured {}

impl de::Error for Captured {
    fn custom<T: fmt::Display>(_msg: T) -> Self {
        Captured(Vec::new())
    }
}

impl<'de> de::Deserializer<'de> for FieldNames {
    type Error = Captured;

    fn deserialize_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, Captured> {
        Err(Captured(Vec::new()))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _v: V,
    ) -> Result<V::Value, Captured> {
        Err(Captured(fields.to_vec()))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier
        ignored_any
    }
}

fn fields_of<'de, T: Deserialize<'de>>() -> Vec<&'static str> {
    match T::deserialize(FieldNames) {
        Err(Captured(f)) => f,
        Ok(_) => Vec::new(),
    }
}

/// Option keys the OpenDAL service behind `scheme` accepts, or `None` for a scheme oprobe does
/// not know the configuration of.
pub fn accepted_keys(scheme: &str) -> Option<BTreeSet<&'static str>> {
    use opendal::services::*;
    let fields = match scheme {
        "s3" => fields_of::<S3Config>(),
        "gcs" => fields_of::<GcsConfig>(),
        "azblob" => fields_of::<AzblobConfig>(),
        "oss" => fields_of::<OssConfig>(),
        "cos" => fields_of::<CosConfig>(),
        "b2" => fields_of::<B2Config>(),
        "webdav" => fields_of::<WebdavConfig>(),
        "http" => fields_of::<HttpConfig>(),
        "fs" => fields_of::<FsConfig>(),
        "memory" => fields_of::<MemoryConfig>(),
        "azdls" => fields_of::<AzdlsConfig>(),
        "azfile" => fields_of::<AzfileConfig>(),
        "obs" => fields_of::<ObsConfig>(),
        "swift" => fields_of::<SwiftConfig>(),
        "tos" => fields_of::<TosConfig>(),
        "webhdfs" => fields_of::<WebhdfsConfig>(),
        "upyun" => fields_of::<UpyunConfig>(),
        _ => return None,
    };
    Some(fields.into_iter().collect())
}

/// Options oprobe itself consumes (never passed to OpenDAL).
const OPROBE_KEYS: [&str; 1] = ["ca_cert_pem"];

/// The closest accepted key, if it is plausibly what was meant.
fn suggestion<'a>(key: &str, accepted: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    crate::input::suggest(key, accepted)
}

/// Reject option keys the service would silently ignore.
pub fn validate_keys<'a>(scheme: &str, keys: impl IntoIterator<Item = &'a String>) -> Result<()> {
    let Some(accepted) = accepted_keys(scheme) else {
        return Ok(());
    };
    for key in keys {
        if accepted.contains(key.as_str()) || OPROBE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let hint = match suggestion(key, accepted.iter().copied()) {
            Some(s) => format!("did you mean `{s}`? "),
            None => String::new(),
        };
        return Err(crate::errors::OprobeError::new(
            crate::errors::ErrorCode::ConfigUnknownOption,
            format!(
                "unknown option `{key}` for the `{scheme}` service (OpenDAL would silently ignore it)"
            ),
        )
        .with_hint(format!(
            "{hint}accepted options: {}",
            accepted.iter().copied().collect::<Vec<_>>().join(", ")
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_and_aliases_from_the_service_config() {
        let s3 = accepted_keys("s3").unwrap();
        for k in [
            "bucket",
            "region",
            "endpoint",
            "access_key_id",
            "secret_access_key",
            "disable_config_load",
            "enable_virtual_host_style",
            "allow_anonymous",
            "skip_signature",
            "root",
            // serde aliases are accepted too
            "aws_region",
            "aws_access_key_id",
        ] {
            assert!(s3.contains(k), "s3 lacks `{k}`: {s3:?}");
        }
        assert!(
            accepted_keys("gcs")
                .unwrap()
                .contains("disable_vm_metadata")
        );
        assert!(accepted_keys("azblob").unwrap().contains("account_name"));
        assert!(accepted_keys("fs").unwrap().contains("root"));
        assert!(accepted_keys("ftp").is_none());
    }

    #[test]
    fn unknown_keys_are_rejected_with_a_suggestion() {
        let keys = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        validate_keys("s3", &keys(&["bucket", "region", "ca_cert_pem"])).unwrap();
        let full = |e: anyhow::Error| {
            let b = crate::errors::classify(&e, "s3");
            assert_eq!(b.code, crate::errors::ErrorCode::ConfigUnknownOption);
            format!("{} {}", b.message, b.hint.unwrap_or_default())
        };
        let e = full(validate_keys("s3", &keys(&["bucket", "regin"])).unwrap_err());
        assert!(e.contains("unknown option `regin`"), "{e}");
        assert!(e.contains("did you mean `region`?"), "{e}");
        let e = full(validate_keys("s3", &keys(&["virtual_host_style"])).unwrap_err());
        assert!(
            e.contains("did you mean `enable_virtual_host_style`?"),
            "{e}"
        );
        let e = validate_keys("gcs", &keys(&["xyzzy"]))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("did you mean"), "{e}");
        // An S3 option on GCS is not silently ignored either.
        assert!(validate_keys("gcs", &keys(&["enable_virtual_host_style"])).is_err());
    }
}

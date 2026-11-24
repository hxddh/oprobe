//! The `OPERATOR` argument (design v6 §8): a URL, a path to an operator file (`op.toml`), or the
//! name of an operator in the config file (`[operators.NAME]`).

use super::*;
use crate::store::ConfigFile;

/// Where an operator came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum OperatorSource {
    Url(String),
    File(PathBuf),
    /// `[operators.NAME]` of the config file.
    Config(String),
}

impl OperatorSource {
    /// The configured operator's name (`None` for a URL or a file).
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Config(n) => Some(n.clone()),
            _ => None,
        }
    }
}

/// The operator file `arg` names when it exists on disk (`None` for a URL or a configured name).
pub fn operator_file_path(arg: &str) -> Option<PathBuf> {
    let arg = arg.trim();
    if arg.is_empty() || arg.contains("://") {
        return None;
    }
    let path = Path::new(arg);
    ((arg.ends_with(".toml") || path.is_file()) && path.is_file()).then(|| path.to_path_buf())
}

/// Resolve `arg` to an operator spec.
pub fn resolve(arg: &str, cfg: &ConfigFile) -> Result<(OperatorSpec, OperatorSource), OprobeError> {
    let arg = arg.trim();
    if arg.contains("://") {
        crate::input::storage_url(arg)?;
        // A pasted cloud address becomes the bucket's URL (`s3://…`), which the run shows.
        let mapped = crate::input::cloud_url(arg);
        let arg = mapped.as_deref().unwrap_or(arg);
        return Ok((
            OperatorSpec::from_url(arg),
            OperatorSource::Url(crate::util::redact_url(arg)),
        ));
    }
    let path = Path::new(arg);
    if arg.ends_with(".toml") || path.is_file() {
        if !path.exists() {
            return Err(
                invalid(format!("operator file `{arg}` does not exist")).with_hint(
                    "pass a URL (s3://bucket/prefix), an op.toml file or a configured name",
                ),
            );
        }
        return Ok((
            OperatorSpec::load(path)?,
            OperatorSource::File(path.to_path_buf()),
        ));
    }
    if let Some(op) = cfg.operator(arg) {
        return Ok((op.clone(), OperatorSource::Config(arg.to_owned())));
    }
    let known: Vec<&str> = cfg.operators.keys().map(String::as_str).collect();
    let hint = match crate::input::suggest(arg, known.iter().copied()) {
        Some(s) => format!("did you mean `{s}`?"),
        None if known.is_empty() => "pass a URL (s3://bucket/prefix, lab://demo), an op.toml file, or define [operators.NAME] in the config file".to_owned(),
        None => format!("configured operators: {}", known.join(", ")),
    };
    Err(OprobeError::new(
        ErrorCode::UsageUnknownTarget,
        format!("unknown operator `{arg}`"),
    )
    .with_hint(hint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_files_and_names_resolve() {
        let mut cfg = ConfigFile::default();
        let mut prod = OperatorSpec::from_url("s3://b/x");
        prod.layers.tail_cut = Some(TailCut::default());
        cfg.operators.insert("prod".into(), prod.clone());
        let (s, src) = resolve("memory://", &cfg).unwrap();
        assert_eq!(s, OperatorSpec::from_url("memory://"));
        assert_eq!(src, OperatorSource::Url("memory://".into()));
        // A pasted cloud address is the bucket's URL.
        let (s, src) = resolve("https://b.s3.amazonaws.com/x", &cfg).unwrap();
        assert_eq!(s, OperatorSpec::from_url("s3://b/x"));
        assert_eq!(src, OperatorSource::Url("s3://b/x".into()));
        let (s, src) = resolve("prod", &cfg).unwrap();
        assert_eq!(s, prod);
        assert_eq!(src, OperatorSource::Config("prod".into()));
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("op.toml");
        std::fs::write(&f, prod.to_toml()).unwrap();
        let (s, _) = resolve(f.to_str().unwrap(), &cfg).unwrap();
        assert_eq!(s, prod);
        assert_eq!(operator_file_path(f.to_str().unwrap()), Some(f.clone()));
        assert_eq!(operator_file_path("s3://b/x"), None);
        assert_eq!(operator_file_path("prod"), None);
        // Relative `http.ca_cert` resolves against the operator file's directory.
        let sub = d.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let ca = sub.join("ca.pem");
        std::fs::write(&ca, "pem").unwrap();
        let sub_op = sub.join("op.toml");
        std::fs::write(
            &sub_op,
            "url = \"s3://b/x\"\n[http]\nca_cert = \"ca.pem\"\n",
        )
        .unwrap();
        let loaded = OperatorSpec::load(&sub_op).unwrap();
        assert_eq!(
            loaded.http.ca_cert,
            Some(std::fs::canonicalize(&sub).unwrap().join("ca.pem"))
        );
        let e = resolve("prdo", &cfg).unwrap_err();
        assert_eq!(e.code, ErrorCode::UsageUnknownTarget);
        assert!(e.hint.unwrap().contains("prod"));
        assert!(resolve("missing.toml", &cfg).is_err());
    }
}

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

/// Resolve `arg` to an operator spec.
pub fn resolve(arg: &str, cfg: &ConfigFile) -> Result<(OperatorSpec, OperatorSource), OprobeError> {
    let arg = arg.trim();
    if arg.contains("://") {
        crate::input::storage_url(arg)?;
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
        let (s, src) = resolve("prod", &cfg).unwrap();
        assert_eq!(s, prod);
        assert_eq!(src, OperatorSource::Config("prod".into()));
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("op.toml");
        std::fs::write(&f, prod.to_toml()).unwrap();
        let (s, _) = resolve(f.to_str().unwrap(), &cfg).unwrap();
        assert_eq!(s, prod);
        let e = resolve("prdo", &cfg).unwrap_err();
        assert_eq!(e.code, ErrorCode::UsageUnknownTarget);
        assert!(e.hint.unwrap().contains("prod"));
        assert!(resolve("missing.toml", &cfg).is_err());
    }
}

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

/// The scheme an OPERATOR argument's errors are classified under (`s3`, `gcs`, `lab`, …):
/// the resolved operator's, else the argument read as a URL, else `s3`.
pub fn scheme_of(arg: &str, cfg: Option<&ConfigFile>) -> String {
    let url = cfg
        .and_then(|c| resolve(arg, c).ok())
        .map(|(spec, _)| spec.url)
        .unwrap_or_else(|| arg.to_owned());
    crate::config::TargetSpec::from_url(&url)
        .ok()
        .map(|t| t.scheme)
        .unwrap_or_else(|| "s3".into())
}

/// The operator a run's operator is saved from: `[operators.NAME]` of the config file when
/// the run named one (its `${ENV}` references and secrets as written), else the run's own,
/// with the run's layers, read, write and HTTP settings either way.
pub fn save_base(run: &OperatorSpec, operator_ref: &str, cfg: Option<&ConfigFile>) -> OperatorSpec {
    let mut base = cfg
        .and_then(|c| c.operators.get(operator_ref).cloned())
        .unwrap_or_else(|| run.clone());
    base.layers = run.layers.clone();
    base.read = run.read.clone();
    base.write = run.write.clone();
    base.http = run.http.clone();
    base
}

/// Space-separated OPERATOR arguments, `"…"` and `'…'` quotes keeping a path with spaces
/// whole (`\"` escapes a quote inside `"…"`).
pub fn split_operators(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(q) if c == '\\' && q == '"' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// An OPERATOR argument quoted for [`split_operators`] when it holds whitespace.
pub fn quote_operator_arg(s: &str) -> String {
    if s.chars().any(char::is_whitespace) && !s.starts_with('"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_owned()
    }
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
        // What a prove result stores: the path as the file gives it, not this machine's
        // absolute one (9.0 stored `/home/…/sub/ca.pem` in `.prove.json`).
        let given = loaded.as_given(sub_op.to_str().unwrap());
        assert_eq!(given.http.ca_cert, Some(PathBuf::from("ca.pem")));
        assert_eq!(loaded.as_given("s3://b/x"), loaded);
        assert_eq!(loaded.as_given("prod"), loaded);
        let e = resolve("prdo", &cfg).unwrap_err();
        assert_eq!(e.code, ErrorCode::UsageUnknownTarget);
        assert!(e.hint.unwrap().contains("prod"));
        assert!(resolve("missing.toml", &cfg).is_err());
    }

    #[test]
    fn split_operators_keeps_quoted_paths_with_spaces() {
        let raw = format!(
            "{} lab://demo '/another dir/b.toml'",
            quote_operator_arg("/Users/me/My Runs/op.toml")
        );
        assert_eq!(
            split_operators(&raw),
            vec![
                "/Users/me/My Runs/op.toml",
                "lab://demo",
                "/another dir/b.toml"
            ]
        );
        assert_eq!(
            scheme_of("memory://", None),
            crate::config::TargetSpec::from_url("memory://")
                .unwrap()
                .scheme
        );
        assert_eq!(scheme_of("not a url", None), "s3");
    }
}

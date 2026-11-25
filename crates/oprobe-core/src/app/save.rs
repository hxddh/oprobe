//! Saving an operator with edits (a proven change, the app's "Save" and "Save as…"): the one
//! entry point, [`App::save_operator`].
//!
//! The destination is one of:
//! - the `op.toml` of the same service: patched in place, keeping its comments, `${ENV}`
//!   references, secrets and relative paths;
//! - a new file: the operator written clean, with `${ENV}` references kept and secret values
//!   left out;
//! - `[operators.NAME]` of a config file: a bare name, or the config file itself for an
//!   operator that came from it.
//!
//! Anything else is refused before a byte is written: an existing file that is not an
//! `OperatorSpec` of the same service is never overwritten.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::App;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::spec::REDACTED;
use crate::operator::OperatorSpec;
use crate::store::ConfigFile;

/// Where [`App::save_operator`] writes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveTo {
    /// A file that does not exist yet.
    NewFile(PathBuf),
    /// The `op.toml` of the same service, patched in place.
    File(PathBuf),
    /// `[operators.NAME]` of the config file at `path`.
    Config { path: PathBuf, name: String },
}

/// What [`App::save_operator`] wrote.
#[derive(Clone, Debug)]
pub struct SavedOperator {
    /// Where, for people: the file, or `[operators.NAME] in FILE`.
    pub to: String,
    /// The operator file written (`None` for a config file's `[operators.NAME]`).
    pub file: Option<PathBuf>,
    /// The operator as saved.
    pub operator: OperatorSpec,
}

fn refuse(msg: String, hint: &str) -> anyhow::Error {
    OprobeError::new(ErrorCode::UsageInvalidArgument, msg)
        .with_hint(hint)
        .into()
}

/// The same file (both canonicalized when they exist).
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            // A file not created yet: compare its directory and its name.
            _ => {
                let dir = |p: &Path| {
                    p.parent()
                        .filter(|d| !d.as_os_str().is_empty())
                        .and_then(|d| std::fs::canonicalize(d).ok())
                };
                a.file_name() == b.file_name() && dir(a).is_some() && dir(a) == dir(b)
            }
        }
}

/// Whether the TOML `raw` is an oprobe config file (`[defaults]`, `[operators.NAME]`), not an
/// operator.
fn is_config_text(raw: &str) -> bool {
    raw.parse::<toml_edit::DocumentMut>().is_ok_and(|d| {
        !d.contains_key("url") && (d.contains_key("operators") || d.contains_key("defaults"))
    })
}

/// `PATH=VALUE` edits as pairs, a redacted value (`<redacted>`) left out: it is no value to
/// save.
fn edit_pairs(edits: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(edits.len());
    for e in edits {
        let (k, v) = crate::input::key_value("edit", e)?;
        if v.trim() != REDACTED {
            pairs.push((k, v));
        }
    }
    Ok(pairs)
}

/// `base` + `pairs` as a new operator: `${ENV}` references kept, secret values and URL
/// credentials left out (a new file is not where a secret should land).
fn clean_operator(base: &OperatorSpec, pairs: &[(String, String)]) -> Result<OperatorSpec> {
    let mut spec = base.redacted();
    spec.options.retain(|_, v| v.trim() != REDACTED);
    spec.apply_edits(pairs)?;
    spec.validate()?;
    Ok(spec)
}

fn with_file(e: OprobeError, path: &Path) -> OprobeError {
    let mut o = OprobeError::new(e.code, format!("`{}`: {}", path.display(), e.message));
    o.hint = e.hint;
    o
}

impl App {
    /// Where `dest` saves an operator of `base`'s service whose operator reference (a URL, an
    /// `op.toml` path or a configured name) is `operator_ref`. Writes nothing.
    fn save_target(&self, dest: &str, base: &OperatorSpec, operator_ref: &str) -> Result<SaveTo> {
        let dest = dest.trim();
        if dest.is_empty() {
            return Err(refuse(
                "empty --save target".into(),
                "pass an op.toml path or an operator name for config.toml",
            ));
        }
        let path = Path::new(dest);
        let looks_like_path = dest.ends_with(".toml")
            || dest.contains('/')
            || dest.contains(std::path::MAIN_SEPARATOR)
            || path.exists();
        // `[operators.NAME]` of the config file at `cfg_path`: NAME is the operator's own name
        // there, when it came from it.
        let config_entry = |cfg_path: &Path| -> Result<SaveTo> {
            let cfg = ConfigFile::load(cfg_path)?;
            if cfg.operators.contains_key(operator_ref.trim()) {
                Ok(SaveTo::Config {
                    path: cfg_path.to_path_buf(),
                    name: operator_ref.trim().to_owned(),
                })
            } else {
                Err(refuse(
                    format!(
                        "`{dest}` is a config file, and this operator is not one of its [operators.NAME]"
                    ),
                    "pass --save NAME to add [operators.NAME] to the config file, or a new op.toml path",
                ))
            }
        };
        if !looks_like_path {
            // A bare name: `[operators.NAME]` of the config file, when NAME is free or the
            // same service.
            let cfg = self.config()?;
            if let Some(existing) = cfg.operators.get(dest)
                && existing.display_url() != base.display_url()
            {
                return Err(refuse(
                    format!(
                        "[operators.{dest}] is the operator of `{}`, not of `{}`",
                        existing.display_url(),
                        base.display_url()
                    ),
                    "pass another name, or a new op.toml path",
                ));
            }
            return Ok(SaveTo::Config {
                path: self.paths.config_file.clone(),
                name: dest.to_owned(),
            });
        }
        if same_path(path, &self.paths.config_file) {
            return config_entry(&self.paths.config_file);
        }
        if !path.exists() {
            return Ok(SaveTo::NewFile(path.to_path_buf()));
        }
        if !path.is_file() {
            return Err(refuse(
                format!("`{dest}` is not a file"),
                "pass an op.toml path or an operator name for config.toml",
            ));
        }
        let raw = std::fs::read_to_string(path).map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("cannot read `{dest}`: {e}"),
            )
        })?;
        if is_config_text(&raw) {
            return config_entry(path);
        }
        let existing = OperatorSpec::parse(&raw).map_err(|e| {
            refuse(
                format!("`{dest}` exists and is not an operator file: {}", e.message),
                "save to a new file, the operator's own op.toml or a configured name",
            )
        })?;
        if existing.display_url() != base.display_url() {
            return Err(refuse(
                format!(
                    "`{dest}` is the operator of `{}`, not of `{}`",
                    existing.display_url(),
                    base.display_url()
                ),
                "save to a new file or to the op.toml of the same service",
            ));
        }
        Ok(SaveTo::File(path.to_path_buf()))
    }

    /// Check that [`App::save_operator`] would write to `dest` (before a prove's trials run).
    pub fn check_save(&self, dest: &str, base: &OperatorSpec, operator_ref: &str) -> Result<()> {
        self.save_target(dest, base, operator_ref).map(|_| ())
    }

    /// Save operator `base` (whose reference is `operator_ref`) with `PATH=VALUE` `edits` to
    /// `dest`: a new `.toml` file, the `op.toml` of the same service (patched in place), or
    /// `[operators.NAME]` of the config file (a bare `NAME`, or the config file's path for an
    /// operator that came from it). An existing file that is not the same service's operator
    /// is refused, not overwritten. The one save path of every front end.
    pub fn save_operator(
        &self,
        dest: &str,
        base: &OperatorSpec,
        operator_ref: &str,
        edits: &[String],
    ) -> Result<SavedOperator> {
        let pairs = edit_pairs(edits)?;
        match self.save_target(dest, base, operator_ref)? {
            SaveTo::NewFile(path) => {
                let spec = clean_operator(base, &pairs)?;
                crate::store::atomic_write(&path, spec.to_toml().as_bytes())?;
                Ok(SavedOperator {
                    to: path.display().to_string(),
                    file: Some(path),
                    operator: spec,
                })
            }
            SaveTo::File(path) => {
                let spec = patch_operator_file(&path, &pairs)?;
                Ok(SavedOperator {
                    to: path.display().to_string(),
                    file: Some(path),
                    operator: spec,
                })
            }
            SaveTo::Config { path, name } => {
                let spec = patch_config_operator(&path, &name, base, &pairs)?;
                if same_path(&path, &self.paths.config_file) {
                    self.invalidate_config();
                }
                Ok(SavedOperator {
                    to: format!("[operators.{name}] in {}", path.display()),
                    file: None,
                    operator: spec,
                })
            }
        }
    }
}

/// Apply `pairs` to the `op.toml` at `path` in place, preserving comments, whitespace, unedited
/// options, `${ENV}` references and relative paths.
fn patch_operator_file(path: &Path, pairs: &[(String, String)]) -> Result<OperatorSpec> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        OprobeError::new(
            ErrorCode::ConfigInvalid,
            format!("cannot read operator file `{}`: {e}", path.display()),
        )
    })?;
    OperatorSpec::parse(&raw).map_err(|e| with_file(e, path))?;
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| {
        OprobeError::new(
            ErrorCode::ConfigInvalid,
            format!("`{}`: {e}", path.display()),
        )
    })?;
    crate::operator::patch_toml_table(doc.as_table_mut(), pairs)?;
    let out = doc.to_string();
    let spec = OperatorSpec::parse(&out).map_err(|e| with_file(e, path))?;
    crate::store::atomic_write(path, out.as_bytes())?;
    Ok(spec)
}

/// Apply `pairs` to `[operators.NAME]` of the config file at `path` (created from `base` when
/// absent), preserving the file's comments and `[defaults]`. The whole file is validated before
/// it is written.
fn patch_config_operator(
    path: &Path,
    name: &str,
    base: &OperatorSpec,
    pairs: &[(String, String)],
) -> Result<OperatorSpec> {
    let invalid = |e: String| {
        OprobeError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid config file `{}`: {e}", path.display()),
        )
    };
    ConfigFile::load(path)?;
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| invalid(format!("{e}")))?;
    if !doc.contains_key("operators") || !doc["operators"].is_table() {
        let mut ops = toml_edit::Table::new();
        ops.set_implicit(true);
        doc["operators"] = toml_edit::Item::Table(ops);
    }
    let ops = doc["operators"].as_table_mut().expect("operators table");
    if let Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(it))) = ops.get(name) {
        let t = it.clone().into_table();
        ops[name] = toml_edit::Item::Table(t);
    }
    if let Some(toml_edit::Item::Table(t)) = ops.get_mut(name) {
        crate::operator::patch_toml_table(t, pairs)?;
    } else {
        let spec = clean_operator(base, pairs)?;
        let sub: toml_edit::DocumentMut = spec
            .to_toml()
            .parse()
            .expect("serialized OperatorSpec is valid TOML");
        ops[name] = toml_edit::Item::Table(sub.as_table().clone());
    }
    let out = doc.to_string();
    let validated: ConfigFile = toml::from_str(&out).map_err(|e| invalid(format!("{e}")))?;
    validated.defaults.validate()?;
    for (n, op) in &validated.operators {
        op.validate().map_err(|e| {
            let mut o = OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("operator `{n}` in `{}`: {}", path.display(), e.message),
            );
            o.hint = e.hint;
            o
        })?;
    }
    crate::store::atomic_write(path, out.as_bytes())?;
    Ok(validated.operators[name].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(dir: &Path) -> App {
        App::new(crate::store::Paths {
            config_file: dir.join("config.toml"),
            data_dir: dir.join("data"),
        })
    }

    #[test]
    fn the_same_services_op_toml_is_patched_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("op.toml");
        std::fs::write(
            &p,
            "# keep user header comment\nurl = \"s3://b/x\"\n[options]\n# keep secret placeholder\nregion = \"us-east-1\"\nsecret_access_key = \"${AWS_SECRET_ACCESS_KEY}\"\n[http]\nca_cert = \"ca.pem\"\n",
        )
        .unwrap();
        let app = app(dir.path());
        let base = OperatorSpec::from_url("s3://b/x");
        let saved = app
            .save_operator(
                p.to_str().unwrap(),
                &base,
                p.to_str().unwrap(),
                &[
                    "layers.concurrent_limit.http=24".to_owned(),
                    "layers.retry.jitter=on".to_owned(),
                    "options.secret_access_key=<redacted>".to_owned(),
                ],
            )
            .unwrap();
        assert_eq!(saved.operator.http_limit(), Some(24));
        assert_eq!(saved.file.as_deref(), Some(p.as_path()));
        let disk = std::fs::read_to_string(&p).unwrap();
        assert!(disk.contains("# keep user header comment"), "{disk}");
        assert!(disk.contains("# keep secret placeholder"), "{disk}");
        assert!(
            disk.contains("secret_access_key = \"${AWS_SECRET_ACCESS_KEY}\""),
            "{disk}"
        );
        assert!(disk.contains("ca_cert = \"ca.pem\""), "{disk}");
        assert!(disk.contains("http = 24"), "{disk}");
        assert!(disk.contains("jitter = true"), "{disk}");
    }

    #[test]
    fn a_name_saves_to_the_config_file_keeping_its_comments() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "# global defaults comment\n[defaults]\nduration = \"5s\"\n\n# prod operator comment\n[operators.prod]\nurl = \"s3://b/x\"\n[operators.prod.options]\nregion = \"us-east-1\"\nsecret_access_key = \"${AWS_SECRET}\"\n",
        )
        .unwrap();
        let app = app(dir.path());
        // Prime the cache before saving.
        assert_eq!(app.config().unwrap().operators["prod"].http_limit(), None);
        let base = OperatorSpec::from_url("s3://b/x");
        let saved = app
            .save_operator(
                "prod",
                &base,
                "prod",
                &["layers.concurrent_limit.http=32".to_owned()],
            )
            .unwrap();
        assert!(saved.to.starts_with("[operators.prod] in "), "{}", saved.to);
        // The cache was invalidated: the next `app.config()` reads the new limit.
        assert_eq!(
            app.config().unwrap().operators["prod"].http_limit(),
            Some(32)
        );
        let disk = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(disk.contains("# global defaults comment"), "{disk}");
        assert!(disk.contains("# prod operator comment"), "{disk}");
        assert!(
            disk.contains("secret_access_key = \"${AWS_SECRET}\""),
            "{disk}"
        );
        assert!(disk.contains("http = 32"), "{disk}");
    }

    /// D1: an existing file that is not an operator of the same service is never overwritten
    /// (9.0 rewrote any `.toml` path, `config.toml` included, as `base + edits`).
    #[test]
    fn a_file_that_is_not_the_same_services_operator_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        let base = OperatorSpec::from_url("s3://b/x");
        let edits = ["layers.concurrent_limit.http=8".to_owned()];
        let cases = [
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"1.0.0\"\n",
            ),
            ("other.toml", "url = \"s3://other/y\"\n"),
            ("notes.txt", "remember the milk\n"),
            ("broken.toml", "url = \n"),
        ];
        for (name, text) in cases {
            let p = dir.path().join(name);
            std::fs::write(&p, text).unwrap();
            let dest = p.to_str().unwrap();
            let e = app
                .save_operator(dest, &base, "s3://b/x", &edits)
                .unwrap_err();
            let b = crate::errors::classify(&e, "s3");
            assert_eq!(b.code, ErrorCode::UsageInvalidArgument, "{name}: {e:#}");
            assert!(b.hint.is_some(), "{name}");
            assert!(app.check_save(dest, &base, "s3://b/x").is_err(), "{name}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), text, "{name} changed");
        }
        // A configured name of another service is refused too.
        std::fs::write(
            dir.path().join("config.toml"),
            "[operators.prod]\nurl = \"s3://other/y\"\n",
        )
        .unwrap();
        assert!(
            app.save_operator("prod", &base, "s3://b/x", &edits)
                .is_err()
        );
    }

    /// D1: the config file goes to `[operators.NAME]` of the operator that came from it, and is
    /// refused for any other operator; its other entries and comments stay.
    #[test]
    fn the_config_file_is_saved_to_the_operators_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let text = "# mine\n[defaults]\nduration = \"5s\"\n[operators.prod]\nurl = \"s3://b/x\"\n[operators.dev]\nurl = \"memory://\"\n";
        std::fs::write(&cfg_path, text).unwrap();
        let app = app(dir.path());
        let base = OperatorSpec::from_url("s3://b/x");
        let edits = ["layers.concurrent_limit.http=8".to_owned()];
        let dest = cfg_path.to_str().unwrap();
        // A URL operator is no entry of the config file.
        assert!(app.check_save(dest, &base, "s3://b/x").is_err());
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), text);
        let saved = app.save_operator(dest, &base, "prod", &edits).unwrap();
        assert_eq!(saved.operator.http_limit(), Some(8));
        let cfg = app.config().unwrap();
        assert_eq!(cfg.operators["prod"].http_limit(), Some(8));
        assert_eq!(cfg.operators["dev"].url, "memory://");
        assert_eq!(cfg.defaults.duration.as_deref(), Some("5s"));
        assert!(
            std::fs::read_to_string(&cfg_path)
                .unwrap()
                .contains("# mine")
        );
        // Another config file, elsewhere, is recognised by its content.
        let other = dir.path().join("team.toml");
        std::fs::write(&other, "[operators.prod]\nurl = \"s3://b/x\"\n").unwrap();
        app.save_operator(other.to_str().unwrap(), &base, "prod", &edits)
            .unwrap();
        let t = std::fs::read_to_string(&other).unwrap();
        assert!(
            t.contains("[operators.prod.layers") || t.contains("http = 8"),
            "{t}"
        );
        assert!(!t.starts_with("url"), "{t}");
    }

    /// W14: a new file keeps `${ENV}` references and leaves secret values out (9.0's "Save
    /// as…" copied a configured operator's secrets into any file).
    #[test]
    fn a_new_file_keeps_env_references_not_secret_values() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        let mut base = OperatorSpec::from_url("s3://AKIDEXAMPLE:hunter2@b/x");
        base.options
            .insert("access_key_id".into(), "${MY_KEY_ID}".into());
        base.options
            .insert("secret_access_key".into(), "wJalrXUtnFEMI-secret".into());
        base.options.insert("region".into(), "us-east-1".into());
        let p = dir.path().join("new").join("op.toml");
        let saved = app
            .save_operator(
                p.to_str().unwrap(),
                &base,
                "prod",
                &["layers.retry.jitter=on".to_owned()],
            )
            .unwrap();
        let disk = std::fs::read_to_string(&p).unwrap();
        assert!(!disk.contains("wJalrXUtnFEMI-secret"), "{disk}");
        assert!(!disk.contains("hunter2"), "{disk}");
        assert!(!disk.contains(REDACTED), "{disk}");
        assert!(disk.contains("access_key_id = \"${MY_KEY_ID}\""), "{disk}");
        assert!(disk.contains("region = \"us-east-1\""), "{disk}");
        assert!(disk.contains("jitter = true"), "{disk}");
        assert_eq!(saved.file.as_deref(), Some(p.as_path()));
    }
}

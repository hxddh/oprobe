//! The Rust `to_rust()` generates, pinned for a matrix of operators.
//!
//! `tests/codegen/NAME.toml` is an operator and `tests/codegen/NAME.rs` is exactly what
//! `to_rust()` generates for it. This test keeps the two equal; `codegen_compiled.rs` compiles
//! every `NAME.rs` against the workspace's OpenDAL, foyer and reqwest, so generated code that
//! does not compile fails the build.
//!
//! To add an operator: write `NAME.toml`, regenerate with
//! `OPROBE_BLESS=1 cargo test -p oprobe-core --test codegen`, then list `NAME` in
//! `codegen_compiled.rs`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use oprobe_core::operator::diff::to_rust;
use oprobe_core::operator::{OperatorSpec, SET_PATHS};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/codegen")
}

fn bless() -> bool {
    std::env::var_os("OPROBE_BLESS").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Every `NAME.toml` of the matrix, by name.
fn operators() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| {
            (
                p.file_stem().unwrap().to_str().unwrap().to_owned(),
                std::fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn generated_rust_is_the_compiled_fixture() {
    let mut stale = Vec::new();
    for (name, toml) in operators() {
        let spec = OperatorSpec::parse(&toml).unwrap_or_else(|e| panic!("{name}.toml: {e}"));
        let generated = to_rust(&spec);
        let path = dir().join(format!("{name}.rs"));
        if std::fs::read_to_string(&path).ok().as_deref() == Some(generated.as_str()) {
            continue;
        }
        if bless() {
            std::fs::write(&path, generated).unwrap();
        } else {
            stale.push(name);
        }
    }
    assert!(
        stale.is_empty(),
        "tests/codegen/ is out of date: {stale:?}\nregenerate with `OPROBE_BLESS=1 cargo test -p oprobe-core --test codegen`"
    );
}

/// Every fixture is compiled: `codegen_compiled.rs` lists exactly the operators of the matrix,
/// and no `NAME.rs` is left without its `NAME.toml`.
#[test]
fn every_fixture_is_compiled() {
    let names: BTreeSet<String> = operators().into_iter().map(|(n, _)| n).collect();
    let compiled = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/codegen_compiled.rs"),
    )
    .unwrap();
    let list = compiled
        .split_once("fixtures! {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .expect("the fixtures! list")
        .0;
    let listed: BTreeSet<String> = list
        .split(',')
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    assert_eq!(
        listed, names,
        "codegen_compiled.rs must list every NAME.toml"
    );
    let sources: BTreeSet<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| p.file_stem().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        sources, names,
        "a NAME.rs without its NAME.toml (or the reverse)"
    );
}

/// The matrix sets every path `--set` accepts, so every option's code is compiled.
#[test]
fn the_matrix_covers_every_operator_path() {
    let docs: Vec<toml::Value> = operators()
        .iter()
        .map(|(_, t)| toml::from_str(t).unwrap())
        .collect();
    for path in SET_PATHS {
        let path = path.replace("options.KEY", "options");
        let set = docs
            .iter()
            .any(|doc| path.split('.').try_fold(doc, |v, key| v.get(key)).is_some());
        assert!(set, "no tests/codegen/*.toml sets `{path}`");
    }
}

/// Generated code never carries a secret, and never a redacted placeholder in a secret's
/// place: pasted, a placeholder would replace the credential the program loads itself.
#[test]
fn generated_rust_never_writes_a_secret() {
    let spec =
        OperatorSpec::parse(&std::fs::read_to_string(dir().join("secrets.toml")).unwrap()).unwrap();
    let rust = to_rust(&spec);
    for leaked in ["SEKRIT", "<redacted>", "user:"] {
        assert!(!rust.contains(leaked), "`{leaked}` in:\n{rust}");
    }
    // The literal secrets are left to the program; the `${NAME}` references are read from the
    // environment, alone or inside a value.
    assert!(
        rust.contains("// secret_access_key: a secret, not written here"),
        "{rust}"
    );
    assert!(
        rust.contains("// session_token: a secret, not written here"),
        "{rust}"
    );
    assert!(
        rust.contains(r#"("access_key_id".to_owned(), std::env::var("AWS_ACCESS_KEY_ID")?),"#),
        "{rust}"
    );
    assert!(
        rust.contains(
            r#"("endpoint".to_owned(), format!("https://{}:9000/{{tenant}}", std::env::var("S3_HOST")?)),"#
        ),
        "{rust}"
    );
    for (name, _) in operators() {
        let rust = std::fs::read_to_string(dir().join(format!("{name}.rs"))).unwrap();
        assert!(
            !rust.contains("<redacted>") && !rust.contains("SEKRIT"),
            "{name}.rs"
        );
    }
}
